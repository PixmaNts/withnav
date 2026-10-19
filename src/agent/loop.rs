use crate::agent::actor::{AgentActor, AgentState, PendingInteraction};
use crate::agent::authorize::{self, Authorized};
use crate::agent::context::Breakdown;
use crate::agent::permissions::{PermissionDecision, PermissionError, PermissionRequest, Scope};
use crate::agent::prompt::{assistant_history_message, build_request};
use crate::agent::response::{HumanRequest, StepOutcome};
use crate::error::{AgentError, ProviderError, ToolError};
use crate::permission::decisions::Outcome;
use crate::provider::client::ModelClient;
use crate::provider::openai::{ChatMessage, ClientCache};
use crate::provider::{AssistantMessage, StreamDelta, ToolCall};
use std::path::{Path, PathBuf};

use crate::tools::{ToolResult, ToolStatus};

/// Children retain their renewable step budget.
pub const DEFAULT_MAX_STEPS: usize = 50;
pub const WATCHDOG_FALLBACK_STEPS: usize = 500;

pub fn max_steps() -> usize {
    configured_max_steps(
        crate::paths::env_var("WITHNAV_MAX_STEPS"),
        DEFAULT_MAX_STEPS,
        DEFAULT_MAX_STEPS,
    )
}

/// Root turns have no step cap unless the Captain explicitly opts in.
pub fn root_max_steps() -> usize {
    configured_max_steps(
        crate::paths::env_var("WITHNAV_MAX_STEPS"),
        usize::MAX,
        WATCHDOG_FALLBACK_STEPS,
    )
}

fn configured_max_steps(
    value: Result<String, std::env::VarError>,
    absent: usize,
    invalid: usize,
) -> usize {
    if matches!(value, Err(std::env::VarError::NotPresent)) {
        return absent;
    }
    if let Some(steps) = value.ok().and_then(|value| value.parse::<usize>().ok()) {
        return steps;
    }
    tracing::warn!(
        fallback = invalid,
        "invalid WITHNAV_MAX_STEPS; expected a non-negative integer, using a bounded fallback"
    );
    invalid
}

#[derive(Debug, Clone, PartialEq)]
pub enum RunOutcome {
    Done(AssistantMessage),
    Yielded(HumanRequest),
    /// A gated tool call has not run: it waits for a decision, given to
    /// [`resolve_permission`]. The actor holds the same request in its
    /// pending interaction.
    PermissionRequired(PermissionRequest),
    Interrupted,
    WatchdogBoundary,
    StepBudgetExhausted,
    /// The next request would not fit the model's input budget. Internal, like
    /// `StepBudgetExhausted`: it never reaches the wire. The run stopped at a
    /// safe boundary (every tool result of the last step is in the
    /// transcript, nothing is pending) with `steps_left` model steps still
    /// to take. The caller compacts the conversation and carries the same
    /// turn on with [`continue_run`]; the budget is not renewed.
    NeedsCompaction {
        steps_left: usize,
    },
}

/// Pure observer events of a run, in the order the loop produces them. The
/// loop's control flow never branches on what the observer does.
///
/// `StepStarted` fires AFTER the per-iteration cancellation check,
/// immediately BEFORE request construction (a cancelled iteration never
/// mints a phantom step). `Delta` fires per streamed delta. `Response`
/// fires immediately AFTER the normalized model response is appended to the
/// transcript (the message already carries `usage` + `finish_reason`).
/// `PreCall` / `PostCall` fire immediately around each `registry.dispatch`.
/// `ContextEstimated` follows `Response` and the end of a step's tool calls.
pub enum AgentEvent<'a> {
    Delta(StreamDelta),
    StepStarted,
    Response(&'a AssistantMessage),
    /// The estimated size of the next request, right after the history
    /// changed: a response was appended, or a step's tool results were.
    ContextEstimated(Breakdown),
    PreCall {
        permission_gate: Option<&'a serde_json::Value>,
        tool_call_id: &'a str,
        name: &'a str,
        args: &'a serde_json::Value,
    },
    PostCall {
        tool_call_id: &'a str,
        name: &'a str,
        result: &'a ToolResult,
    },
}

pub async fn run_agent(
    actor: &mut AgentActor,
    observe: &mut (dyn for<'e> FnMut(AgentEvent<'e>) + Send),
) -> Result<RunOutcome, AgentError> {
    let budget = actor.watchdog_steps_left.take().unwrap_or(actor.max_steps);
    if !actor.skill_turn_open {
        actor.begin_skill_turn();
    }
    // A new turn: the loop count and the decision table's turn start over.
    actor.doom.new_turn();
    actor.permission_turn = ulid::Ulid::generate().to_string();
    run_steps(actor, observe, budget).await
}

/// Carries a turn on after a compaction: [`run_agent`] with the `steps_left`
/// a [`RunOutcome::NeedsCompaction`] reported, not a fresh budget.
pub async fn continue_run(
    actor: &mut AgentActor,
    observe: &mut (dyn for<'e> FnMut(AgentEvent<'e>) + Send),
    steps_left: usize,
) -> Result<RunOutcome, AgentError> {
    run_steps(actor, observe, steps_left).await
}

/// [`run_agent`] with at most `budget` more model steps. A run resumed after a
/// permission decision gets what is left of its turn, not a fresh budget.
async fn run_steps(
    actor: &mut AgentActor,
    observe: &mut (dyn for<'e> FnMut(AgentEvent<'e>) + Send),
    budget: usize,
) -> Result<RunOutcome, AgentError> {
    let mut state = RunState(actor, false);
    let outcome = run_steps_inner(&mut *state.0, observe, budget).await;
    state.1 = true;
    if matches!(
        &outcome,
        Err(_) | Ok(RunOutcome::Done(_) | RunOutcome::Interrupted | RunOutcome::WatchdogBoundary)
    ) {
        state.0.end_skill_turn();
    }
    outcome
}

/// An early error or dropped run cannot leave an inactive actor running.
struct RunState<'a>(&'a mut AgentActor, bool);

impl Drop for RunState<'_> {
    fn drop(&mut self) {
        if self.0.state == AgentState::Running {
            if !self.1 {
                self.0.end_skill_turn();
            }
            self.0.state = AgentState::Idle;
        }
    }
}

async fn run_steps_inner(
    actor: &mut AgentActor,
    observe: &mut (dyn for<'e> FnMut(AgentEvent<'e>) + Send),
    budget: usize,
) -> Result<RunOutcome, AgentError> {
    actor.state = AgentState::Running;
    while let Some((generation, id)) = actor.selected_skill_snapshots.first().cloned() {
        actor.selected_skill_snapshots.remove(0);
        if generation != actor.skill_turn_generation {
            continue;
        }
        let snapshot = actor
            .skills
            .validation
            .snapshots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id)
            .cloned();
        let Some(snapshot) = snapshot else {
            continue;
        };
        let call = ToolCall {
            id: ulid::Ulid::generate().to_string(),
            name: "skill".into(),
            args: serde_json::json!({"name":snapshot.name,"location":snapshot.location,"snapshot_id":id,"captain_selected":true}),
            args_valid: true,
        };
        actor.transcript.push(ChatMessage::Assistant {
            content: None,
            reasoning_content: None,
            tool_calls: vec![crate::provider::openai::RequestToolCall {
                id: call.id.clone(),
                kind: "function".into(),
                function: crate::provider::openai::RequestFunction {
                    name: call.name.clone(),
                    arguments: call.args.to_string(),
                },
            }],
        });
        match run_skill(actor, &call, observe, None, None).await {
            One::Ask(request, destination) => {
                actor.pending = Some(PendingInteraction::Permission {
                    request: request.clone(),
                    call,
                    remaining_calls: vec![],
                    destination,
                    steps_left: budget,
                });
                actor.state = AgentState::Yielded;
                return Ok(RunOutcome::PermissionRequired(*request));
            }
            One::Interrupted => return Ok(RunOutcome::Interrupted),
            One::Settled => {}
        }
    }

    // One client (one connection pool) for every step of this run.
    let client = ModelClient::new(
        "agent",
        &actor.provider,
        actor.api_key.clone(),
        &ClientCache::new(),
    )?;

    for step in 0..budget {
        if actor.cancel.is_cancelled() {
            actor.state = AgentState::Idle;
            return Ok(RunOutcome::Interrupted);
        }

        if !actor.has_pending()
            && let Some(boundary) = actor.safe_boundary.clone()
        {
            let updated = match boundary(&actor.transcript, actor.cancel.clone()).await {
                Ok(updated) => updated,
                Err(error) => {
                    if let Some(committed) = error.committed {
                        actor.transcript = committed;
                    }
                    return Err(AgentError::Tool(crate::error::ToolError::Denied {
                        code: "safe_boundary".into(),
                        message: error.message,
                    }));
                }
            };
            match updated {
                crate::agent::boundary::BoundaryResult::Unchanged => {}
                crate::agent::boundary::BoundaryResult::Continue(updated) => {
                    actor.transcript = updated
                }
                crate::agent::boundary::BoundaryResult::Interrupted => {
                    actor.state = AgentState::Idle;
                    return Ok(RunOutcome::Interrupted);
                }
                crate::agent::boundary::BoundaryResult::Watchdog(updated) => {
                    if let Some(updated) = updated {
                        actor.transcript = updated;
                    }
                    actor.watchdog_steps_left = Some(budget - step);
                    actor.state = AgentState::Idle;
                    return Ok(RunOutcome::WatchdogBoundary);
                }
            }
        }

        if actor.cancel.is_cancelled() {
            actor.state = AgentState::Idle;
            return Ok(RunOutcome::Interrupted);
        }

        // The safe boundary before a request: every tool result of the last
        // step is in, nothing waits for a human. A prompt over the input
        // budget is compacted by the caller before it is sent.
        let role_message = actor.role_message();
        let transcript_history = actor.effective_history();
        let image_plan = crate::agent::images::plan(&transcript_history, actor.images);
        let image_ids = image_plan.send_ids(&transcript_history);
        let mut history = actor.request_history(transcript_history.clone());
        let tools = actor.effective_tool_schemas();
        if actor.input_budget.is_some_and(|budget| {
            crate::agent::context::estimate(
                role_message.as_deref(),
                actor.rules.message(),
                actor.effective_skills_catalog(),
                &history,
                &tools,
            )
            .total()
                > budget
        }) {
            return Ok(RunOutcome::NeedsCompaction {
                steps_left: budget - step,
            });
        }

        // The bytes of the images this request sends, read after the estimate (which counts a
        // fixed cost per image and needs none). An image that cannot be read becomes a text
        // placeholder and the step goes on; the estimate only over-counted it.
        let mut image_payloads = std::collections::HashMap::new();
        if let Some(loader) = actor.attachment_loader.clone()
            && !image_ids.is_empty()
        {
            let mut loaded = loader.load(image_ids.clone()).await;
            let mut unavailable = std::collections::HashSet::new();
            for id in &image_ids {
                match loaded.remove(id) {
                    Some(Ok(payload)) => {
                        image_payloads.insert(id.clone(), payload);
                    }
                    Some(Err(reason)) => {
                        tracing::warn!(attachment = %id, %reason, "image unavailable for request");
                        unavailable.insert(id.clone());
                    }
                    None => {
                        unavailable.insert(id.clone());
                    }
                }
            }
            if !unavailable.is_empty() {
                history = image_plan.apply_excluding(&transcript_history, &unavailable);
            }
        }

        observe(AgentEvent::StepStarted);

        let mut request = build_request(
            &actor.model,
            role_message.as_deref(),
            actor.rules.message(),
            actor.effective_skills_catalog(),
            history,
            tools,
            actor.max_tokens,
        );
        request.images = image_payloads;

        let message = match client
            .chat(
                request,
                actor.variant.as_ref(),
                &actor.cancel,
                &mut |delta| observe(AgentEvent::Delta(delta)),
            )
            .await
        {
            Ok(m) => m,
            Err(ProviderError::Canceled { .. }) => {
                actor.state = AgentState::Idle;
                return Ok(RunOutcome::Interrupted);
            }
            Err(e) => return Err(AgentError::Provider(e)),
        };

        actor
            .transcript
            .push(assistant_history_message(&message, actor.reasoning_history));
        observe(AgentEvent::Response(&message));
        observe(AgentEvent::ContextEstimated(actor.context_breakdown()));

        let sibling_ids: Vec<String> = message.tool_calls.iter().map(|tc| tc.id.clone()).collect();

        match crate::agent::response::classify_with_ask(
            message,
            actor.tool_allowed(crate::agent::response::ASK_TOOL),
        ) {
            StepOutcome::Done { response } => {
                actor.state = AgentState::Idle;
                return Ok(RunOutcome::Done(response));
            }
            StepOutcome::Yield { request } => {
                for id in &sibling_ids {
                    if id != &request.tool_call_id {
                        append_interrupted(actor, id);
                    }
                }
                actor.pending = Some(PendingInteraction::Ask {
                    tool_call_id: request.tool_call_id.clone(),
                });
                actor.state = AgentState::Yielded;
                return Ok(RunOutcome::Yielded(request));
            }
            StepOutcome::Continue { tool_calls } => {
                match run_calls(actor, tool_calls, budget - step - 1, observe).await {
                    Calls::Finished => {}
                    Calls::Interrupted => return Ok(RunOutcome::Interrupted),
                    Calls::Permission(request) => {
                        return Ok(RunOutcome::PermissionRequired(*request));
                    }
                }
                if actor.cancel.is_cancelled() {
                    actor.state = AgentState::Idle;
                    return Ok(RunOutcome::Interrupted);
                }
            }
        }
    }

    if actor.cancel.is_cancelled() {
        actor.state = AgentState::Idle;
        return Ok(RunOutcome::Interrupted);
    }
    actor.state = AgentState::Yielded;
    Ok(RunOutcome::StepBudgetExhausted)
}

/// Why [`resolve_permission`] did not resume the turn.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    /// The decision matched nothing waiting: nothing was run or recorded.
    #[error(transparent)]
    Rejected(#[from] PermissionError),
    /// The turn resumed and then failed.
    #[error(transparent)]
    Agent(#[from] AgentError),
}

/// Applies a Captain's `decision` on the permission request `request_id`, then
/// carries the turn on, BEFORE any new model request:
///
/// * `AllowOnce`: the held call runs exactly once;
/// * `AllowAlways`: the proposed rules are saved in one transaction, then the
///   held call runs;
/// * `Reject`: nothing runs; the held call and every call behind it get a
///   `denied` result carrying the Captain's message.
///
/// After an allow, the calls that were behind it each go through the whole
/// decision again (any of them may ask: the outcome is then
/// `PermissionRequired`), and once all are settled the turn continues as
/// [`run_agent`] does. Legacy answers still apply: "allow folder" and "allow
/// configuration for the session" count as `AllowOnce`, `deny` as `Reject`.
///
/// `request_id` must be the id of the request the actor waits on; a stale or
/// repeated id is [`PermissionError::Stale`] and changes nothing.
pub async fn resolve_permission(
    actor: &mut AgentActor,
    request_id: ulid::Ulid,
    decision: PermissionDecision,
    observe: &mut (dyn for<'e> FnMut(AgentEvent<'e>) + Send),
) -> Result<RunOutcome, ResolveError> {
    resolve_permission_with_origin(
        actor,
        request_id,
        decision,
        super::permissions::PermissionOrigin::Captain,
        observe,
    )
    .await
}

/// Saves the rules of an "Always" answer to the pending request `request_id`
/// before its resolution is published, so that the event can say whether
/// they were saved. The outcome is kept on the request, and
/// [`resolve_permission_with_origin`] uses it instead of saving again.
/// Returns the error when the rules were not saved; `None` when they were, or
/// when the answer saves no rule (a skill validation, a stopped turn).
pub async fn save_pending_always(actor: &mut AgentActor, request_id: ulid::Ulid) -> Option<String> {
    if actor.cancel.is_cancelled() {
        return None;
    }
    let Some(PendingInteraction::Permission { request, .. }) = &actor.pending else {
        return None;
    };
    if request.id != request_id
        || request.always.is_empty()
        || (request.tool == "skill" && !request.skill_rule)
    {
        return None;
    }
    let always = request.always.clone();
    let saved = authorize::save_always(actor, &always).await;
    let error = saved.as_ref().err().cloned();
    if let Some(PendingInteraction::Permission { request, .. }) = &mut actor.pending {
        request.always_saved = Some(saved);
    }
    error
}

/// Whether bypass may answer the pending request: one raised by a rule `ask`
/// or the permission flow, never the fixed floor, a loop, or a skill validation.
pub fn pending_bypass_eligible(actor: &AgentActor) -> bool {
    matches!(
        &actor.pending,
        Some(PendingInteraction::Permission { request, .. })
            if !request.requires_captain && (request.tool != "skill" || request.skill_rule)
    )
}

pub async fn resolve_permission_with_origin(
    actor: &mut AgentActor,
    request_id: ulid::Ulid,
    decision: PermissionDecision,
    origin: super::permissions::PermissionOrigin,
    observe: &mut (dyn for<'e> FnMut(AgentEvent<'e>) + Send),
) -> Result<RunOutcome, ResolveError> {
    let decision = decision.current();
    if decision == PermissionDecision::AllowAlways
        && matches!(&actor.pending, Some(PendingInteraction::Permission { request, .. })
            if request.id == request_id && !request.reusable_approval)
    {
        return Err(PermissionError::Stale.into());
    }
    if matches!(&actor.pending, Some(PendingInteraction::Permission { request, .. }) if request.id == request_id && request.tool == "skill" && !request.skill_rule)
    {
        return resolve_skill(actor, decision, origin, observe).await;
    }
    let bypass = origin == super::permissions::PermissionOrigin::Bypass;
    match &actor.pending {
        Some(PendingInteraction::Permission { request, .. }) if request.id == request_id => {
            if (bypass && (request.requires_captain || decision != PermissionDecision::AllowOnce))
                || (decision == PermissionDecision::AllowAlways && request.always.is_empty())
                || decision == PermissionDecision::AllowSkillMachine
            {
                return Err(PermissionError::Stale.into());
            }
        }
        _ => return Err(PermissionError::Stale.into()),
    }
    let Some(PendingInteraction::Permission {
        mut request,
        call,
        remaining_calls,
        destination,
        steps_left,
    }) = actor.pending.take()
    else {
        return Err(PermissionError::Stale.into());
    };
    actor.state = AgentState::Running;
    let answer = serde_json::to_value(&decision).ok();

    if actor.cancel.is_cancelled() {
        if let Some(id) = request.decision_id {
            authorize::update(actor, id, Outcome::Cancelled, answer.as_ref(), None).await;
        }
        append_interrupted(actor, &call.id);
        for rest in &remaining_calls {
            append_interrupted(actor, &rest.id);
        }
        actor.state = AgentState::Idle;
        return Ok(RunOutcome::Interrupted);
    }
    let one = match decision {
        // Bypass decides the call again under its own control level, so a
        // deny rule written since the question still stops it.
        PermissionDecision::AllowOnce if bypass => {
            run_decided(actor, &call, observe, request.repeats, request.decision_id).await
        }
        PermissionDecision::AllowOnce | PermissionDecision::AllowAlways => {
            let mut answer = answer.unwrap_or_default();
            if decision == PermissionDecision::AllowAlways {
                let saved = match request.always_saved.take() {
                    Some(saved) => saved,
                    None => authorize::save_always(actor, &request.always).await,
                };
                match saved {
                    Ok(rules) => {
                        answer["always_saved"] = serde_json::json!(true);
                        answer["rule_ids"] = serde_json::json!(rules);
                    }
                    Err(error) => {
                        tracing::warn!(%error, "the Always rules were not saved; the call runs once");
                        answer["always_saved"] = serde_json::json!(false);
                        answer["always_error"] = serde_json::json!(error);
                    }
                }
            }
            if let Some(id) = request.decision_id {
                authorize::update(actor, id, Outcome::Allowed, Some(&answer), Some("captain"))
                    .await;
            }
            let note = authorize::answered_approval(&request, "captain");
            if call.name == "skill" {
                run_skill(actor, &call, observe, Some(note), None).await
            } else {
                run_approved(
                    actor,
                    &call,
                    destination.as_deref(),
                    note,
                    request.repeats,
                    observe,
                )
                .await
            }
        }
        PermissionDecision::Reject { message } => {
            if let Some(id) = request.decision_id {
                authorize::update(actor, id, Outcome::Denied, answer.as_ref(), None).await;
            }
            settle_rejected(actor, &call, &remaining_calls, message);
            return Ok(run_steps(actor, observe, steps_left).await?);
        }
        PermissionDecision::AllowFolder
        | PermissionDecision::AllowConfigSession
        | PermissionDecision::AllowSkillMachine
        | PermissionDecision::Deny { .. } => {
            unreachable!("legacy answers were mapped to the current ones")
        }
    };
    match one {
        One::Settled => {}
        One::Ask(fresh, destination) => {
            actor.pending = Some(PendingInteraction::Permission {
                request: fresh.clone(),
                call,
                remaining_calls,
                destination,
                steps_left,
            });
            actor.state = AgentState::Yielded;
            return Ok(RunOutcome::PermissionRequired(*fresh));
        }
        One::Interrupted => {
            append_interrupted(actor, &call.id);
            for rest in &remaining_calls {
                append_interrupted(actor, &rest.id);
            }
            actor.state = AgentState::Idle;
            return Ok(RunOutcome::Interrupted);
        }
    }
    match run_calls(actor, remaining_calls, steps_left, observe).await {
        Calls::Finished => {}
        Calls::Interrupted => return Ok(RunOutcome::Interrupted),
        Calls::Permission(request) => return Ok(RunOutcome::PermissionRequired(*request)),
    }
    Ok(run_steps(actor, observe, steps_left).await?)
}

/// Rejects the held call and every call held behind it with the Captain's words.
fn settle_rejected(
    actor: &mut AgentActor,
    call: &ToolCall,
    remaining_calls: &[ToolCall],
    message: Option<String>,
) {
    let message = message
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| "the Captain rejected this tool call".into());
    for held in std::iter::once(call).chain(remaining_calls) {
        let result = ToolResult::permission_denied(message.clone())
            .with_metadata(serde_json::json!({"approved_by":"denied","denied_by":"captain"}));
        append_tool_result(actor, &held.id, &result);
    }
}

/// The Captain's answer to a skill validation: only the Captain answers it.
async fn resolve_skill(
    actor: &mut AgentActor,
    decision: PermissionDecision,
    origin: super::permissions::PermissionOrigin,
    observe: &mut (dyn for<'e> FnMut(AgentEvent<'e>) + Send),
) -> Result<RunOutcome, ResolveError> {
    if origin != super::permissions::PermissionOrigin::Captain
        || !matches!(
            decision,
            PermissionDecision::AllowOnce
                | PermissionDecision::AllowAlways
                | PermissionDecision::Reject { .. }
        )
    {
        return Err(PermissionError::Stale.into());
    }
    let Some(PendingInteraction::Permission {
        call,
        remaining_calls,
        steps_left,
        ..
    }) = actor.pending.take()
    else {
        unreachable!()
    };
    let snapshot = call.args["snapshot_id"].as_str().and_then(|id| {
        actor
            .skills
            .validation
            .snapshots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()
    });
    if actor.cancel.is_cancelled() {
        actor.end_skill_turn();
        append_interrupted(actor, &call.id);
        for rest in &remaining_calls {
            append_interrupted(actor, &rest.id);
        }
        return Ok(RunOutcome::Interrupted);
    }
    if let PermissionDecision::Reject { message } = decision {
        // Not validated: nothing of the turn that waited on it runs.
        settle_rejected(actor, &call, &remaining_calls, message);
        if call.args["captain_selected"] == true {
            let name = call.args["name"].as_str().unwrap_or_default();
            actor.push_user_with_provenance(
                skill_host_block(
                    &call,
                    &ToolResult::error(
                        "skill_not_validated",
                        format!("skill {name} was not loaded"),
                    ),
                ),
                crate::provider::openai::UserProvenance::Navigator,
            );
        }
        return Ok(run_steps(actor, observe, steps_left).await?);
    }
    let mut gate = None;
    if matches!(
        decision,
        PermissionDecision::AllowOnce | PermissionDecision::AllowAlways
    ) && snapshot.is_some()
    {
        match skill_admission(actor, &call, observe, true, None).await {
            Admission::Run(note) => gate = Some(note),
            Admission::Done(One::Interrupted) => {
                actor.end_skill_turn();
                append_interrupted(actor, &call.id);
                for rest in &remaining_calls {
                    append_interrupted(actor, &rest.id);
                }
                return Ok(RunOutcome::Interrupted);
            }
            // Refused by a rule: the refusal is in the transcript.
            Admission::Done(_) => {
                return Ok(continue_after(actor, remaining_calls, steps_left, observe).await?);
            }
        }
    }
    let result = if matches!(
        decision,
        PermissionDecision::AllowOnce | PermissionDecision::AllowAlways
    ) {
        match snapshot {
            Some(snapshot) => {
                let validation = actor.skills.validation.clone();
                let always = decision == PermissionDecision::AllowAlways;
                let approved = tokio::task::spawn_blocking(move || {
                    validation
                        .approve(&snapshot, always)
                        .map(|_| snapshot.output)
                })
                .await;
                match approved {
                    Ok(Ok(output)) => output,
                    other => ToolResult::error(
                        "skill_not_validated",
                        format!("cannot validate skill: {other:?}"),
                    ),
                }
            }
            None => ToolResult::error("skill_not_validated", "skill snapshot expired"),
        }
    } else {
        let reason = match &decision {
            PermissionDecision::Reject { message } => message.as_deref(),
            _ => None,
        }
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("not validated");
        ToolResult::error(
            "skill_not_validated",
            format!(
                "skill {} was not loaded: {}",
                call.args["name"].as_str().unwrap_or("unknown"),
                reason
            ),
        )
    };
    let mut result = result;
    if let Some(note) = &gate {
        merge_approval_metadata(&mut result.metadata, note);
    }
    observe(AgentEvent::PreCall {
        permission_gate: gate.as_ref().and_then(|note| note.get("permission_gate")),
        tool_call_id: &call.id,
        name: &call.name,
        args: &call.args,
    });
    observe(AgentEvent::PostCall {
        tool_call_id: &call.id,
        name: &call.name,
        result: &result,
    });
    append_tool_result(actor, &call.id, &result);
    if call.args["captain_selected"] == true {
        actor.push_user_with_provenance(
            skill_host_block(&call, &result),
            crate::provider::openai::UserProvenance::Navigator,
        );
    }
    Ok(continue_after(actor, remaining_calls, steps_left, observe).await?)
}

/// Runs the calls still held, then carries the turn on.
async fn continue_after(
    actor: &mut AgentActor,
    remaining_calls: Vec<ToolCall>,
    steps_left: usize,
    observe: &mut (dyn for<'e> FnMut(AgentEvent<'e>) + Send),
) -> Result<RunOutcome, AgentError> {
    match run_calls(actor, remaining_calls, steps_left, observe).await {
        Calls::Permission(request) => return Ok(RunOutcome::PermissionRequired(*request)),
        Calls::Interrupted => return Ok(RunOutcome::Interrupted),
        Calls::Finished => {}
    }
    run_steps(actor, observe, steps_left).await
}

/// What the permission rules say about loading a validated skill.
enum Admission {
    /// Load it; the note goes on its tool events.
    Run(serde_json::Value),
    /// Refused, waiting for the Captain, or interrupted.
    Done(One),
}

/// Decides the loading of a skill whose content is validated, with action
/// `skill` and the skill's name: a `deny` rule refuses it whoever selected
/// it. `answered` is true right after the Captain validated it in this turn:
/// a rule `ask` is then already answered. `previous` is the pending decision
/// of a rule question that bypass answers: this decision settles it.
async fn skill_admission(
    actor: &mut AgentActor,
    call: &ToolCall,
    observe: &mut (dyn for<'e> FnMut(AgentEvent<'e>) + Send),
    answered: bool,
    previous: Option<i64>,
) -> Admission {
    match authorize::authorize(actor, call, 1, previous, true).await {
        Authorized::Run(note) => Admission::Run(note),
        Authorized::Denied { message, note } => {
            deny_call(actor, call, message, note, observe);
            Admission::Done(One::Settled)
        }
        Authorized::Ask(request) if answered => {
            if let Some(id) = request.decision_id {
                authorize::update(
                    actor,
                    id,
                    Outcome::Allowed,
                    Some(&serde_json::json!({"answer":"skill validation"})),
                    Some("captain"),
                )
                .await;
            }
            Admission::Run(authorize::answered_approval(&request, "captain"))
        }
        Authorized::Ask(mut request) => {
            // A rule question about a validated skill, not a validation:
            // bypass and "Always" follow the ordinary path.
            request.reason = format!("{} (skill {})", request.reason, call.args["name"]);
            request.skill_rule = true;
            Admission::Done(One::Ask(request, None))
        }
        Authorized::Interrupted => Admission::Done(One::Interrupted),
    }
}

/// How a pass over a message's tool calls ended.
enum Calls {
    Finished,
    Interrupted,
    /// The call at hand needs a decision; the actor holds it, and the calls
    /// behind it, as its pending interaction.
    Permission(Box<PermissionRequest>),
}

fn skill_host_block(call: &ToolCall, result: &ToolResult) -> String {
    let encoded = serde_json::json!({"name":call.args["name"],"selected_by":"captain","content":result.output})
        .to_string()
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026");
    format!("<captain_selected_skill encoding=\"json\">\n{encoded}\n</captain_selected_skill>")
}

/// Loads a skill: its content must be validated, then the permission rules
/// decide. `admitted` is the note of a rule question the Captain already answered.
async fn run_skill(
    actor: &mut AgentActor,
    call: &ToolCall,
    observe: &mut (dyn for<'e> FnMut(AgentEvent<'e>) + Send),
    admitted: Option<serde_json::Value>,
    previous: Option<i64>,
) -> One {
    if let Err(error) = preflight(actor, call) {
        reject_call(actor, call, error, observe);
        return One::Settled;
    }
    let snapshot = call.args["snapshot_id"].as_str().and_then(|id| {
        actor
            .skills
            .validation
            .snapshots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()
    });
    let Some(snapshot) = snapshot else {
        reject_call(actor, call, "skill snapshot unavailable".into(), observe);
        return One::Settled;
    };
    actor
        .skills
        .validation
        .turn_snapshots
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(snapshot.id.clone());
    let validation = actor.skills.validation.clone();
    let checked = snapshot.clone();
    let mut valid = if actor.yolo {
        Ok(Ok(true))
    } else {
        tokio::task::spawn_blocking(move || validation.validated(&checked)).await
    };
    let turn_approved = actor
        .skills
        .validation
        .approved
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&snapshot.name)
        .cloned()
        .and_then(|id| {
            actor
                .skills
                .validation
                .snapshots
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&id)
                .map(|s| s.hash == snapshot.hash && s.location == snapshot.location)
        })
        .unwrap_or(false);
    if turn_approved && matches!(valid, Ok(Ok(false))) {
        valid = Ok(Ok(true));
    }
    match valid {
        Ok(Ok(true)) => {
            // Validation proves which content the Captain trusted; the rules
            // still decide whether it may be loaded now.
            let approval = if actor.yolo {
                authorize::record_yolo(actor, call).await;
                yolo_approval()
            } else if let Some(note) = admitted {
                note
            } else {
                match skill_admission(actor, call, observe, false, previous).await {
                    Admission::Run(note) => note,
                    Admission::Done(one) => return one,
                }
            };
            if let Err(error) = actor.skills.validation.approve(&snapshot, false) {
                reject_call(actor, call, error, observe);
                return One::Settled;
            }
            let mut result = snapshot.output.clone();
            let approval = Some(approval);
            if let Some(note) = &approval {
                merge_approval_metadata(&mut result.metadata, note);
            }
            observe(AgentEvent::PreCall {
                permission_gate: approval.as_ref().and_then(|v| v.get("permission_gate")),
                tool_call_id: &call.id,
                name: &call.name,
                args: &call.args,
            });
            observe(AgentEvent::PostCall {
                tool_call_id: &call.id,
                name: &call.name,
                result: &result,
            });
            append_tool_result(actor, &call.id, &result);
            if call.args["captain_selected"] == true {
                actor.push_user_with_provenance(
                    skill_host_block(call, &snapshot.output),
                    crate::provider::openai::UserProvenance::Navigator,
                );
            }
            One::Settled
        }
        Ok(Ok(false)) => {
            let mut request = PermissionRequest::new(
                call,
                Scope::Folder(PathBuf::from(&snapshot.location)),
                actor
                    .role
                    .as_ref()
                    .map(|r| r.id.as_str())
                    .unwrap_or("build"),
            );
            request.requires_captain = true;
            request.reusable_approval = actor.permission_store.is_some();
            request.reason = format!(
                "Validate skill {} at {} for this version",
                snapshot.name, snapshot.location
            );
            One::Ask(Box::new(request), None)
        }
        other => {
            reject_call(
                actor,
                call,
                format!("cannot inspect skill validation: {other:?}"),
                observe,
            );
            One::Settled
        }
    }
}

/// Runs `calls` in order. A call that needs approval and has none stops the
/// pass BEFORE it executes: it and the calls behind it are kept in the
/// actor's pending interaction, with the `steps_left` of the turn.
async fn run_calls(
    actor: &mut AgentActor,
    calls: Vec<ToolCall>,
    steps_left: usize,
    observe: &mut (dyn for<'e> FnMut(AgentEvent<'e>) + Send),
) -> Calls {
    let mut calls = std::collections::VecDeque::from(calls);
    while let Some(mut call) = calls.pop_front() {
        if call.name == "skill" {
            // Provider arguments never choose host snapshots or claim human selection.
            if call.args.get("snapshot_id").is_some() || call.args.get("captain_selected").is_some()
            {
                reject_call(actor, &call, "reserved skill argument".into(), observe);
                continue;
            }
            let skills = actor.skills.clone();
            let name = call.args["name"].as_str().unwrap_or("").to_owned();
            let snapshot = tokio::task::spawn_blocking(move || skills.snapshot(&name))
                .await
                .unwrap_or_else(|e| Err(e.to_string()));
            match snapshot {
                Ok(snapshot) => {
                    call.args["snapshot_id"] = serde_json::json!(snapshot.id);
                    call.args["location"] = serde_json::json!(snapshot.location);
                }
                Err(error) => {
                    reject_call(actor, &call, error, observe);
                    continue;
                }
            }
        }
        if actor.cancel.is_cancelled() {
            append_interrupted(actor, &call.id);
            for rest in &calls {
                append_interrupted(actor, &rest.id);
            }
            actor.state = AgentState::Idle;
            return Calls::Interrupted;
        }
        let repeats = actor.doom.observe(&call);
        match run_decided(actor, &call, observe, repeats, None).await {
            One::Settled => {}
            One::Ask(request, destination) => {
                actor.pending = Some(PendingInteraction::Permission {
                    request: request.clone(),
                    call,
                    remaining_calls: calls.into(),
                    destination,
                    steps_left,
                });
                actor.state = AgentState::Yielded;
                return Calls::Permission(request);
            }
            One::Interrupted => {
                append_interrupted(actor, &call.id);
                for rest in &calls {
                    append_interrupted(actor, &rest.id);
                }
                actor.state = AgentState::Idle;
                return Calls::Interrupted;
            }
        }
    }
    observe(AgentEvent::ContextEstimated(actor.context_breakdown()));
    Calls::Finished
}

fn preflight(actor: &AgentActor, call: &ToolCall) -> Result<(), String> {
    if let Err(error) = crate::tools::reject_reserved_args(&call.args) {
        return Err(error.to_string());
    }
    if !call.args_valid || !call.args.is_object() {
        return Err("invalid tool arguments".into());
    }
    if !actor.tool_allowed(&call.name) {
        return Err(format!(
            "tool '{}' is disabled by this session's policy",
            call.name
        ));
    }
    let schema = actor
        .registry
        .get(&call.name)
        .expect("allowed tool")
        .schema();
    let parameters = &schema["parameters"];
    if let Some(required) = parameters["required"].as_array() {
        for name in required.iter().filter_map(serde_json::Value::as_str) {
            let kind = parameters["properties"][name]["type"].as_str();
            let valid = call.args.get(name).is_some_and(|value| match kind {
                Some("string") => value.is_string(),
                Some("boolean") => value.is_boolean(),
                Some("integer") => value.is_i64() || value.is_u64(),
                Some("number") => value.is_number(),
                Some("array") => value.is_array(),
                Some("object") => value.is_object(),
                Some("null") => value.is_null(),
                _ => true,
            });
            if !valid {
                return Err(format!(
                    "missing {} argument: {name}",
                    kind.unwrap_or("required")
                ));
            }
        }
    }
    // Role-based authorization, not argument validation: yolo does not check it.
    if let Some(role) = actor.role.as_deref().filter(|_| !actor.yolo) {
        role.validate_write_scope(&call.name, &call.args, &actor.tool_context.workspace)?;
    }
    Ok(())
}

/// How one call came out of the gate and, when it was let through, its run.
enum One {
    /// Run, or refused: its result is in the transcript.
    Settled,
    /// The captain decides; the path is the canonical file the request is for.
    Ask(Box<PermissionRequest>, Option<PathBuf>),
    /// The session was stopped while the call was being checked.
    Interrupted,
}

/// How many times a call whose file moved under it is checked afresh before
/// it is refused: a file that keeps changing is not one to write.
const REGATE_LIMIT: usize = 3;

/// Takes `call` through the one permission decision and runs it when it is
/// allowed. Tool availability and the call's arguments are checked first; a
/// skill goes through its validation instead; yolo checks nothing.
/// `previous` is the pending decision a bypass answer settles.
async fn run_decided(
    actor: &mut AgentActor,
    call: &ToolCall,
    observe: &mut (dyn for<'e> FnMut(AgentEvent<'e>) + Send),
    repeats: usize,
    previous: Option<i64>,
) -> One {
    if call.name == "skill" {
        let one = run_skill(actor, call, observe, None, previous).await;
        // A skill refused before its admission (snapshot gone, validation
        // revoked) never settles the question bypass answered: end it here.
        if let (Some(id), Some(store)) = (previous, &actor.permission_store)
            && let Err(error) = store.run(move |store| store.cancel_if_pending(id)).await
        {
            tracing::warn!(%error, "permission decision outcome not recorded");
        }
        return one;
    }
    if let Err(message) = preflight(actor, call) {
        reject_call(actor, call, message, observe);
        return One::Settled;
    }
    if actor.yolo {
        authorize::record_yolo(actor, call).await;
        return run_yolo(actor, call, observe).await;
    }
    // WithNav's own conversation controls act on this session and its
    // children, never on files, commands or the network: no rule covers them.
    // Each child's own calls go through the decision.
    if actor.permission_store.is_some()
        && matches!(
            call.name.as_str(),
            "task" | "steer_subagent" | "decide_subagent" | "intent"
        )
    {
        return execute_and_settle(actor, call, None, None, observe)
            .await
            .unwrap_or(One::Settled);
    }
    let mut previous = previous;
    for _ in 0..REGATE_LIMIT {
        let note = match authorize::authorize(actor, call, repeats, previous.take(), false).await {
            Authorized::Run(note) => note,
            Authorized::Denied { message, note } => {
                deny_call(actor, call, message, note, observe);
                return One::Settled;
            }
            Authorized::Ask(mut request) => {
                request.repeats = repeats;
                // The file the Captain is shown: an answer runs the call only
                // if it still lands there.
                return One::Ask(request, bind_destination(actor, call));
            }
            Authorized::Interrupted => return One::Interrupted,
        };
        if actor.cancel.is_cancelled() {
            return One::Interrupted;
        }
        let destination = bind_destination(actor, call);
        if let Some(outcome) =
            execute_and_settle(actor, call, destination.as_deref(), Some(note), observe).await
        {
            return outcome;
        }
    }
    reject_call(
        actor,
        call,
        "the target changed while the call was being checked; nothing was changed".to_string(),
        observe,
    );
    One::Settled
}

/// Runs a call the Captain allowed. A target that moved between the answer
/// and the dispatch goes through the decision again.
async fn run_approved(
    actor: &mut AgentActor,
    call: &ToolCall,
    destination: Option<&Path>,
    note: serde_json::Value,
    repeats: usize,
    observe: &mut (dyn for<'e> FnMut(AgentEvent<'e>) + Send),
) -> One {
    if let Err(message) = preflight(actor, call) {
        reject_call(actor, call, message, observe);
        return One::Settled;
    }
    if let Some(outcome) = execute_and_settle(actor, call, destination, Some(note), observe).await {
        return outcome;
    }
    run_decided(actor, call, observe, repeats, None).await
}

/// The canonical file a file tool is bound to once it is allowed, so it opens
/// exactly that file without following a link and without the tool's own
/// root checks, which the decision replaced.
fn bind_destination(actor: &AgentActor, call: &ToolCall) -> Option<PathBuf> {
    match call.name.as_str() {
        "read" | "write" | "edit" => call
            .args
            .get("path")
            .and_then(serde_json::Value::as_str)
            .and_then(|path| {
                super::permissions::resolve_read_target(Path::new(path), &actor.tool_context).ok()
            })
            .filter(|path| !path.is_dir()),
        "grep" | "glob" => {
            let path = call
                .args
                .get("path")
                .and_then(serde_json::Value::as_str)
                .map(Path::new)
                .unwrap_or(&actor.tool_context.workspace);
            super::permissions::resolve_read_target(path, &actor.tool_context).ok()
        }
        _ => None,
    }
}

/// Settles a call a `deny` rule or the permission flow refused.
fn deny_call(
    actor: &mut AgentActor,
    call: &ToolCall,
    message: String,
    note: serde_json::Value,
    observe: &mut (dyn for<'e> FnMut(AgentEvent<'e>) + Send),
) {
    observe(AgentEvent::PreCall {
        permission_gate: note.get("permission_gate"),
        tool_call_id: &call.id,
        name: &call.name,
        args: &call.args,
    });
    let mut result = ToolResult::permission_denied(message);
    merge_approval_metadata(&mut result.metadata, &note);
    observe(AgentEvent::PostCall {
        tool_call_id: &call.id,
        name: &call.name,
        result: &result,
    });
    append_tool_result(actor, &call.id, &result);
}

/// The evidence a yolo call carries: nothing was checked, and it says so.
fn yolo_approval() -> serde_json::Value {
    serde_json::json!({
        "approved_by": "yolo",
        "permission_gate": {
            "policy_version": 2,
            "source": "yolo",
            "approved_by": "yolo",
            "reason": "yolo: permission control is off",
            "truncated": false
        }
    })
}

/// The Captain turned permission control off (`yolo`): the call is dispatched
/// without rules, flows or the fixed floor. What stays
/// is what is not an approval: the session's tool availability and the call's
/// own arguments (`preflight`: malformed or missing arguments), and
/// cancellation.
async fn run_yolo(
    actor: &mut AgentActor,
    call: &ToolCall,
    observe: &mut (dyn for<'e> FnMut(AgentEvent<'e>) + Send),
) -> One {
    if call.name == "skill" {
        return run_skill(actor, call, observe, None, None).await;
    }
    if let Err(message) = preflight(actor, call) {
        reject_call(actor, call, message, observe);
        return One::Settled;
    }
    for _ in 0..REGATE_LIMIT {
        if actor.cancel.is_cancelled() {
            return One::Interrupted;
        }
        // A file tool is bound to the canonical file it names, as after a
        // Captain's approval, so it opens it without following a link.
        let destination = match call.name.as_str() {
            "read" | "write" | "edit" => call
                .args
                .get("path")
                .and_then(serde_json::Value::as_str)
                .and_then(|path| {
                    super::permissions::resolve_read_target(Path::new(path), &actor.tool_context)
                        .ok()
                })
                .filter(|path| !path.is_dir()),
            // Searches are bound too, so the host tells the tool the root is trusted.
            "grep" | "glob" => {
                let path = call
                    .args
                    .get("path")
                    .and_then(serde_json::Value::as_str)
                    .map(Path::new)
                    .unwrap_or(&actor.tool_context.workspace);
                super::permissions::resolve_read_target(path, &actor.tool_context).ok()
            }
            _ => None,
        };
        if let Some(outcome) = execute_and_settle(
            actor,
            call,
            destination.as_deref(),
            Some(yolo_approval()),
            observe,
        )
        .await
        {
            return outcome;
        }
    }
    reject_call(
        actor,
        call,
        "the target changed while the call was being checked; nothing was changed".to_string(),
        observe,
    );
    One::Settled
}

/// A changed destination remains the caller's responsibility: retry the gate
/// or ask for a fresh flow binding, without reusing the old approval.
async fn execute_and_settle(
    actor: &mut AgentActor,
    call: &ToolCall,
    destination: Option<&Path>,
    approval_note: Option<serde_json::Value>,
    observe: &mut (dyn for<'e> FnMut(AgentEvent<'e>) + Send),
) -> Option<One> {
    match execute_call(actor, call, destination, approval_note, observe).await {
        DispatchOutcome::Settled(result) => {
            append_tool_result(actor, &call.id, &result);
            Some(One::Settled)
        }
        DispatchOutcome::DestinationChanged => None,
    }
}

/// Settles a call the gate refused: it is never dispatched, and the model
/// reads the tool error.
fn reject_call(
    actor: &mut AgentActor,
    call: &ToolCall,
    message: String,
    observe: &mut (dyn for<'e> FnMut(AgentEvent<'e>) + Send),
) {
    // In yolo the refusal carries the same evidence as a run: nothing was asked.
    let approval = actor.yolo.then(yolo_approval);
    observe(AgentEvent::PreCall {
        permission_gate: approval
            .as_ref()
            .and_then(|note| note.get("permission_gate")),
        tool_call_id: &call.id,
        name: &call.name,
        args: &call.args,
    });
    let mut result = ToolResult::error("tool_error", message);
    if let Some(note) = &approval {
        merge_approval_metadata(&mut result.metadata, note);
    }
    observe(AgentEvent::PostCall {
        tool_call_id: &call.id,
        name: &call.name,
        result: &result,
    });
    append_tool_result(actor, &call.id, &result);
}

/// Dispatches `call`. With a `destination` (the canonical file the decision
/// was made for) the tool opens exactly that file without following links;
/// one replaced by a link or another kind of file is `DestinationChanged`,
/// and the call is decided again.
enum DispatchOutcome {
    Settled(ToolResult),
    DestinationChanged,
}

async fn execute_call(
    actor: &AgentActor,
    call: &ToolCall,
    destination: Option<&Path>,
    note: Option<serde_json::Value>,
    observe: &mut (dyn for<'e> FnMut(AgentEvent<'e>) + Send),
) -> DispatchOutcome {
    // The seam tests use to move a file between the check and the dispatch.
    #[cfg(test)]
    tests::before_dispatch();
    if let Some(role) = actor.role.as_deref().filter(|_| !actor.yolo)
        && let Err(message) =
            role.validate_write_scope(&call.name, &call.args, &actor.tool_context.workspace)
    {
        return DispatchOutcome::Settled(ToolResult::error("agent_write_scope", message));
    }
    if actor.cancel.is_cancelled() {
        return DispatchOutcome::Settled(ToolResult::not_started());
    }
    observe(AgentEvent::PreCall {
        permission_gate: note.as_ref().and_then(|note| note.get("permission_gate")),
        tool_call_id: &call.id,
        name: &call.name,
        args: &call.args,
    });
    let args = call.args.clone();
    let mut cx = actor.tool_context.clone();
    if actor.yolo {
        // No root confinement either: yolo is the Captain's word for the whole machine.
        cx.writable_roots = vec![PathBuf::from("/")];
    }
    // An allowed search may look where it was allowed, outside the project too.
    if let Some(root) = destination.filter(|_| matches!(call.name.as_str(), "grep" | "glob")) {
        cx.readable_roots.push(root.to_path_buf());
    }
    cx.binding = destination.map(|destination| crate::tools::DispatchBinding {
        destination: destination.to_path_buf(),
        // A scratch identity is a permission control (inode and mode): not in yolo.
        scratch: if actor.yolo {
            None
        } else {
            actor
                .scratch_roots
                .iter()
                .find(|root| destination.starts_with(&root.path))
                .cloned()
        },
        yolo: actor.yolo,
    });
    let dispatched = actor
        .registry
        .dispatch(&call.name, args, actor.cancel.child_token(), &cx)
        .await;
    let mut result = match dispatched {
        // The tool opened the reviewed file without following links and found
        // one (or another kind of file) there: nothing was touched, and the
        // call is checked again. The attempt stays visible, closed as an error.
        Err(ToolError::DestinationChanged(path)) => {
            let mut result = ToolResult::error(
                "destination_changed",
                format!(
                    "{} changed after it was checked; nothing was written, the call is checked again",
                    path.display()
                ),
            );
            if let Some(note) = &note {
                merge_approval_metadata(&mut result.metadata, note);
            }
            observe(AgentEvent::PostCall {
                tool_call_id: &call.id,
                name: &call.name,
                result: &result,
            });
            return DispatchOutcome::DestinationChanged;
        }
        Ok(result) => result,
        Err(e) => ToolResult::error("tool_error", e.to_string()),
    };
    // How the call was allowed: the durable `tool.post_call` says so.
    if let Some(note) = note {
        merge_approval_metadata(&mut result.metadata, &note);
    }
    observe(AgentEvent::PostCall {
        tool_call_id: &call.id,
        name: &call.name,
        result: &result,
    });
    DispatchOutcome::Settled(result)
}

fn merge_approval_metadata(metadata: &mut serde_json::Value, note: &serde_json::Value) {
    if !metadata.is_object() {
        *metadata = serde_json::json!({});
    }
    for key in [
        "approved_by",
        "denied_by",
        "permission_gate",
        "guard",
        "flow",
    ] {
        if let Some(value) = note.get(key) {
            metadata[key] = value.clone();
        }
    }
    if note.get("permission_gate").is_none()
        && note.get("guard").is_none()
        && note.get("flow").is_none()
    {
        metadata["guard"] = note.clone();
    }
}

/// What the model reads for a call cancelled before it started: an empty
/// result would read as a success with no output.
pub(crate) const NOT_RUN: &str = "interrupted: the call did not run";

/// What the model reads for an interrupted call.
fn interrupted_content(result: &ToolResult) -> String {
    if result.never_started() {
        NOT_RUN.to_owned()
    } else if result.output.trim().is_empty() {
        "interrupted while running; partial output: (none)".to_owned()
    } else {
        format!(
            "interrupted while running; partial output:\n{}",
            result.output
        )
    }
}

fn append_tool_result(actor: &mut AgentActor, tool_call_id: &str, result: &ToolResult) {
    let content = match result.status {
        ToolStatus::Ok => result.output.clone(),
        ToolStatus::Interrupted => interrupted_content(result),
        _ => serde_json::to_string(result).unwrap_or_else(|_| result.output.clone()),
    };
    actor.transcript.push(ChatMessage::Tool {
        tool_call_id: tool_call_id.to_string(),
        content,
        answered_by: None,
    });
}

/// Settles a call that never started.
fn append_interrupted(actor: &mut AgentActor, tool_call_id: &str) {
    append_tool_result(actor, tool_call_id, &ToolResult::not_started());
}

#[cfg(test)]
#[path = "loop_permission_tests.rs"]
mod permission_tests;

#[cfg(test)]
#[path = "loop_image_tests.rs"]
mod image_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::actor::AgentActor;
    #[cfg(unix)]
    use crate::agent::permissions::Scope;
    use crate::provider::config::{ProviderConfig, ReasoningHistory};
    use crate::provider::test_support::{http_sse, sse_body};

    #[tokio::test]
    async fn typed_watchdog_boundary_preserves_transcript_and_steps_at_every_position() {
        for before_first in [true, false] {
            let call = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"g","function":{"name":"glob","arguments":"{\"pattern\":\"*\"}"}}]},"finish_reason":"tool_calls"}]}"#;
            let server = spawn_mock(vec![http_sse(&sse_body(&[call]))]).await;
            let mut actor = make_actor(server.url, registry_all(), CancellationToken::new());
            actor.max_steps = 7;
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            actor.safe_boundary = Some(Arc::new(move |transcript, _| {
                let step = calls.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                Box::pin(async move {
                    if !before_first && step == 0 {
                        return Ok(crate::agent::boundary::BoundaryResult::Unchanged);
                    }
                    let mut transcript = transcript.to_vec();
                    transcript.push(ChatMessage::User {
                        provenance: crate::provider::openai::UserProvenance::Parent,
                        content: "committed steering".into(),
                        images: Vec::new(),
                    });
                    Ok(crate::agent::boundary::BoundaryResult::Watchdog(Some(
                        transcript,
                    )))
                })
            }));
            assert_eq!(
                run_agent(&mut actor, &mut |_| {}).await.unwrap(),
                RunOutcome::WatchdogBoundary
            );
            assert_eq!(
                actor.watchdog_steps_left,
                Some(if before_first { 7 } else { 6 })
            );
            assert!(
                matches!(actor.transcript.last(),Some(ChatMessage::User {content,..}) if content == "committed steering")
            );
        }
    }

    #[tokio::test]
    async fn borrowed_boundaries_keep_original_content_on_no_change_interrupt_error_and_drop() {
        use crate::agent::boundary::BoundaryResult;
        for mode in ["unchanged", "interrupt", "watchdog", "error", "drop"] {
            let server = spawn_mock(vec![http_sse(&sse_body(&[&text_chunk("done")]))]).await;
            let mut actor = make_actor(server.url, registry_all(), CancellationToken::new());
            actor.push_user("large original input".repeat(4096));
            let messages = actor.transcript.as_ptr() as usize;
            let ChatMessage::User { content, .. } = &actor.transcript[0] else {
                panic!("expected user message")
            };
            let contents = content.as_ptr() as usize;
            actor.safe_boundary = Some(Arc::new(move |transcript, _| {
                assert_eq!(transcript.as_ptr() as usize, messages);
                Box::pin(async move {
                    let ChatMessage::User { content, .. } = &transcript[0] else {
                        panic!("expected user message")
                    };
                    assert_eq!(content.as_ptr() as usize, contents);
                    tokio::task::yield_now().await;
                    match mode {
                        "unchanged" => Ok(BoundaryResult::Unchanged),
                        "interrupt" => Ok(BoundaryResult::Interrupted),
                        "watchdog" => Ok(BoundaryResult::Watchdog(None)),
                        "error" => Err("uncommitted failure".to_string().into()),
                        _ => std::future::pending().await,
                    }
                })
            }));
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(1),
                run_agent(&mut actor, &mut |_| {}),
            )
            .await;
            match mode {
                "unchanged" => assert!(matches!(result, Ok(Ok(RunOutcome::Done(_))))),
                "interrupt" => assert_eq!(result.unwrap().unwrap(), RunOutcome::Interrupted),
                "watchdog" => {
                    assert_eq!(result.unwrap().unwrap(), RunOutcome::WatchdogBoundary);
                    assert_eq!(actor.watchdog_steps_left, Some(actor.max_steps));
                }
                "error" => assert!(result.unwrap().is_err()),
                _ => assert!(result.is_err()),
            }
            assert_eq!(actor.state, AgentState::Idle);
            let ChatMessage::User { content, .. } = &actor.transcript[0] else {
                panic!("expected original input")
            };
            assert_eq!(content.as_ptr() as usize, contents);
            assert_eq!(content, &"large original input".repeat(4096));
            if mode != "unchanged" {
                assert_eq!(actor.transcript.len(), 1);
                assert_eq!(actor.transcript.as_ptr() as usize, messages);
                assert!(server.requests.lock().await.is_empty());
            }
        }
    }
    use crate::tools::bash::BashTool;
    use crate::tools::control::AskTool;
    use crate::tools::native::{GrepTool, ReadTool, WriteTool};
    use crate::tools::{Registry, ToolContext};
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::Mutex;
    use tokio_util::sync::CancellationToken;

    async fn read_request(sock: &mut tokio::net::TcpStream) -> String {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        loop {
            let n = sock.read(&mut tmp).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            let s = String::from_utf8_lossy(&buf);
            if let Some(idx) = s.find("\r\n\r\n") {
                let header = &s[..idx];
                let content_len = header
                    .lines()
                    .find_map(|l| {
                        l.to_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                    })
                    .unwrap_or(0);
                let body_start = idx + 4;
                if buf.len() >= body_start + content_len {
                    break;
                }
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    pub(super) struct MockServer {
        pub(super) url: String,
        pub(super) requests: Arc<Mutex<Vec<String>>>,
    }

    pub(super) async fn spawn_mock(responses: Vec<Vec<u8>>) -> MockServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let req_clone = requests.clone();
        tokio::spawn(async move {
            for resp in responses {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                let req = read_request(&mut sock).await;
                req_clone.lock().await.push(req);
                let _ = sock.write_all(&resp).await;
                let _ = sock.flush().await;
                let _ = sock.shutdown().await;
            }
        });
        MockServer {
            url: format!("http://{addr}/v1"),
            requests,
        }
    }

    pub(super) fn make_actor(
        base_url: String,
        registry: Registry,
        cancel: CancellationToken,
    ) -> AgentActor {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        let mut actor = AgentActor::new(
            registry,
            ToolContext {
                binding: None,
                workspace: ws.clone(),
                writable_roots: vec![ws],
                readable_roots: Vec::new(),
            },
            ProviderConfig {
                base_url,
                api_key_env: None,
                headers: BTreeMap::new(),
                connect_timeout_ms: 2000,
                request_timeout_ms: 600_000,
                catalog_format: None,
            },
            "test-key".to_string(),
            "test-model".to_string(),
            ReasoningHistory::Preserve,
            Some(256),
            cancel,
        );
        // The test workspaces live in the system temp directory, where writes
        // would need no permission: these tests judge them like project writes.
        actor.permission_dirs.temp = Vec::new();
        actor
    }

    /// Loop tests that are not about permissions run under one rule that allows everything.
    pub(super) fn allow_everything(actor: &AgentActor) {
        actor
            .permission_store
            .as_ref()
            .unwrap()
            .add(
                "*",
                "*",
                crate::permission::rules::Effect::Allow,
                crate::permission::rules::RuleScope::Global,
                crate::permission::rules::RuleOrigin::Manual,
            )
            .unwrap();
    }

    pub(super) fn registry_all() -> Registry {
        let mut r = Registry::new();
        r.register(Box::new(ReadTool));
        r.register(Box::new(BashTool));
        r.register(Box::new(AskTool));
        r
    }

    struct PolicyTool(&'static str);

    impl crate::tools::Tool for PolicyTool {
        fn name(&self) -> &str {
            self.0
        }

        fn schema(&self) -> serde_json::Value {
            serde_json::json!({"description":"policy test", "parameters":{"type":"object"}})
        }

        fn call<'a>(
            &'a self,
            _args: serde_json::Value,
            _cancel: CancellationToken,
            _cx: &'a ToolContext,
        ) -> futures::future::BoxFuture<'a, Result<ToolResult, crate::error::ToolError>> {
            panic!("denied tool {} must never dispatch", self.0);
        }
    }

    fn policy_role(
        access: crate::agent::definitions::AgentAccess,
        denied: &[&str],
    ) -> Arc<crate::agent::definitions::AgentDefinition> {
        let mut role = crate::agent::definitions::Agents::builtin()
            .get("build")
            .unwrap()
            .as_ref()
            .clone();
        role.access = access;
        role.tools = denied.iter().map(|name| ((*name).into(), false)).collect();
        Arc::new(role)
    }

    #[tokio::test]
    async fn effective_policy_denies_direct_names_before_review_or_dispatch() {
        use crate::agent::definitions::AgentAccess;
        let names = [
            "read",
            "write",
            "edit",
            "bash",
            "ask",
            "task",
            "steer_subagent",
            "decide_subagent",
            "intent",
            "skill",
            "fetch",
            "grep",
            "glob",
            "custom",
        ];
        for access in [AgentAccess::Full, AgentAccess::ReadOnly] {
            let mut registry = Registry::new();
            for name in names {
                registry.register(Box::new(PolicyTool(name)));
            }
            let mut actor = make_actor("http://unused".into(), registry, CancellationToken::new());
            actor.role = Some(policy_role(
                access,
                if access == AgentAccess::Full {
                    &names
                } else {
                    &[]
                },
            ));
            let denied = if access == AgentAccess::Full {
                names.to_vec()
            } else {
                vec![
                    "write",
                    "edit",
                    "bash",
                    "ask",
                    "task",
                    "steer_subagent",
                    "decide_subagent",
                    "intent",
                ]
            };
            for name in denied {
                assert!(actor.registry.get(name).is_some());
                assert!(!actor.tool_allowed(name), "{name}");
                assert!(
                    actor
                        .effective_tool_schemas()
                        .iter()
                        .all(|schema| schema.function.name != name)
                );
                let call = ToolCall {
                    id: name.into(),
                    name: name.into(),
                    args: serde_json::json!({"command":"not executed", "path":"file", "content":"no"}),
                    args_valid: true,
                };
                assert!(preflight(&actor, &call).is_err());
                assert!(matches!(
                    run_decided(&mut actor, &call, &mut |_| {}, 1, None).await,
                    One::Settled
                ));
                assert!(
                    matches!(actor.transcript.last(), Some(ChatMessage::Tool {content, ..}) if content.contains("disabled by this session's policy"))
                );
            }
            assert!(!actor.has_pending());
            if access == AgentAccess::ReadOnly {
                for name in ["read", "grep", "glob", "fetch", "skill", "custom"] {
                    assert!(actor.tool_allowed(name), "{name}");
                }
            }
        }
    }

    #[test]
    fn role_projection_switches_back_without_expanding_the_frozen_ceiling() {
        use crate::agent::definitions::AgentAccess;
        let mut registry = Registry::new();
        for name in ["read", "write", "skill", "custom"] {
            registry.register(Box::new(PolicyTool(name)));
        }
        let mut actor = make_actor("http://unused".into(), registry, CancellationToken::new());
        let model = actor.model.clone();
        let frozen = actor.registry.names();
        let full = policy_role(AgentAccess::Full, &[]);
        let narrow = policy_role(AgentAccess::ReadOnly, &["skill", "read"]);
        actor.role = Some(full.clone());
        let original = actor.effective_tool_schemas();
        let projected = actor.effective_tool_schemas_for_role(Some(&narrow));
        assert_eq!(projected.len(), 1);
        actor.role = Some(narrow);
        assert_eq!(
            serde_json::to_value(actor.effective_tool_schemas()).unwrap(),
            serde_json::to_value(projected).unwrap()
        );
        assert!(!actor.tool_allowed("write"));
        assert!(!actor.tool_allowed("skill"));
        assert!(!actor.tool_allowed("read"));
        actor.role = Some(full);
        assert_eq!(
            serde_json::to_value(actor.effective_tool_schemas()).unwrap(),
            serde_json::to_value(original).unwrap()
        );
        assert_eq!(actor.registry.names(), frozen);
        assert_eq!(actor.model, model);
        assert!(!actor.tool_allowed("bash"));
        assert!(!actor.tool_allowed("task"));
    }

    #[tokio::test]
    async fn disabled_ask_and_skill_are_hidden_and_cannot_bypass_policy() {
        use crate::agent::definitions::AgentAccess;
        let calls = calls_chunk(&[
            (
                "ask-denied",
                "ask",
                serde_json::json!({"question":"yield bypass?"}),
            ),
            (
                "skill-denied",
                "skill",
                serde_json::json!({"name":"policy-skill"}),
            ),
        ]);
        let server = spawn_mock(vec![
            http_sse(&sse_body(&[&calls])),
            http_sse(&sse_body(&[&text_chunk("done")])),
        ])
        .await;
        let mut actor = make_actor(server.url.clone(), registry_all(), CancellationToken::new());
        let skill_dir = actor
            .tool_context
            .workspace
            .join(".agents/skills/policy-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: policy-skill\ndescription: Policy catalog marker.\n---\nInstructions.\n",
        )
        .unwrap();
        actor.set_skills(crate::agent::skills::Skills::load(
            &crate::agent::instructions::RulesRoots::default(),
            &actor.tool_context.workspace,
        ));
        assert!(actor.effective_skills_catalog().is_some());
        let full = policy_role(AgentAccess::Full, &[]);
        let denied = policy_role(AgentAccess::Full, &["ask", "skill"]);
        assert!(
            actor
                .effective_skills_catalog_for_role(Some(&denied))
                .is_none()
        );
        actor.role = Some(denied);
        let schemas = actor.effective_tool_schemas();
        assert!(actor.components(None, &schemas).skills.is_none());
        assert_eq!(
            actor.context_breakdown().tools,
            crate::agent::context::estimate(
                actor.role_message().as_deref(),
                actor.rules.message(),
                actor.effective_skills_catalog(),
                &actor.effective_history(),
                &schemas,
            )
            .tools
        );
        assert_eq!(actor.context_breakdown().skills_catalog, 0);
        assert!(actor.effective_skills_catalog().is_none());
        actor.push_user("go");
        assert!(matches!(
            run_agent(&mut actor, &mut |_| {}).await.unwrap(),
            RunOutcome::Done(_)
        ));
        assert!(!actor.has_pending());
        for id in ["ask-denied", "skill-denied"] {
            assert!(actor.transcript.iter().any(|message| matches!(message, ChatMessage::Tool {tool_call_id, content, .. } if tool_call_id == id && content.contains("disabled by this session's policy"))));
        }
        let requests = server.requests.lock().await;
        for request in requests.iter() {
            let body: serde_json::Value =
                serde_json::from_str(request.split("\r\n\r\n").nth(1).unwrap()).unwrap();
            assert!(
                body["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|tool| !matches!(
                        tool["function"]["name"].as_str(),
                        Some("ask" | "skill")
                    ))
            );
            assert!(!body["messages"].as_array().unwrap().iter().any(|message| {
                message["role"] == "system"
                    && message["content"]
                        .as_str()
                        .unwrap_or("")
                        .contains("Policy catalog marker")
            }));
        }
        actor.role = Some(full);
        assert!(actor.effective_skills_catalog().is_some());
        assert!(actor.context_breakdown().skills_catalog > 0);
    }

    #[tokio::test]
    async fn continue_then_done_runs_tool_and_finishes() {
        let toolcall = r#"{"choices":[{"index":0,"delta":{"reasoning_content":"let me look","tool_calls":[{"index":0,"id":"call_1","function":{"name":"bash","arguments":"{\"command\":\"echo hi\"}"}}]},"finish_reason":"tool_calls"}]}"#;
        let done =
            r#"{"choices":[{"index":0,"delta":{"content":"all done"},"finish_reason":"stop"}]}"#;
        let server = spawn_mock(vec![
            http_sse(&sse_body(&[toolcall])),
            http_sse(&sse_body(&[done])),
        ])
        .await;

        let cancel = CancellationToken::new();
        let mut actor = make_actor(server.url.clone(), registry_all(), cancel);
        allow_everything(&actor);
        actor.push_user("run echo hi");
        actor.input_budget = Some(u64::MAX);
        let first_request = build_request(
            &actor.model,
            actor.role_message().as_deref(),
            actor.rules.message(),
            actor.effective_skills_catalog(),
            actor.effective_history(),
            actor.effective_tool_schemas(),
            actor.max_tokens,
        );
        let mut noop = |_e: AgentEvent| {};
        let outcome = run_agent(&mut actor, &mut noop).await.unwrap();
        assert!(matches!(outcome, RunOutcome::Done(m) if m.text == "all done"));

        let reqs = server.requests.lock().await;
        assert_eq!(reqs.len(), 2);
        let second = &reqs[1];
        assert!(second.contains("\"role\":\"tool\""));
        assert!(second.contains("\"tool_call_id\":\"call_1\""));
        assert!(second.contains("\"reasoning_content\":\"let me look\""));
        assert!(second.contains("\"tool_calls\""));
        let second_request = build_request(
            &actor.model,
            actor.role_message().as_deref(),
            actor.rules.message(),
            actor.effective_skills_catalog(),
            actor.transcript[..actor.transcript.len() - 1].to_vec(),
            actor.effective_tool_schemas(),
            actor.max_tokens,
        );
        for (actual, expected) in reqs.iter().zip([first_request, second_request]) {
            let actual: serde_json::Value =
                serde_json::from_str(actual.split("\r\n\r\n").nth(1).unwrap()).unwrap();
            assert_eq!(actual, serde_json::to_value(expected).unwrap());
        }
    }

    /// Serves keep-alive connections and counts how many were opened.
    async fn spawn_keepalive_mock(
        bodies: Vec<String>,
    ) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let connections = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = connections.clone();
        let bodies = Arc::new(Mutex::new(bodies.into_iter()));
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let bodies = bodies.clone();
                tokio::spawn(async move {
                    loop {
                        let req = read_request(&mut sock).await;
                        if req.is_empty() {
                            break;
                        }
                        let Some(body) = bodies.lock().await.next() else {
                            break;
                        };
                        let head = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n",
                            body.len()
                        );
                        let _ = sock.write_all(head.as_bytes()).await;
                        let _ = sock.write_all(body.as_bytes()).await;
                        let _ = sock.flush().await;
                    }
                });
            }
        });
        (format!("http://{addr}/v1"), connections)
    }

    #[tokio::test]
    async fn one_connection_serves_every_step_of_a_turn() {
        let toolcall = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"bash","arguments":"{\"command\":\"true\"}"}}]},"finish_reason":"tool_calls"}]}"#;
        let done = r#"{"choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":"stop"}]}"#;
        let (url, connections) =
            spawn_keepalive_mock(vec![sse_body(&[toolcall]), sse_body(&[done])]).await;

        let mut actor = make_actor(url, registry_all(), CancellationToken::new());
        allow_everything(&actor);
        actor.push_user("go");
        let mut noop = |_e: AgentEvent| {};
        let outcome = run_agent(&mut actor, &mut noop).await.unwrap();
        assert!(matches!(outcome, RunOutcome::Done(_)));
        // Two steps, one pooled client, one connection.
        assert_eq!(connections.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn ask_yields_then_resume_same_transcript() {
        let ask = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"ask_1","function":{"name":"ask","arguments":"{\"question\":\"insert SD card\"}"}}]},"finish_reason":"tool_calls"}]}"#;
        let done = r#"{"choices":[{"index":0,"delta":{"content":"thanks, continuing"},"finish_reason":"stop"}]}"#;
        let server = spawn_mock(vec![
            http_sse(&sse_body(&[ask])),
            http_sse(&sse_body(&[done])),
        ])
        .await;

        let cancel = CancellationToken::new();
        let mut actor = make_actor(server.url.clone(), registry_all(), cancel);
        actor.push_user("set up the SD card");
        let mut noop = |_e: AgentEvent| {};
        let first = run_agent(&mut actor, &mut noop).await.unwrap();
        match first {
            RunOutcome::Yielded(req) => assert_eq!(req.question, "insert SD card"),
            other => panic!("expected Yielded, got {other:?}"),
        }
        let transcript_len_after_yield = actor.transcript.len();

        actor.push_user("done, it is visible");
        let second = run_agent(&mut actor, &mut noop).await.unwrap();
        assert!(matches!(second, RunOutcome::Done(m) if m.text == "thanks, continuing"));
        assert!(actor.transcript.len() > transcript_len_after_yield);

        let reqs = server.requests.lock().await;
        let resume = &reqs[1];
        assert!(resume.contains("\"tool_call_id\":\"ask_1\""));
        assert!(resume.contains("done, it is visible"));
    }

    #[tokio::test]
    async fn cancel_mid_loop_returns_interrupted_no_further_tools() {
        let toolcall = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"bash","arguments":"{\"command\":\"echo hi\"}"}}]},"finish_reason":"tool_calls"}]}"#;
        let server = spawn_mock(vec![http_sse(&sse_body(&[toolcall]))]).await;

        let cancel = CancellationToken::new();
        let mut actor = make_actor(server.url.clone(), registry_all(), cancel.clone());
        actor.push_user("go");
        cancel.cancel();
        let mut noop = |_e: AgentEvent| {};
        let outcome = run_agent(&mut actor, &mut noop).await.unwrap();
        assert!(matches!(outcome, RunOutcome::Interrupted));
        assert_eq!(actor.state, AgentState::Idle);
    }

    #[tokio::test]
    async fn failed_runs_restore_idle_and_keep_committed_boundary_input() {
        for boundary_error in [false, true] {
            let response = b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n".to_vec();
            let server = spawn_mock(vec![response]).await;
            let mut actor = make_actor(server.url, registry_all(), CancellationToken::new());
            actor.push_user("go");
            if boundary_error {
                actor.safe_boundary = Some(Arc::new(|transcript, _| {
                    Box::pin(async move {
                        let mut transcript = transcript.to_vec();
                        transcript.push(ChatMessage::User {
                            provenance: crate::provider::openai::UserProvenance::Parent,
                            content: "committed steering".into(),
                            images: Vec::new(),
                        });
                        Err(crate::agent::boundary::BoundaryError {
                            message: "delivery failed".into(),
                            committed: Some(transcript),
                        })
                    })
                }));
            }
            assert!(run_agent(&mut actor, &mut |_| {}).await.is_err());
            assert_eq!(actor.state, AgentState::Idle);
            if boundary_error {
                assert!(
                    matches!(actor.transcript.last(), Some(ChatMessage::User { content, .. }) if content == "committed steering")
                );
            }
        }
    }

    #[tokio::test]
    async fn on_delta_streams_text_incrementally() {
        let c1 = r#"{"choices":[{"index":0,"delta":{"content":"one "}}]}"#;
        let c2 = r#"{"choices":[{"index":0,"delta":{"content":"two "}}]}"#;
        let c3 = r#"{"choices":[{"index":0,"delta":{"content":"three"},"finish_reason":"stop"}]}"#;
        let server = spawn_mock(vec![http_sse(&sse_body(&[c1, c2, c3]))]).await;

        let cancel = CancellationToken::new();
        let mut actor = make_actor(server.url.clone(), registry_all(), cancel);
        actor.push_user("count");
        let mut timeline: Vec<String> = Vec::new();
        {
            let mut on_delta = |e: AgentEvent| {
                if let AgentEvent::Delta(StreamDelta::Text { text }) = e {
                    timeline.push(text);
                }
            };
            let outcome = run_agent(&mut actor, &mut on_delta).await.unwrap();
            assert!(matches!(outcome, RunOutcome::Done(_)));
        }
        assert_eq!(timeline, vec!["one ", "two ", "three"]);
    }

    #[tokio::test]
    async fn max_steps_exhaustion_yields_not_forced_continuation() {
        for (absent, invalid) in [
            (usize::MAX, WATCHDOG_FALLBACK_STEPS),
            (DEFAULT_MAX_STEPS, DEFAULT_MAX_STEPS),
        ] {
            assert_eq!(
                configured_max_steps(Err(std::env::VarError::NotPresent), absent, invalid),
                absent
            );
            for value in ["0", "1", "50"] {
                assert_eq!(
                    configured_max_steps(Ok(value.into()), absent, invalid),
                    value.parse::<usize>().unwrap()
                );
            }
            for value in ["", "50steps", "-1", "999999999999999999999999999999"] {
                assert_eq!(
                    configured_max_steps(Ok(value.into()), absent, invalid),
                    invalid
                );
            }
            assert_eq!(
                configured_max_steps(
                    Err(std::env::VarError::NotUnicode(std::ffi::OsString::new())),
                    absent,
                    invalid,
                ),
                invalid
            );
        }
        let toolcall = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"bash","arguments":"{\"command\":\"true\"}"}}]},"finish_reason":"tool_calls"}]}"#;
        let server = spawn_mock(vec![
            http_sse(&sse_body(&[toolcall])),
            http_sse(&sse_body(&[toolcall])),
        ])
        .await;
        let cancel = CancellationToken::new();
        let mut actor = make_actor(server.url.clone(), registry_all(), cancel);
        allow_everything(&actor);
        actor.max_steps = 2;
        actor.push_user("loop forever");
        let mut noop = |_e: AgentEvent| {};
        let outcome = run_agent(&mut actor, &mut noop).await.unwrap();
        assert!(matches!(outcome, RunOutcome::StepBudgetExhausted));
    }

    #[tokio::test]
    async fn cancellation_on_last_budget_tool_returns_interrupted_with_paired_result() {
        let toolcall = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"last","function":{"name":"bash","arguments":"{\"command\":\"true\"}"}}]},"finish_reason":"tool_calls"}]}"#;
        let server = spawn_mock(vec![
            http_sse(&sse_body(&[toolcall])),
            http_sse(&sse_body(&[&text_chunk("replacement done")])),
        ])
        .await;
        let cancel = CancellationToken::new();
        let mut actor = make_actor(server.url.clone(), registry_all(), cancel.clone());
        allow_everything(&actor);
        actor.max_steps = 1;
        actor.push_user("run one tool");
        let mut observe = |event: AgentEvent| {
            if matches!(event, AgentEvent::PostCall { .. }) {
                cancel.cancel();
            }
        };
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            run_agent(&mut actor, &mut observe),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(matches!(outcome, RunOutcome::Interrupted));
        assert_eq!(
            actor
                .transcript
                .iter()
                .filter(|m| matches!(m,ChatMessage::Tool {tool_call_id,..} if tool_call_id=="last"))
                .count(),
            1
        );
        actor.cancel = CancellationToken::new();
        actor.push_user("replacement instruction");
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            run_agent(&mut actor, &mut |_| {}),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(matches!(outcome, RunOutcome::Done(_)));
    }

    // Given an input budget the next request would exceed
    // When the run reaches a step boundary
    // Then it stops with NeedsCompaction and the steps it has left, sending
    //      nothing; continue_run carries on with exactly those steps
    #[tokio::test]
    async fn a_prompt_over_the_budget_stops_at_the_boundary_and_keeps_the_step_budget() {
        let toolcall = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"bash","arguments":"{\"command\":\"echo hi\"}"}}]},"finish_reason":"tool_calls"}]}"#;
        let done = r#"{"choices":[{"index":0,"delta":{"content":"fin"},"finish_reason":"stop"}]}"#;
        let server = spawn_mock(vec![
            http_sse(&sse_body(&[toolcall])),
            http_sse(&sse_body(&[done])),
        ])
        .await;
        let cancel = CancellationToken::new();
        let mut actor = make_actor(server.url.clone(), registry_all(), cancel);
        allow_everything(&actor);
        actor.max_steps = 5;
        actor.push_user("go");
        // Room for the first request, not for the second.
        let first = actor.context_breakdown().total();
        actor.input_budget = Some(first);
        let mut noop = |_e: AgentEvent| {};

        let outcome = run_agent(&mut actor, &mut noop).await.unwrap();
        // One step taken (step 0), so four are left.
        assert!(
            matches!(outcome, RunOutcome::NeedsCompaction { steps_left: 4 }),
            "{outcome:?}"
        );
        assert_eq!(server.requests.lock().await.len(), 1, "nothing was sent");
        assert_eq!(actor.state, AgentState::Idle, "compaction suspends the run");
        assert!(matches!(
            actor.transcript.last(),
            Some(ChatMessage::Tool { .. })
        ));

        actor.input_budget = None;
        let outcome = continue_run(&mut actor, &mut noop, 4).await.unwrap();
        assert!(matches!(outcome, RunOutcome::Done(m) if m.text == "fin"));
    }

    #[tokio::test]
    async fn default_root_turn_runs_past_sixty_steps() {
        let call = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"glob","arguments":"{\"pattern\":\"*\"}"}}]},"finish_reason":"tool_calls"}]}"#;
        let done =
            r#"{"choices":[{"index":0,"delta":{"content":"finished"},"finish_reason":"stop"}]}"#;
        let mut replies = vec![http_sse(&sse_body(&[call])); 60];
        replies.push(http_sse(&sse_body(&[done])));
        let server = spawn_mock(replies).await;
        let mut actor = make_actor(server.url, registry_all(), CancellationToken::new());
        assert_eq!(actor.max_steps, usize::MAX);
        actor.push_user("keep going");
        let mut steps = 0;
        let result = run_agent(&mut actor, &mut |event| {
            if matches!(event, AgentEvent::StepStarted) {
                steps += 1;
            }
        })
        .await
        .unwrap();
        assert!(matches!(result, RunOutcome::Done(_)));
        assert_eq!(steps, 61);
    }

    // Given a model that keeps calling tools and a prompt over the budget at
    //       the first step
    // Then the run is held before any request, with the whole budget left
    #[tokio::test]
    async fn without_a_budget_a_run_is_never_held_back() {
        let done = r#"{"choices":[{"index":0,"delta":{"content":"fin"},"finish_reason":"stop"}]}"#;
        let server = spawn_mock(vec![http_sse(&sse_body(&[done]))]).await;
        let mut actor = make_actor(server.url.clone(), registry_all(), CancellationToken::new());
        actor.push_user("go");
        assert!(actor.input_budget.is_none());
        let mut noop = |_e: AgentEvent| {};
        let outcome = run_agent(&mut actor, &mut noop).await.unwrap();
        assert!(matches!(outcome, RunOutcome::Done(_)));
    }

    /// One assistant message holding the given `(id, tool, arguments)` calls.
    pub(super) fn calls_chunk(calls: &[(&str, &str, serde_json::Value)]) -> String {
        let calls: Vec<serde_json::Value> = calls
            .iter()
            .enumerate()
            .map(|(index, (id, name, args))| {
                serde_json::json!({
                    "index": index,
                    "id": id,
                    "function": {"name": name, "arguments": args.to_string()},
                })
            })
            .collect();
        serde_json::json!({"choices": [{
            "index": 0,
            "delta": {"tool_calls": calls},
            "finish_reason": "tool_calls",
        }]})
        .to_string()
    }

    pub(super) fn text_chunk(text: &str) -> String {
        serde_json::json!({"choices": [{
            "index": 0,
            "delta": {"content": text},
            "finish_reason": "stop",
        }]})
        .to_string()
    }

    pub(super) fn registry_gated() -> Registry {
        let mut r = registry_all();
        r.register(Box::new(WriteTool));
        r.register(Box::new(GrepTool));
        r
    }

    pub(super) fn as_build(actor: &mut AgentActor) {
        actor.role = Some(
            crate::agent::definitions::Agents::builtin()
                .get("build")
                .unwrap()
                .clone(),
        );
    }

    pub(super) fn tool_result_of(actor: &AgentActor, id: &str) -> Option<String> {
        actor.transcript.iter().find_map(|m| match m {
            ChatMessage::Tool {
                tool_call_id,
                content,
                ..
            } if tool_call_id == id => Some(content.clone()),
            _ => None,
        })
    }

    #[tokio::test]
    async fn a_write_is_not_executed_and_the_turn_yields_with_a_pending_permission() {
        let mut actor = make_actor(
            "http://127.0.0.1:1/v1".into(),
            registry_gated(),
            CancellationToken::new(),
        );
        as_build(&mut actor);
        let target = actor.tool_context.workspace.join("a.txt");
        let server = spawn_mock(vec![http_sse(&sse_body(&[&calls_chunk(&[(
            "w1",
            "write",
            serde_json::json!({"path": target, "content": "hello"}),
        )])]))])
        .await;
        actor.provider.base_url = server.url.clone();
        actor.push_user("write it");
        let mut events = Vec::new();
        let mut observe = |e: AgentEvent| {
            if let AgentEvent::PreCall { name, .. } = e {
                events.push(name.to_string());
            }
        };
        let outcome = run_agent(&mut actor, &mut observe).await.unwrap();
        let RunOutcome::PermissionRequired(request) = outcome else {
            panic!("expected a permission request");
        };
        assert_eq!(request.tool, "write");
        assert_eq!(request.tool_call_id, "w1");
        assert_eq!(request.agent, "build");
        assert!(!target.exists(), "the write must not have run");
        assert!(events.is_empty(), "no pre-call for a gated call");
        assert_eq!(actor.state, AgentState::Yielded);
        assert!(actor.has_pending());
        assert_eq!(actor.pending_permission(), Some(&request));
        assert_eq!(tool_result_of(&actor, "w1"), None);
    }

    #[tokio::test]
    async fn reads_still_run_and_the_siblings_of_a_gated_call_wait_behind_it() {
        let mut actor = make_actor(
            "http://127.0.0.1:1/v1".into(),
            registry_gated(),
            CancellationToken::new(),
        );
        as_build(&mut actor);
        let ws = actor.tool_context.workspace.clone();
        std::fs::write(ws.join("r.txt"), "needle").unwrap();
        let target = ws.join("b.txt");
        let server = spawn_mock(vec![http_sse(&sse_body(&[&calls_chunk(&[
            ("g1", "grep", serde_json::json!({"pattern": "needle"})),
            (
                "w1",
                "write",
                serde_json::json!({"path": target, "content": "x"}),
            ),
            ("r1", "read", serde_json::json!({"path": ws.join("r.txt")})),
        ])]))])
        .await;
        actor.provider.base_url = server.url.clone();
        actor.push_user("go");
        let mut noop = |_e: AgentEvent| {};
        let outcome = run_agent(&mut actor, &mut noop).await.unwrap();
        assert!(matches!(outcome, RunOutcome::PermissionRequired(_)));
        assert!(tool_result_of(&actor, "g1").unwrap().contains("needle"));
        assert_eq!(tool_result_of(&actor, "w1"), None);
        assert_eq!(tool_result_of(&actor, "r1"), None);
        let Some(PendingInteraction::Permission {
            call,
            remaining_calls,
            ..
        }) = &actor.pending
        else {
            panic!("expected a pending permission");
        };
        assert_eq!(call.id, "w1");
        assert_eq!(remaining_calls.len(), 1);
        assert_eq!(remaining_calls[0].id, "r1");
    }

    #[tokio::test]
    async fn a_bare_runtime_without_a_role_asks_too() {
        let mut actor = make_actor(
            "http://127.0.0.1:1/v1".into(),
            registry_gated(),
            CancellationToken::new(),
        );
        let target = actor.tool_context.workspace.join("c.txt");
        let server = spawn_mock(vec![http_sse(&sse_body(&[&calls_chunk(&[(
            "w1",
            "write",
            serde_json::json!({"path": target, "content": "x"}),
        )])]))])
        .await;
        actor.provider.base_url = server.url.clone();
        actor.push_user("go");
        let mut noop = |_e: AgentEvent| {};
        let outcome = run_agent(&mut actor, &mut noop).await.unwrap();
        // No role and no rule: the write waits for the Captain.
        assert!(matches!(outcome, RunOutcome::PermissionRequired(_)));
        assert!(!target.exists());
    }

    /// A build actor with a scripted provider, yielded on a pending `write`
    /// of `a.txt` followed by a second `write` of `b.txt` in the same message.
    /// Returns the two targets and the mock server.
    pub(super) async fn yielded_on_two_writes(
        then: Vec<Vec<u8>>,
    ) -> (
        AgentActor,
        std::path::PathBuf,
        std::path::PathBuf,
        MockServer,
    ) {
        let mut actor = make_actor(
            "http://127.0.0.1:1/v1".into(),
            registry_gated(),
            CancellationToken::new(),
        );
        as_build(&mut actor);
        let ws = actor.tool_context.workspace.canonicalize().unwrap();
        let (a, b) = (ws.join("a.txt"), ws.join("sub").join("b.txt"));
        let mut responses = vec![http_sse(&sse_body(&[&calls_chunk(&[
            (
                "w1",
                "write",
                serde_json::json!({"path": a, "content": "A"}),
            ),
            (
                "w2",
                "write",
                serde_json::json!({"path": b, "content": "B"}),
            ),
        ])]))];
        responses.extend(then);
        let server = spawn_mock(responses).await;
        actor.provider.base_url = server.url.clone();
        actor.push_user("go");
        let mut noop = |_e: AgentEvent| {};
        let outcome = run_agent(&mut actor, &mut noop).await.unwrap();
        assert!(matches!(outcome, RunOutcome::PermissionRequired(_)));
        (actor, a, b, server)
    }

    pub(super) fn pending_id(actor: &AgentActor) -> ulid::Ulid {
        actor.pending_permission().unwrap().id
    }

    #[tokio::test]
    async fn allow_once_runs_that_call_once_then_asks_again_for_the_sibling() {
        let (mut actor, a, b, server) = yielded_on_two_writes(vec![]).await;
        let first = pending_id(&actor);
        let mut precalls = Vec::new();
        let mut observe = |e: AgentEvent| {
            if let AgentEvent::PreCall { tool_call_id, .. } = e {
                precalls.push(tool_call_id.to_string());
            }
        };
        let outcome = resolve_permission(
            &mut actor,
            first,
            PermissionDecision::AllowOnce,
            &mut observe,
        )
        .await
        .unwrap();
        let RunOutcome::PermissionRequired(second) = outcome else {
            panic!("the sibling write must ask again");
        };
        assert_eq!(second.tool_call_id, "w2");
        assert_ne!(second.id, first);
        assert_eq!(precalls, ["w1"]);
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "A");
        assert!(!b.exists());
        assert_eq!(tool_result_of(&actor, "w1").unwrap(), "wrote 1 bytes");
        assert_eq!(tool_result_of(&actor, "w2"), None);
        // No model request was made by the resume.
        assert_eq!(server.requests.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn old_allow_folder_answer_runs_once_then_reviews_the_sibling() {
        let done = http_sse(&sse_body(&[&text_chunk("all written")]));
        let (mut actor, a, b, server) = yielded_on_two_writes(vec![done]).await;
        let id = pending_id(&actor);
        let mut noop = |_e: AgentEvent| {};
        let outcome =
            resolve_permission(&mut actor, id, PermissionDecision::AllowFolder, &mut noop)
                .await
                .unwrap();
        assert!(matches!(outcome, RunOutcome::PermissionRequired(_)));
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "A");
        assert!(!b.exists());
        assert!(actor.has_pending());
        let requests = server.requests.lock().await;
        assert_eq!(requests.len(), 1);
    }

    #[tokio::test]
    async fn legacy_deny_rejects_the_call_and_every_held_sibling() {
        let done = http_sse(&sse_body(&[&text_chunk("understood")]));
        let (mut actor, a, _b, _server) = yielded_on_two_writes(vec![done]).await;
        let first = pending_id(&actor);
        let mut noop = |_e: AgentEvent| {};
        let outcome = resolve_permission(
            &mut actor,
            first,
            PermissionDecision::Deny {
                message: "not in this repo".into(),
            },
            &mut noop,
        )
        .await
        .unwrap();
        // A legacy deny is a reject: the model goes on with both calls refused.
        assert!(matches!(outcome, RunOutcome::Done(m) if m.text == "understood"));
        assert!(!a.exists(), "a denied call never runs");
        let denied: serde_json::Value =
            serde_json::from_str(&tool_result_of(&actor, "w1").unwrap()).unwrap();
        assert_eq!(denied["status"], "denied");
        assert_eq!(denied["error"]["code"], "permission_denied");
        assert_eq!(denied["error"]["message"], "not in this repo");
        let sibling: serde_json::Value =
            serde_json::from_str(&tool_result_of(&actor, "w2").unwrap()).unwrap();
        assert_eq!(sibling["error"]["message"], "not in this repo");
    }

    #[tokio::test]
    async fn a_stale_wrong_or_repeated_request_id_is_rejected_and_changes_nothing() {
        let done = http_sse(&sse_body(&[&text_chunk("ok")]));
        let (mut actor, a, _b, _server) = yielded_on_two_writes(vec![done]).await;
        let id = pending_id(&actor);
        let before = actor.transcript.len();
        let mut noop = |_e: AgentEvent| {};

        let wrong = ulid::Ulid::generate();
        let rejected =
            resolve_permission(&mut actor, wrong, PermissionDecision::AllowOnce, &mut noop).await;
        assert!(matches!(
            rejected,
            Err(ResolveError::Rejected(PermissionError::Stale))
        ));
        assert_eq!(actor.transcript.len(), before);
        assert_eq!(pending_id(&actor), id, "the real request still waits");
        assert!(!a.exists());

        resolve_permission(&mut actor, id, PermissionDecision::AllowOnce, &mut noop)
            .await
            .unwrap();
        // The same id again: it was answered already.
        let again =
            resolve_permission(&mut actor, id, PermissionDecision::AllowOnce, &mut noop).await;
        assert!(matches!(
            again,
            Err(ResolveError::Rejected(PermissionError::Stale))
        ));
    }

    #[tokio::test]
    async fn an_idle_actor_or_a_pending_ask_rejects_any_decision() {
        let mut actor = make_actor(
            "http://127.0.0.1:1/v1".into(),
            registry_gated(),
            CancellationToken::new(),
        );
        let mut noop = |_e: AgentEvent| {};
        let idle = resolve_permission(
            &mut actor,
            ulid::Ulid::generate(),
            PermissionDecision::AllowOnce,
            &mut noop,
        )
        .await;
        assert!(matches!(idle, Err(ResolveError::Rejected(_))));

        actor.pending = Some(PendingInteraction::Ask {
            tool_call_id: "ask_1".into(),
        });
        let asked = resolve_permission(
            &mut actor,
            ulid::Ulid::generate(),
            PermissionDecision::AllowOnce,
            &mut noop,
        )
        .await;
        assert!(matches!(asked, Err(ResolveError::Rejected(_))));
        assert!(matches!(
            actor.pending,
            Some(PendingInteraction::Ask { .. })
        ));
    }

    #[tokio::test]
    async fn user_text_never_answers_a_permission_and_settles_its_calls() {
        let (mut actor, a, b, _server) = yielded_on_two_writes(vec![]).await;
        actor.push_user("yes, go ahead");
        assert!(!actor.has_pending());
        assert!(!a.exists() && !b.exists(), "text is never an approval");
        // The model reads that nothing ran, never an empty success.
        assert_eq!(tool_result_of(&actor, "w1").unwrap(), NOT_RUN);
        assert_eq!(tool_result_of(&actor, "w2").unwrap(), NOT_RUN);
        assert!(matches!(
            actor.transcript.last(),
            Some(ChatMessage::User { content, .. }) if content == "yes, go ahead"
        ));
    }

    #[tokio::test]
    async fn a_stop_settles_every_held_call_as_interrupted() {
        let (mut actor, a, b, _server) = yielded_on_two_writes(vec![]).await;
        actor.interrupt_pending_permission();
        assert!(!actor.has_pending());
        assert!(!a.exists() && !b.exists());
        // The model reads that nothing ran, never an empty success.
        assert_eq!(tool_result_of(&actor, "w1").as_deref(), Some(NOT_RUN));
        assert_eq!(tool_result_of(&actor, "w2").as_deref(), Some(NOT_RUN));
    }

    #[test]
    fn an_interrupted_call_never_reads_as_an_empty_success() {
        let mut actor = make_actor(
            "http://127.0.0.1:1/v1".into(),
            registry_all(),
            CancellationToken::new(),
        );
        append_tool_result(&mut actor, "a", &ToolResult::not_started());
        append_tool_result(
            &mut actor,
            "b",
            &ToolResult::interrupted_with("line 1\nline 2"),
        );
        append_tool_result(&mut actor, "c", &ToolResult::interrupted());
        assert_eq!(
            tool_result_of(&actor, "a").as_deref(),
            Some("interrupted: the call did not run")
        );
        assert_eq!(
            tool_result_of(&actor, "b").as_deref(),
            Some("interrupted while running; partial output:\nline 1\nline 2")
        );
        assert_eq!(
            tool_result_of(&actor, "c").as_deref(),
            Some("interrupted while running; partial output: (none)")
        );
    }

    #[tokio::test]
    async fn a_write_outside_the_roots_is_refused_without_asking_or_creating_anything() {
        let mut actor = make_actor(
            "http://127.0.0.1:1/v1".into(),
            registry_gated(),
            CancellationToken::new(),
        );
        as_build(&mut actor);
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("new/deeper/a.txt");
        let server = spawn_mock(vec![
            http_sse(&sse_body(&[&calls_chunk(&[(
                "w1",
                "write",
                serde_json::json!({"path": target, "content": "x"}),
            )])])),
            http_sse(&sse_body(&[&text_chunk("done")])),
        ])
        .await;
        actor.provider.base_url = server.url.clone();
        actor.push_user("go");
        let mut noop = |_e: AgentEvent| {};
        let outcome = run_agent(&mut actor, &mut noop).await.unwrap();
        assert!(matches!(outcome, RunOutcome::PermissionRequired(_)));
        assert!(actor.has_pending());
        assert!(!outside.path().join("new").exists(), "nothing was created");
        assert!(tool_result_of(&actor, "w1").is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_answer_writes_the_file_it_was_shown_even_if_a_link_moves() {
        let mut actor = make_actor(
            "http://127.0.0.1:1/v1".into(),
            registry_gated(),
            CancellationToken::new(),
        );
        as_build(&mut actor);
        let ws = actor.tool_context.workspace.canonicalize().unwrap();
        let config = tempfile::tempdir().unwrap();
        let config = config.keep().canonicalize().unwrap();
        actor.tool_context.writable_roots = vec![ws.clone(), config.clone()];
        let reviewed = ws.join("reviewed");
        std::fs::create_dir_all(&reviewed).unwrap();
        let link = ws.join("link");
        std::os::unix::fs::symlink(&reviewed, &link).unwrap();
        let server = spawn_mock(vec![
            http_sse(&sse_body(&[&calls_chunk(&[(
                "w1",
                "write",
                serde_json::json!({"path": link.join("f.txt"), "content": "x"}),
            )])])),
            http_sse(&sse_body(&[&text_chunk("done")])),
        ])
        .await;
        actor.provider.base_url = server.url.clone();
        actor.push_user("go");
        let mut noop = |_e: AgentEvent| {};
        let RunOutcome::PermissionRequired(first) = run_agent(&mut actor, &mut noop).await.unwrap()
        else {
            panic!("expected a request");
        };
        assert_eq!(first.scope, Scope::Folder(reviewed.clone()));

        // The link moves to the config directory while the request waits.
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&config, &link).unwrap();

        let outcome = resolve_permission(
            &mut actor,
            first.id,
            PermissionDecision::AllowOnce,
            &mut noop,
        )
        .await
        .unwrap();
        // The answer is bound to the file the Captain was shown, opened without
        // following links: the moved link cannot carry the write elsewhere.
        assert!(matches!(outcome, RunOutcome::Done(_)), "{outcome:?}");
        assert_eq!(
            std::fs::read_to_string(reviewed.join("f.txt")).unwrap(),
            "x"
        );
        assert!(!config.join("f.txt").exists());
    }

    #[tokio::test]
    async fn repeated_approvals_share_one_step_budget() {
        let mut actor = make_actor(
            "http://127.0.0.1:1/v1".into(),
            registry_gated(),
            CancellationToken::new(),
        );
        as_build(&mut actor);
        actor.max_steps = 2;
        let ws = actor.tool_context.workspace.canonicalize().unwrap();
        let write = |id: &str, name: &str| {
            http_sse(&sse_body(&[&calls_chunk(&[(
                id,
                "write",
                serde_json::json!({"path": ws.join(name), "content": "x"}),
            )])]))
        };
        let server = spawn_mock(vec![
            write("w1", "a.txt"),
            write("w2", "b.txt"),
            write("w3", "c.txt"),
        ])
        .await;
        actor.provider.base_url = server.url.clone();
        actor.push_user("go");
        let mut noop = |_e: AgentEvent| {};
        let mut outcome = run_agent(&mut actor, &mut noop).await.unwrap();
        let mut approvals = 0;
        while let RunOutcome::PermissionRequired(request) = outcome {
            approvals += 1;
            assert!(approvals <= 2, "the budget bounds the approvals");
            outcome = resolve_permission(
                &mut actor,
                request.id,
                PermissionDecision::AllowOnce,
                &mut noop,
            )
            .await
            .unwrap();
        }
        assert!(matches!(outcome, RunOutcome::StepBudgetExhausted));
        assert_eq!(approvals, 2);
        assert_eq!(server.requests.lock().await.len(), 2);
    }

    fn history_of_turns(turns: usize) -> Vec<ChatMessage> {
        use crate::provider::openai::{RequestFunction, RequestToolCall};
        let mut t = Vec::new();
        for n in 0..turns {
            t.push(ChatMessage::User {
                provenance: Default::default(),
                content: format!("request {n}"),
                images: Vec::new(),
            });
            t.push(ChatMessage::Assistant {
                content: None,
                reasoning_content: None,
                tool_calls: vec![RequestToolCall {
                    id: format!("c{n}"),
                    kind: "function".into(),
                    function: RequestFunction {
                        name: "read".into(),
                        arguments: "{}".into(),
                    },
                }],
            });
            t.push(ChatMessage::Tool {
                tool_call_id: format!("c{n}"),
                content: "output ".repeat(200),
                answered_by: None,
            });
            t.push(ChatMessage::Assistant {
                content: Some(format!("done {n}")),
                reasoning_content: None,
                tool_calls: vec![],
            });
        }
        t
    }

    fn compaction_through(
        through: usize,
        pruned: Vec<usize>,
    ) -> crate::agent::compaction::Compaction {
        crate::agent::compaction::Compaction {
            checkpoint: crate::agent::compaction::Checkpoint {
                through_message: through,
                summary: "the gist of the early work".into(),
                generation: 1,
            },
            pruned_message_indices: pruned,
        }
    }

    #[tokio::test]
    async fn a_resumed_checkpoint_sends_the_effective_prompt() {
        let done = r#"{"choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":"stop"}]}"#;
        let server = spawn_mock(vec![http_sse(&sse_body(&[done]))]).await;
        let mut actor = make_actor(server.url.clone(), registry_all(), CancellationToken::new());
        actor.transcript = history_of_turns(4);
        let before = actor.context_breakdown().total();
        actor.compaction = Some(compaction_through(7, vec![10]));
        let after = actor.context_breakdown().total();
        assert!(after < before, "{after} < {before}");

        actor.push_user("and now?");
        let mut noop = |_e: AgentEvent| {};
        run_agent(&mut actor, &mut noop).await.unwrap();

        let reqs = server.requests.lock().await;
        let body = reqs[0].split("\r\n\r\n").nth(1).unwrap();
        let v: serde_json::Value = serde_json::from_str(body).unwrap();
        let contents: Vec<String> = v["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["content"].as_str().unwrap_or("").to_string())
            .collect();
        let all = contents.join("\n");
        assert!(all.contains("the gist of the early work"));
        assert!(all.contains("<conversation_summary>"));
        assert!(!all.contains("request 0") && !all.contains("request 1"));
        assert!(all.contains("request 2") && all.contains("and now?"));
        // Message 10 is turn 2's output: its call stays, its text is a marker.
        assert!(all.contains("[output elided to save context: 1400 bytes]"));
        // Turn 3's output is whole, turn 2's is the marker.
        assert_eq!(all.matches(&"output ".repeat(200)).count(), 1);
        // The transcript itself is untouched.
        assert_eq!(actor.transcript.len(), 18);
    }

    #[tokio::test]
    async fn commit_persists_then_installs_and_a_failed_write_installs_nothing() {
        let mut actor = make_actor(
            "http://127.0.0.1:1/v1".into(),
            registry_all(),
            CancellationToken::new(),
        );
        actor.transcript = history_of_turns(4);
        let before = actor.context_breakdown();

        let refused = actor
            .commit_compaction(compaction_through(7, vec![]), |_, _| async {
                Err("disk full".to_string())
            })
            .await;
        assert_eq!(refused.unwrap_err(), "disk full");
        assert!(actor.compaction.is_none());
        assert_eq!(actor.context_breakdown(), before);

        let saved = std::sync::Mutex::new(None);
        actor
            .commit_compaction(compaction_through(3, vec![6]), |transcript, k| {
                *saved.lock().unwrap() = Some((transcript.len(), k));
                async { Ok(()) }
            })
            .await
            .unwrap();
        let (len, k) = saved.into_inner().unwrap().unwrap();
        assert_eq!(len, 16);
        assert_eq!(actor.compaction.as_ref(), Some(&k));
        assert!(actor.context_breakdown().total() < before.total());
        assert_eq!(actor.context_breakdown().system, before.system);

        // A compaction that does not fit the transcript is refused unwritten.
        let mut called = false;
        let bad = actor
            .commit_compaction(compaction_through(99, vec![]), |_, _| {
                called = true;
                async { Ok(()) }
            })
            .await;
        assert!(bad.is_err() && !called);
    }

    thread_local! {
        static BEFORE_DISPATCH: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
            const { std::cell::RefCell::new(None) };
    }

    /// Called by `execute_call` just before it re-checks the target: lets a
    /// test move a file in the gap between the gate and the dispatch.
    pub(super) fn before_dispatch() {
        if let Some(hook) = BEFORE_DISPATCH.with(|h| h.borrow_mut().take()) {
            hook();
        }
    }

    #[cfg(unix)]
    pub(super) fn on_next_dispatch(hook: impl FnOnce() + 'static) {
        BEFORE_DISPATCH.with(|h| *h.borrow_mut() = Some(Box::new(hook)));
    }

    #[tokio::test]
    async fn yolo_runs_review_and_hard_floor_calls_without_asking_anyone() {
        let mut actor = plan_actor("http://unused");
        let config = actor.tool_context.workspace.join("config");
        std::fs::create_dir(&config).unwrap();
        actor.permission_dirs.config = Some(config.clone());
        let protected = config.join("config.json");
        let outside = tempfile::tempdir().unwrap();
        let elsewhere = outside.path().join("x.txt");
        let calls = [
            ToolCall {
                id: "bash".into(),
                name: "bash".into(),
                args: serde_json::json!({"command":"echo yolo"}),
                args_valid: true,
            },
            ToolCall {
                id: "config".into(),
                name: "write".into(),
                args: serde_json::json!({"path":protected,"content":"changed"}),
                args_valid: true,
            },
            ToolCall {
                id: "outside".into(),
                name: "write".into(),
                args: serde_json::json!({"path":elsewhere,"content":"outside"}),
                args_valid: true,
            },
        ];
        // Off: the protected write asks the Captain and does not run.
        assert!(matches!(
            run_decided(&mut actor, &calls[1], &mut |_| panic!("must ask"), 1, None).await,
            One::Ask(request, _) if request.requires_captain
        ));
        assert!(!protected.exists(), "the protected file stays untouched");
        actor.yolo = true;
        for call in &calls {
            let mut approved = Vec::new();
            let mut pre = Vec::new();
            let outcome = run_decided(
                &mut actor,
                call,
                &mut |event| match event {
                    AgentEvent::PreCall {
                        permission_gate, ..
                    } => {
                        pre.push(permission_gate.cloned());
                    }
                    AgentEvent::PostCall { result, .. } => {
                        approved.push(result.metadata["approved_by"].clone());
                    }
                    _ => {}
                },
                1,
                None,
            )
            .await;
            assert!(matches!(outcome, One::Settled), "{}", call.id);
            assert_eq!(approved, [serde_json::json!("yolo")], "{}", call.id);
            assert_eq!(pre.len(), 1);
            assert_eq!(pre[0].as_ref().unwrap()["source"], "yolo");
        }
        assert_eq!(std::fs::read_to_string(&protected).unwrap(), "changed");
        assert_eq!(std::fs::read_to_string(&elsewhere).unwrap(), "outside");
    }

    #[tokio::test(start_paused = true)]
    async fn yolo_does_not_consult_tool_flows() {
        for mode in ["deny", "ask", "timeout"] {
            let mut actor = make_actor(String::new(), registry_all(), CancellationToken::new());
            actor.yolo = true;
            let path = actor.tool_context.workspace.join("file.txt");
            std::fs::write(&path, "hello").unwrap();
            let call = ToolCall {
                id: "flow-call".into(),
                name: "read".into(),
                args: serde_json::json!({"path":path}),
                args_valid: true,
            };
            let flows = Arc::new(super::permission_tests::FakeFlows::new(mode));
            actor.tool_flows = Some(flows.clone());
            let mut approved = Vec::new();
            let one = run_decided(
                &mut actor,
                &call,
                &mut |event| {
                    if let AgentEvent::PostCall { result, .. } = event {
                        approved.push(result.metadata["approved_by"].clone());
                    }
                },
                1,
                None,
            )
            .await;
            assert!(matches!(one, One::Settled), "{mode}");
            assert_eq!(approved, [serde_json::json!("yolo")], "{mode}");
            assert_eq!(flows.asked(), 0, "{mode}: flows must not be asked");
        }
    }

    #[tokio::test]
    async fn yolo_keeps_tool_availability_argument_checks_and_cancellation() {
        let mut actor = plan_actor("http://unused");
        actor.yolo = true;
        let missing = ToolCall {
            id: "m".into(),
            name: "write".into(),
            args: serde_json::json!({"content":"x"}),
            args_valid: true,
        };
        let absent = ToolCall {
            id: "a".into(),
            name: "no_such_tool".into(),
            args: serde_json::json!({}),
            args_valid: true,
        };
        for call in [&missing, &absent] {
            let mut pre = Vec::new();
            let mut post = Vec::new();
            let outcome = run_decided(
                &mut actor,
                call,
                &mut |event| match event {
                    AgentEvent::PreCall {
                        permission_gate, ..
                    } => pre.push(permission_gate.cloned()),
                    AgentEvent::PostCall { result, .. } => {
                        post.push((result.status, result.metadata.clone()))
                    }
                    _ => {}
                },
                1,
                None,
            )
            .await;
            assert!(matches!(outcome, One::Settled), "{}", call.id);
            // Refused, and the refusal carries the same yolo evidence as a run.
            assert_eq!(pre.len(), 1, "{}", call.id);
            assert_eq!(pre[0].as_ref().unwrap()["source"], "yolo", "{}", call.id);
            assert_eq!(post.len(), 1, "{}", call.id);
            assert_eq!(post[0].0, ToolStatus::Error, "{}", call.id);
            assert_eq!(post[0].1["approved_by"], "yolo", "{}", call.id);
            assert_eq!(
                post[0].1["permission_gate"]["source"], "yolo",
                "{}",
                call.id
            );
        }
        let call = ToolCall {
            id: "c".into(),
            name: "bash".into(),
            args: serde_json::json!({"command":"echo x"}),
            args_valid: true,
        };
        actor.cancel.cancel();
        assert!(matches!(
            run_decided(&mut actor, &call, &mut |_| panic!("cancelled"), 1, None).await,
            One::Interrupted
        ));
    }

    /// Runs `call` in yolo and returns the `approved_by` of its post-call.
    async fn yolo_run(actor: &mut AgentActor, call: &ToolCall) -> (ToolStatus, serde_json::Value) {
        let mut post = None;
        let outcome = run_decided(
            actor,
            call,
            &mut |event| {
                if let AgentEvent::PostCall { result, .. } = event {
                    post = Some((result.status, result.metadata["approved_by"].clone()));
                }
            },
            1,
            None,
        )
        .await;
        assert!(matches!(outcome, One::Settled), "{}", call.id);
        post.expect("post call")
    }

    #[tokio::test]
    async fn yolo_ignores_scratch_identity_and_mode() {
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;
        let mut actor = plan_actor("http://unused");
        actor.yolo = true;
        actor
            .registry
            .register(Box::new(crate::tools::native::EditTool));
        let root = actor.tool_context.workspace.join("scratch");
        actor
            .scratch_roots
            .push(crate::agent::scratch::Identity::capture(&root).unwrap());
        let write = |name: &str| ToolCall {
            id: format!("w-{name}"),
            name: "write".into(),
            args: serde_json::json!({"path":root.join(name),"content":"one"}),
            args_valid: true,
        };
        let edit = |name: &str| ToolCall {
            id: format!("e-{name}"),
            name: "edit".into(),
            args: serde_json::json!({"path":root.join(name),"old_string":"one","new_string":"two"}),
            args_valid: true,
        };
        // Widened mode: the identity would refuse it.
        #[cfg(unix)]
        {
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert!(!actor.scratch_roots[0].valid());
        }
        for call in [write("a"), edit("a")] {
            assert_eq!(
                yolo_run(&mut actor, &call).await,
                (ToolStatus::Ok, serde_json::json!("yolo")),
                "{}",
                call.id
            );
        }
        assert_eq!(std::fs::read_to_string(root.join("a")).unwrap(), "two");
        // Replaced root: another inode at the same path.
        std::fs::remove_dir_all(&root).unwrap();
        std::fs::create_dir(&root).unwrap();
        for call in [write("b"), edit("b")] {
            assert_eq!(
                yolo_run(&mut actor, &call).await,
                (ToolStatus::Ok, serde_json::json!("yolo")),
                "{}",
                call.id
            );
        }
        assert_eq!(std::fs::read_to_string(root.join("b")).unwrap(), "two");
    }

    #[tokio::test]
    async fn yolo_searches_a_sensitive_root_but_filters_its_credentials() {
        let mut actor = plan_actor("http://unused");
        let ssh = actor.tool_context.workspace.join(".ssh");
        std::fs::create_dir(&ssh).unwrap();
        std::fs::write(ssh.join("notes.txt"), "needle in notes\n").unwrap();
        std::fs::write(ssh.join("id_ed25519"), "needle in key\n").unwrap();
        let call = ToolCall {
            id: "g".into(),
            name: "grep".into(),
            args: serde_json::json!({"pattern":"needle","path":ssh}),
            args_valid: true,
        };
        let mut output = String::new();
        let mut observe = |event: AgentEvent| {
            if let AgentEvent::PostCall { result, .. } = event {
                output = result.output.clone();
            }
        };
        let asked = run_decided(&mut actor, &call, &mut observe, 1, None).await;
        assert!(matches!(asked, One::Ask(..)), "normal mode asks");
        assert!(!output.contains("needle in notes"));
        actor.yolo = true;
        let mut observe = |event: AgentEvent| {
            if let AgentEvent::PostCall { result, .. } = event {
                output = result.output.clone();
            }
        };
        assert!(matches!(
            run_decided(&mut actor, &call, &mut observe, 1, None).await,
            One::Settled
        ));
        assert!(output.contains("needle in notes"), "{output}");
        assert!(!output.contains("needle in key"));
    }

    #[tokio::test]
    async fn yolo_does_not_enforce_the_cartographe_write_scope() {
        let mut actor = plan_actor("http://unused");
        actor.role = Some(
            crate::agent::definitions::Agents::builtin()
                .get("cartographe")
                .unwrap()
                .clone(),
        );
        let target = actor.tool_context.workspace.join("src.rs");
        let call = ToolCall {
            id: "c".into(),
            name: "write".into(),
            args: serde_json::json!({"path":target,"content":"x"}),
            args_valid: true,
        };
        // Normal: the role's scope refuses it before anything runs.
        let mut refused = false;
        let _ = run_decided(
            &mut actor,
            &call,
            &mut |event| {
                if let AgentEvent::PostCall { result, .. } = event {
                    refused = result.status != ToolStatus::Ok;
                }
            },
            1,
            None,
        )
        .await;
        assert!(refused && !target.exists());
        actor.yolo = true;
        assert_eq!(
            yolo_run(&mut actor, &call).await,
            (ToolStatus::Ok, serde_json::json!("yolo"))
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "x");
    }

    fn as_plan(actor: &mut AgentActor) {
        actor.role = Some(
            crate::agent::definitions::Agents::builtin()
                .get("plan")
                .unwrap()
                .clone(),
        );
    }

    fn plan_actor(base_url: &str) -> AgentActor {
        let mut actor = make_actor(base_url.into(), registry_gated(), CancellationToken::new());
        as_plan(&mut actor);
        actor.tool_context.workspace = actor.tool_context.workspace.canonicalize().unwrap();
        actor.tool_context.writable_roots = vec![actor.tool_context.workspace.clone()];
        actor
    }

    #[cfg(unix)]
    fn write_script(path: &Path) -> Vec<Vec<u8>> {
        vec![
            http_sse(&sse_body(&[&calls_chunk(&[(
                "w1",
                "write",
                serde_json::json!({"path": path, "content": "x"}),
            )])])),
            http_sse(&sse_body(&[&text_chunk("done")])),
        ]
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_approved_write_swapped_for_a_link_after_the_final_check_asks_again() {
        let mut actor = make_actor(
            "http://127.0.0.1:1/v1".into(),
            registry_gated(),
            CancellationToken::new(),
        );
        as_build(&mut actor);
        let ws = actor.tool_context.workspace.canonicalize().unwrap();
        actor.tool_context.workspace = ws.clone();
        actor.tool_context.writable_roots = vec![ws.clone()];
        let (target, secret) = (ws.join("a.txt"), ws.join("secret.toml"));
        std::fs::write(&secret, "keep").unwrap();
        let server = spawn_mock(write_script(&target)).await;
        actor.provider.base_url = server.url.clone();
        actor.push_user("go");
        let mut noop = |_e: AgentEvent| {};
        let RunOutcome::PermissionRequired(first) = run_agent(&mut actor, &mut noop).await.unwrap()
        else {
            panic!("build asks first");
        };
        {
            let (target, secret) = (target.clone(), secret.clone());
            crate::tools::nofollow::on_next_open(move || {
                std::os::unix::fs::symlink(&secret, &target).unwrap();
            });
        }
        let outcome = resolve_permission(
            &mut actor,
            first.id,
            PermissionDecision::AllowOnce,
            &mut noop,
        )
        .await
        .unwrap();
        let RunOutcome::PermissionRequired(second) = outcome else {
            panic!("the approval was for the old destination: {outcome:?}");
        };
        assert_ne!(first.id, second.id);
        assert_eq!(std::fs::read_to_string(&secret).unwrap(), "keep");
        assert_eq!(tool_result_of(&actor, "w1"), None);
    }

    #[tokio::test]
    async fn a_model_cannot_bind_a_call_to_a_destination_itself() {
        let mut actor = make_actor(
            "http://127.0.0.1:1/v1".into(),
            registry_gated(),
            CancellationToken::new(),
        );
        let ws = actor.tool_context.workspace.canonicalize().unwrap();
        actor.tool_context.writable_roots = vec![ws.clone()];
        let target = ws.join("a.txt");
        let call = ToolCall {
            id: "w1".into(),
            name: "write".into(),
            args: serde_json::json!({
                "path": target, "content": "x", crate::tools::BOUND_ARG: true
            }),
            args_valid: true,
        };
        let mut noop = |_e: AgentEvent| {};
        // No destination from the loop: the argument is stripped, the plain
        // tool path runs.
        let DispatchOutcome::Settled(result) =
            execute_call(&actor, &call, None, None, &mut noop).await
        else {
            panic!("unexpected dispatch outcome")
        };
        assert_eq!(result.status, ToolStatus::Error);
        assert!(!target.exists());
    }
}
