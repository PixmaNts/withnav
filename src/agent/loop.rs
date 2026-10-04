use crate::agent::actor::{AgentActor, AgentState, PendingInteraction};
use crate::agent::context::Breakdown;
use crate::agent::guard::{self, CallFacts, GuardInput, Judged};
use crate::agent::permissions::{
    exempt_from_guard, is_guarded, needs_permission, still_resolves, Gate, Needed,
    PermissionDecision, PermissionError, PermissionRequest,
};
use crate::agent::prompt::{assistant_history_message, build_request};
use crate::agent::response::{HumanRequest, StepOutcome};
use crate::error::{AgentError, ProviderError, ToolError};
use crate::provider::client::ModelClient;
use crate::provider::openai::{ChatMessage, ClientCache};
use crate::provider::{AssistantMessage, StreamDelta, ToolCall};
use std::path::{Path, PathBuf};

use crate::tools::{ToolResult, ToolStatus};

pub const DEFAULT_MAX_STEPS: usize = 50;

pub fn max_steps() -> usize {
    std::env::var("NAVAI_MAX_STEPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_MAX_STEPS)
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
    let budget = actor.max_steps;
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
    actor.state = AgentState::Running;
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

        if !actor.has_pending() {
            if let Some(boundary) = actor.safe_boundary.clone() {
                let updated = match boundary(actor.transcript.clone(), actor.cancel.clone()).await {
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
                let Some(updated) = updated else {
                    actor.state = AgentState::Idle;
                    return Ok(RunOutcome::Interrupted);
                };
                actor.transcript = updated;
            }
        }

        if actor.cancel.is_cancelled() {
            actor.state = AgentState::Idle;
            return Ok(RunOutcome::Interrupted);
        }

        // The safe boundary before a request: every tool result of the last
        // step is in, nothing waits for a human. A prompt over the input
        // budget is compacted by the caller before it is sent.
        if actor
            .input_budget
            .is_some_and(|budget| actor.context_breakdown().total() > budget)
        {
            return Ok(RunOutcome::NeedsCompaction {
                steps_left: budget - step,
            });
        }

        observe(AgentEvent::StepStarted);

        let role_message = actor.role_message();
        let request = build_request(
            &actor.model,
            role_message.as_deref(),
            actor.rules.message(),
            actor.skills.catalog(),
            actor.effective_history(),
            actor.registry.schemas(),
            actor.max_tokens,
        );

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
            actor
                .registry
                .get(crate::agent::response::ASK_TOOL)
                .is_some(),
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
                        return Ok(RunOutcome::PermissionRequired(request))
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

/// Applies a driver's `decision` on the permission request `request_id`, then
/// carries the turn on, BEFORE any new model request:
///
/// * `AllowOnce` / `AllowFolder`: the held call runs exactly once and its real
///   result is appended (`AllowFolder` also records the grant first);
/// * `Deny`: nothing runs; a `denied` result with code `permission_denied`
///   and the driver's message is appended.
///
/// The calls that were behind it then go through the gate in order (any of
/// them may ask again: the outcome is then `PermissionRequired`), and once
/// all are settled the turn continues as [`run_agent`] does.
///
/// `request_id` must be the id of the request the actor waits on; a stale or
/// repeated id is [`PermissionError::Stale`] and changes nothing.
pub async fn resolve_permission(
    actor: &mut AgentActor,
    request_id: ulid::Ulid,
    decision: PermissionDecision,
    observe: &mut (dyn for<'e> FnMut(AgentEvent<'e>) + Send),
) -> Result<RunOutcome, ResolveError> {
    match &actor.pending {
        Some(PendingInteraction::Permission { request, .. }) if request.id == request_id => {}
        Some(PendingInteraction::Permission { .. }) | Some(PendingInteraction::Ask { .. }) => {
            return Err(PermissionError::Stale.into());
        }
        None => return Err(PermissionError::Stale.into()),
    }
    let Some(PendingInteraction::Permission {
        request,
        call,
        remaining_calls,
        destination,
        steps_left,
    }) = actor.pending.take()
    else {
        return Err(PermissionError::Stale.into());
    };
    actor.state = AgentState::Running;

    if actor.cancel.is_cancelled() {
        append_interrupted(actor, &call.id);
        for rest in &remaining_calls {
            append_interrupted(actor, &rest.id);
        }
        actor.state = AgentState::Idle;
        return Ok(RunOutcome::Interrupted);
    }
    match decision {
        PermissionDecision::AllowOnce | PermissionDecision::AllowFolder => {
            // The approval is for the destination the driver reviewed. Resolve
            // it again: a call that now lands elsewhere, or is refused, does
            // not run on this approval, and goes through the gate afresh.
            let one = match gate(actor, &call) {
                Gated::Ask(fresh, fresh_destination)
                    if fresh_destination == destination && fresh.scope == request.scope =>
                {
                    if decision == PermissionDecision::AllowFolder {
                        actor
                            .grants
                            .grant(&request.agent, &request.tool, request.scope.clone());
                    }
                    run_reviewed(actor, &call, destination.as_deref(), observe).await
                }
                // An exception of a guarded agent: the driver's approval is
                // for this one call and never becomes a grant, whatever the
                // decision said.
                Gated::Guarded(needed) | Gated::Reviewed(needed)
                    if needed.destination == destination && needed.scope == request.scope =>
                {
                    run_reviewed(actor, &call, destination.as_deref(), observe).await
                }
                _ => run_gated(actor, &call, observe).await,
            };
            match one {
                One::Settled => {}
                One::Ask(fresh, fresh_destination) => {
                    actor.pending = Some(PendingInteraction::Permission {
                        request: Box::new(fresh.clone()),
                        call,
                        remaining_calls,
                        destination: fresh_destination,
                        steps_left,
                    });
                    actor.state = AgentState::Yielded;
                    return Ok(RunOutcome::PermissionRequired(fresh));
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
        }
        PermissionDecision::Deny { message } => {
            append_tool_result(actor, &call.id, &ToolResult::permission_denied(message));
        }
    }
    match run_calls(actor, remaining_calls, steps_left, observe).await {
        Calls::Finished => {}
        Calls::Interrupted => return Ok(RunOutcome::Interrupted),
        Calls::Permission(request) => return Ok(RunOutcome::PermissionRequired(request)),
    }
    Ok(run_steps(actor, observe, steps_left).await?)
}

/// How a pass over a message's tool calls ended.
enum Calls {
    Finished,
    Interrupted,
    /// The call at hand needs a decision; the actor holds it, and the calls
    /// behind it, as its pending interaction.
    Permission(PermissionRequest),
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
    while let Some(call) = calls.pop_front() {
        if actor.cancel.is_cancelled() {
            append_interrupted(actor, &call.id);
            for rest in &calls {
                append_interrupted(actor, &rest.id);
            }
            actor.state = AgentState::Idle;
            return Calls::Interrupted;
        }
        match run_gated(actor, &call, observe).await {
            One::Settled => {}
            One::Ask(request, destination) => {
                actor.pending = Some(PendingInteraction::Permission {
                    request: Box::new(request.clone()),
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

/// What the gate decided about a call.
enum Gated {
    /// Runs now: no role, not gated, covered by a grant, or a guarded agent's
    /// markdown. The path is the canonical file a `write` or `edit` was
    /// checked for: the tool is handed it, and only if it still resolves
    /// there.
    Run(Option<PathBuf>),
    /// Refused before it runs; the string is the tool error.
    Rejected(String),
    /// Waits for a decision; the path is the canonical file a `write` or
    /// `edit` was resolved to.
    Ask(PermissionRequest, Option<PathBuf>),
    /// An exception of a guarded agent: the role guard is asked before the
    /// driver is. See [`guard_step`].
    Guarded(Needed),
    Reviewed(Needed),
}

fn gate(actor: &AgentActor, call: &ToolCall) -> Gated {
    if actor.registry.get(&call.name).is_none() {
        return Gated::Rejected(format!(
            "tool '{}' is disabled by this session's policy",
            call.name
        ));
    }
    let Some(role) = actor.role.as_deref() else {
        return Gated::Run(None);
    };
    let needed = match needs_permission(Some(role), &call.name, &call.args, &actor.tool_context) {
        Gate::Ungated => return Gated::Run(None),
        Gate::Rejected(message) => return Gated::Rejected(message),
        Gate::Needs(needed) => needed,
    };
    let agent = role.id.as_str();
    if matches!(call.name.as_str(), "bash" | "write" | "edit") {
        return Gated::Reviewed(needed);
    }
    // A guarded agent writes its markdown without asking; everything else it
    // does is an exception, and no folder grant covers an exception.
    if is_guarded(role) {
        if exempt_from_guard(&call.name, &call.args, &needed) {
            return Gated::Run(needed.destination);
        }
        return Gated::Guarded(needed);
    }
    if actor
        .grants
        .covers(agent, &call.name, &needed.scope, &actor.tool_context)
    {
        return Gated::Run(needed.destination);
    }
    Gated::Ask(
        PermissionRequest::new(call, needed.scope, agent),
        needed.destination,
    )
}

/// How one call came out of the gate and, when it was let through, its run.
enum One {
    /// Run, or refused: its result is in the transcript.
    Settled,
    /// The driver decides; the path is the canonical file the request is for.
    Ask(PermissionRequest, Option<PathBuf>),
    /// The session was stopped while the call was being checked.
    Interrupted,
}

/// How many times a call whose file moved under it is checked afresh before
/// it is refused: a file that keeps changing is not one to write.
const REGATE_LIMIT: usize = 3;

/// Takes `call` through the gate and runs it when it is let through. A call
/// whose file no longer resolves where it was checked (a link put in its
/// place, during the check or while Jev was asked) does not run: it goes
/// through the gate again, the guard first and the driver after.
async fn run_gated(
    actor: &mut AgentActor,
    call: &ToolCall,
    observe: &mut (dyn for<'e> FnMut(AgentEvent<'e>) + Send),
) -> One {
    for _ in 0..REGATE_LIMIT {
        let (destination, note) = match gate(actor, call) {
            Gated::Rejected(message) => {
                reject_call(actor, call, message, observe);
                return One::Settled;
            }
            Gated::Run(destination) => (destination, None),
            Gated::Reviewed(needed) => {
                match super::permission_gate::consult(actor, call, &needed).await {
                    super::permission_gate::Judged::Run(note) => (needed.destination, Some(note)),
                    super::permission_gate::Judged::Interrupted => return One::Interrupted,
                    super::permission_gate::Judged::Ask(reason) => {
                        let mut request = PermissionRequest::new(
                            call,
                            needed.scope,
                            actor
                                .role
                                .as_ref()
                                .map(|r| r.id.as_str())
                                .unwrap_or("build"),
                        );
                        request.reason = reason;
                        return One::Ask(request, needed.destination);
                    }
                }
            }
            Gated::Ask(request, destination) => return One::Ask(request, destination),
            Gated::Guarded(needed) => match guard_step(actor, call, needed).await {
                GuardStep::Run(note, destination) => (destination, Some(note)),
                GuardStep::Ask(request, destination) => return One::Ask(request, destination),
                GuardStep::Interrupted => return One::Interrupted,
            },
        };
        if let Some(result) = execute_call(actor, call, destination.as_deref(), note, observe).await
        {
            append_tool_result(actor, &call.id, &result);
            return One::Settled;
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

/// Runs a call the driver reviewed for `destination`; if that file moved, the
/// call is checked afresh instead.
async fn run_reviewed(
    actor: &mut AgentActor,
    call: &ToolCall,
    destination: Option<&Path>,
    observe: &mut (dyn for<'e> FnMut(AgentEvent<'e>) + Send),
) -> One {
    if let Some(result) = execute_call(actor, call, destination, None, observe).await {
        append_tool_result(actor, &call.id, &result);
        return One::Settled;
    }
    run_gated(actor, call, observe).await
}

/// What the role guard made of an exception.
enum GuardStep {
    /// Jev judged it within the role: it runs, with this note on its result,
    /// on the canonical destination that was judged.
    Run(serde_json::Value, Option<PathBuf>),
    /// The driver decides.
    Ask(PermissionRequest, Option<PathBuf>),
    /// The session was stopped while Jev was asked.
    Interrupted,
}

/// Asks the role guard about an exception of a guarded agent. Only a valid
/// `conforms` runs the call; everything else becomes a permission request
/// whose reason says why, offered once (an approval of an exception never
/// becomes a grant). The guard is advisory, not a sandbox.
async fn guard_step(actor: &AgentActor, call: &ToolCall, needed: Needed) -> GuardStep {
    let role = actor.role.as_deref();
    let role_id = role.map(|r| r.id.as_str()).unwrap_or("plan");
    let task = actor.driver_task.as_deref();
    let input = GuardInput::describe(&CallFacts {
        role_id,
        role_prompt: role.map(|r| r.prompt.as_str()).unwrap_or(""),
        task,
        workspace: &actor.tool_context.workspace,
        tool: &call.name,
        target: needed.destination.as_deref(),
        arguments: &call.args,
    });
    match guard::consult(actor.guard.as_ref(), input, role_id, &actor.cancel).await {
        Judged::Allowed(note) => GuardStep::Run(note, needed.destination),
        Judged::Interrupted => GuardStep::Interrupted,
        Judged::AskDriver(why) => {
            let mut request = PermissionRequest::new(call, needed.scope, role_id);
            request.reason = format!(
                "{why}. {} (approve this call once, or deny)",
                request.reason
            );
            GuardStep::Ask(request, needed.destination)
        }
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
    observe(AgentEvent::PreCall {
        permission_gate: None,
        tool_call_id: &call.id,
        name: &call.name,
        args: &call.args,
    });
    let result = ToolResult::error("tool_error", message);
    observe(AgentEvent::PostCall {
        tool_call_id: &call.id,
        name: &call.name,
        result: &result,
    });
    append_tool_result(actor, &call.id, &result);
}

/// Dispatches `call`. With a `destination` (the canonical file a `write` or
/// `edit` was checked or approved for) the tool is handed that path, so the
/// file written is the one reviewed, and only if the call's own path still
/// resolves there and the file is not a link now. `None`: it moved, nothing
/// ran and nothing was observed.
async fn execute_call(
    actor: &AgentActor,
    call: &ToolCall,
    destination: Option<&Path>,
    guard_note: Option<serde_json::Value>,
    observe: &mut (dyn for<'e> FnMut(AgentEvent<'e>) + Send),
) -> Option<ToolResult> {
    // The seam tests use to move a file between the check and the dispatch.
    #[cfg(test)]
    tests::before_dispatch();
    if !still_resolves(&call.args, destination, &actor.tool_context) {
        return None;
    }
    observe(AgentEvent::PreCall {
        permission_gate: guard_note
            .as_ref()
            .and_then(|note| note.get("permission_gate")),
        tool_call_id: &call.id,
        name: &call.name,
        args: &call.args,
    });
    let mut args = call.args.clone();
    if let Some(object) = args.as_object_mut() {
        // Only the loop binds a call to a destination, never the model.
        object.remove(crate::tools::BOUND_ARG);
        if let Some(destination) = destination {
            object.insert(
                "path".into(),
                serde_json::Value::String(destination.to_string_lossy().into_owned()),
            );
            object.insert(
                crate::tools::BOUND_ARG.into(),
                serde_json::Value::Bool(true),
            );
        }
    }
    let dispatched = actor
        .registry
        .dispatch(
            &call.name,
            args,
            actor.cancel.child_token(),
            &actor.tool_context,
        )
        .await;
    let mut result = match dispatched {
        // The tool opened the reviewed file without following links and found
        // one (or another kind of file) there: nothing was touched, and the
        // call is checked again. The attempt stays visible, closed as an error.
        Err(ToolError::DestinationChanged(path)) => {
            observe(AgentEvent::PostCall {
                tool_call_id: &call.id,
                name: &call.name,
                result: &ToolResult::error(
                    "destination_changed",
                    format!(
                        "{} changed after it was checked; nothing was written, the call is checked again",
                        path.display()
                    ),
                ),
            });
            return None;
        }
        Ok(result) => result,
        Err(e) => ToolResult::error("tool_error", e.to_string()),
    };
    // The role guard let this call run: the durable `tool.post_call` says so.
    if let Some(note) = guard_note {
        let (key, note) = if let Some(evidence) = note.get("permission_gate") {
            ("permission_gate", evidence.clone())
        } else {
            ("guard", note)
        };
        match &mut result.metadata {
            serde_json::Value::Object(map) => {
                map.insert(key.into(), note);
            }
            other => *other = serde_json::json!({ key: note }),
        }
    }
    observe(AgentEvent::PostCall {
        tool_call_id: &call.id,
        name: &call.name,
        result: &result,
    });
    Some(result)
}

fn append_tool_result(actor: &mut AgentActor, tool_call_id: &str, result: &ToolResult) {
    let content = match result.status {
        ToolStatus::Ok | ToolStatus::Interrupted => result.output.clone(),
        _ => serde_json::to_string(result).unwrap_or_else(|_| result.output.clone()),
    };
    actor.transcript.push(ChatMessage::Tool {
        tool_call_id: tool_call_id.to_string(),
        content,
    });
}

fn append_interrupted(actor: &mut AgentActor, tool_call_id: &str) {
    append_tool_result(actor, tool_call_id, &ToolResult::interrupted());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::actor::AgentActor;
    use crate::agent::permissions::Scope;
    use crate::provider::config::{ProviderConfig, ReasoningHistory};
    use crate::provider::test_support::{http_sse, sse_body};
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

    struct MockServer {
        url: String,
        requests: Arc<Mutex<Vec<String>>>,
    }

    async fn spawn_mock(responses: Vec<Vec<u8>>) -> MockServer {
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

    fn make_actor(base_url: String, registry: Registry, cancel: CancellationToken) -> AgentActor {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().to_path_buf();
        std::mem::forget(tmp);
        AgentActor::new(
            registry,
            ToolContext {
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
        )
    }

    fn registry_all() -> Registry {
        let mut r = Registry::new();
        r.register(Box::new(ReadTool));
        r.register(Box::new(BashTool));
        r.register(Box::new(AskTool));
        r
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
        actor.push_user("run echo hi");
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
        let toolcall = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"bash","arguments":"{\"command\":\"true\"}"}}]},"finish_reason":"tool_calls"}]}"#;
        let server = spawn_mock(vec![
            http_sse(&sse_body(&[toolcall])),
            http_sse(&sse_body(&[toolcall])),
        ])
        .await;
        let cancel = CancellationToken::new();
        let mut actor = make_actor(server.url.clone(), registry_all(), cancel);
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
        assert!(matches!(
            actor.transcript.last(),
            Some(ChatMessage::Tool { .. })
        ));

        actor.input_budget = None;
        let outcome = continue_run(&mut actor, &mut noop, 4).await.unwrap();
        assert!(matches!(outcome, RunOutcome::Done(m) if m.text == "fin"));
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
    fn calls_chunk(calls: &[(&str, &str, serde_json::Value)]) -> String {
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

    fn text_chunk(text: &str) -> String {
        serde_json::json!({"choices": [{
            "index": 0,
            "delta": {"content": text},
            "finish_reason": "stop",
        }]})
        .to_string()
    }

    fn registry_gated() -> Registry {
        let mut r = registry_all();
        r.register(Box::new(WriteTool));
        r.register(Box::new(GrepTool));
        r
    }

    fn as_build(actor: &mut AgentActor) {
        actor.role = Some(
            crate::agent::definitions::Agents::builtin()
                .get("build")
                .unwrap()
                .clone(),
        );
    }

    fn tool_result_of(actor: &AgentActor, id: &str) -> Option<String> {
        actor.transcript.iter().find_map(|m| match m {
            ChatMessage::Tool {
                tool_call_id,
                content,
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
    async fn a_bare_runtime_without_a_role_gates_nothing() {
        let mut actor = make_actor(
            "http://127.0.0.1:1/v1".into(),
            registry_gated(),
            CancellationToken::new(),
        );
        let target = actor.tool_context.workspace.join("c.txt");
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
        assert!(matches!(outcome, RunOutcome::Done(_)));
        assert!(target.exists());
    }

    #[tokio::test]
    async fn a_saved_folder_grant_never_bypasses_review() {
        let mut actor = make_actor(
            "http://127.0.0.1:1/v1".into(),
            registry_gated(),
            CancellationToken::new(),
        );
        as_build(&mut actor);
        let ws = actor.tool_context.workspace.canonicalize().unwrap();
        actor.grants.grant(
            "build",
            "write",
            crate::agent::permissions::Scope::Folder(ws.clone()),
        );
        let target = ws.join("sub").join("d.txt");
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
        assert!(!target.exists());
        assert!(actor.has_pending());
    }

    /// A build actor with a scripted provider, yielded on a pending `write`
    /// of `a.txt` followed by a second `write` of `b.txt` in the same message.
    /// Returns the two targets and the mock server.
    async fn yielded_on_two_writes(
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

    fn pending_id(actor: &AgentActor) -> ulid::Ulid {
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
    async fn deny_appends_a_denied_result_and_goes_on_with_the_siblings() {
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
        // The sibling is gated on its own and asks.
        let RunOutcome::PermissionRequired(second) = outcome else {
            panic!("the sibling write must ask");
        };
        assert!(!a.exists(), "a denied call never runs");
        let denied: serde_json::Value =
            serde_json::from_str(&tool_result_of(&actor, "w1").unwrap()).unwrap();
        assert_eq!(denied["status"], "denied");
        assert_eq!(denied["error"]["code"], "permission_denied");
        assert_eq!(denied["error"]["message"], "not in this repo");

        let outcome = resolve_permission(
            &mut actor,
            second.id,
            PermissionDecision::Deny {
                message: "no".into(),
            },
            &mut noop,
        )
        .await
        .unwrap();
        assert!(matches!(outcome, RunOutcome::Done(m) if m.text == "understood"));
    }

    #[tokio::test]
    async fn a_stale_wrong_or_repeated_request_id_is_rejected_and_changes_nothing() {
        let done = http_sse(&sse_body(&[&text_chunk("ok")]));
        let (mut actor, a, _b, _server) = yielded_on_two_writes(vec![done]).await;
        let id = pending_id(&actor);
        let before = actor.transcript.len();
        let mut noop = |_e: AgentEvent| {};

        let wrong = ulid::Ulid::new();
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
            ulid::Ulid::new(),
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
            ulid::Ulid::new(),
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
        assert_eq!(tool_result_of(&actor, "w1").unwrap(), "");
        assert_eq!(tool_result_of(&actor, "w2").unwrap(), "");
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
        assert!(tool_result_of(&actor, "w1").is_some());
        assert!(tool_result_of(&actor, "w2").is_some());
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
        assert!(matches!(outcome, RunOutcome::Done(_)));
        assert!(!actor.has_pending());
        assert!(!outside.path().join("new").exists(), "nothing was created");
        let result: serde_json::Value =
            serde_json::from_str(&tool_result_of(&actor, "w1").unwrap()).unwrap();
        assert_eq!(result["status"], "error");
    }

    #[tokio::test]
    async fn a_symlink_retargeted_while_approval_waits_needs_a_fresh_approval() {
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
            PermissionDecision::AllowFolder,
            &mut noop,
        )
        .await
        .unwrap();
        let RunOutcome::PermissionRequired(second) = outcome else {
            panic!("a retargeted destination must ask again");
        };
        assert_ne!(second.id, first.id);
        assert_eq!(second.scope, Scope::Folder(config.clone()));
        assert!(!config.join("f.txt").exists());
        assert!(!reviewed.join("f.txt").exists());
        assert_eq!(tool_result_of(&actor, "w1"), None);
        assert!(
            !actor.grants.covers(
                "build",
                "write",
                &Scope::Folder(reviewed.clone()),
                &actor.tool_context
            ),
            "a request that did not run leaves no grant"
        );

        // Approving the fresh request runs it, on the destination it names.
        resolve_permission(
            &mut actor,
            second.id,
            PermissionDecision::AllowOnce,
            &mut noop,
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(config.join("f.txt")).unwrap(), "x");
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

    // --- the role guard of the plan agent ---

    use crate::agent::guard::{GuardError, GuardInput, RoleGuard, Verdict};

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

    fn on_next_dispatch(hook: impl FnOnce() + 'static) {
        BEFORE_DISPATCH.with(|h| *h.borrow_mut() = Some(Box::new(hook)));
    }

    /// A guard that answers what it was told and counts its questions.
    struct FakeJev {
        answer: Result<Verdict, GuardError>,
        asked: std::sync::atomic::AtomicUsize,
        /// The description of every call it was asked about.
        seen: std::sync::Mutex<Vec<String>>,
    }

    impl FakeJev {
        fn new(answer: Result<Verdict, GuardError>) -> Arc<Self> {
            Arc::new(Self {
                answer,
                asked: std::sync::atomic::AtomicUsize::new(0),
                seen: std::sync::Mutex::new(Vec::new()),
            })
        }

        fn asked(&self) -> usize {
            self.asked.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl RoleGuard for FakeJev {
        fn judge<'a>(
            &'a self,
            input: &'a GuardInput,
            _cancel: &'a CancellationToken,
        ) -> futures::future::BoxFuture<'a, Result<Verdict, GuardError>> {
            self.asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.seen.lock().unwrap().push(input.state.clone());
            let answer = self.answer.clone();
            Box::pin(async move { answer })
        }
    }

    fn safety_answers(
        input: &super::super::permission_gate::PermissionReviewInput,
        verdict: Verdict,
    ) -> crate::provider::systemone::Answers {
        input
            .questions
            .keys()
            .map(|name| {
                let choice = match name.as_str() {
                    "call_effect" => "read_write",
                    "call_scope" => "inside",
                    "call_risk" => "low",
                    _ => verdict.as_str(),
                };
                (
                    name.clone(),
                    crate::provider::systemone::Answer::Choice {
                        choice: choice.into(),
                        probabilities: Default::default(),
                        confidence: 1.0,
                    },
                )
            })
            .collect()
    }

    impl super::super::permission_gate::PermissionJudge for FakeJev {
        fn judge<'a>(
            &'a self,
            input: &'a super::super::permission_gate::PermissionReviewInput,
            _cancel: &'a CancellationToken,
        ) -> futures::future::BoxFuture<'a, Result<crate::provider::systemone::Answers, String>>
        {
            self.asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.seen.lock().unwrap().push(input.state.clone());
            let answer = self
                .answer
                .clone()
                .map(|verdict| safety_answers(input, verdict))
                .map_err(|_| "unavailable".into());
            Box::pin(async move { answer })
        }
    }

    fn as_plan(actor: &mut AgentActor, guard: Option<Arc<FakeJev>>) {
        actor.role = Some(
            crate::agent::definitions::Agents::builtin()
                .get("plan")
                .unwrap()
                .clone(),
        );
        actor.permission_judge = guard
            .clone()
            .map(|g| g as Arc<dyn super::super::permission_gate::PermissionJudge>);
        actor.guard = guard.map(|g| g as Arc<dyn RoleGuard>);
    }

    fn plan_actor(guard: Option<Arc<FakeJev>>, base_url: &str) -> AgentActor {
        let mut actor = make_actor(base_url.into(), registry_gated(), CancellationToken::new());
        as_plan(&mut actor, guard);
        actor.tool_context.workspace = actor.tool_context.workspace.canonicalize().unwrap();
        actor.tool_context.writable_roots = vec![actor.tool_context.workspace.clone()];
        actor
    }

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

    /// The `guard` note of every post-call the observer saw.
    #[derive(Default)]
    struct Notes(Vec<serde_json::Value>);

    async fn run_plan(actor: &mut AgentActor, notes: &mut Notes) -> Result<RunOutcome, AgentError> {
        actor.push_user("plan it");
        actor.driver_task = Some("plan it".to_string());
        let mut observe = |event: AgentEvent| {
            if let AgentEvent::PostCall { result, .. } = event {
                notes.0.push(result.metadata["permission_gate"].clone());
            }
        };
        run_agent(actor, &mut observe).await
    }

    #[tokio::test]
    async fn plan_markdown_is_safety_reviewed_without_a_role_question() {
        let jev = FakeJev::new(Ok(Verdict::Violates));
        let mut actor = plan_actor(Some(jev.clone()), "http://127.0.0.1:1/v1");
        let target = actor.tool_context.workspace.join("notes.md");
        let server = spawn_mock(write_script(&target)).await;
        actor.provider.base_url = server.url.clone();
        let outcome = run_plan(&mut actor, &mut Notes::default()).await.unwrap();
        assert!(matches!(outcome, RunOutcome::Done(_)), "{outcome:?}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "x");
        assert_eq!(jev.asked(), 1, "markdown still needs safety review");
        assert!(!jev.seen.lock().unwrap()[0].contains("plan only"));
    }

    #[tokio::test]
    async fn a_conforming_exception_runs_and_leaves_a_note_on_the_result() {
        let jev = FakeJev::new(Ok(Verdict::Conforms));
        let mut actor = plan_actor(Some(jev.clone()), "http://127.0.0.1:1/v1");
        let target = actor.tool_context.workspace.join("src_main.rs");
        let server = spawn_mock(write_script(&target)).await;
        actor.provider.base_url = server.url.clone();
        let mut notes = Notes::default();
        let outcome = run_plan(&mut actor, &mut notes).await.unwrap();
        assert!(matches!(outcome, RunOutcome::Done(_)), "{outcome:?}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "x");
        assert_eq!(jev.asked(), 1);
        assert_eq!(
            notes.0,
            vec![serde_json::json!({"policy_version": 1, "source": "jev"})]
        );
    }

    #[tokio::test]
    async fn anything_but_a_valid_conforms_asks_the_driver_with_the_reason() {
        let cases: [(Option<Result<Verdict, GuardError>>, &str); 6] = [
            (Some(Ok(Verdict::Violates)), "uncertain"),
            (Some(Ok(Verdict::Uncertain)), "uncertain"),
            (Some(Err(GuardError::TimedOut)), "unavailable"),
            (
                Some(Err(GuardError::Unavailable("no key".into()))),
                "unavailable",
            ),
            (
                Some(Err(GuardError::Malformed("maybe".into()))),
                "unavailable",
            ),
            (None, "Jev unavailable"),
        ];
        for (answer, reason) in cases {
            let jev = answer.map(FakeJev::new);
            let mut actor = plan_actor(jev.clone(), "http://127.0.0.1:1/v1");
            let target = actor.tool_context.workspace.join("main.rs");
            let server = spawn_mock(write_script(&target)).await;
            actor.provider.base_url = server.url.clone();
            let outcome = run_plan(&mut actor, &mut Notes::default()).await.unwrap();
            let RunOutcome::PermissionRequired(request) = outcome else {
                panic!("{reason}: expected a request, got {outcome:?}");
            };
            assert!(request.reason.contains(reason), "{}", request.reason);
            assert_eq!(request.agent, "plan");
            assert!(!target.exists(), "{reason}: nothing ran");
            assert_eq!(tool_result_of(&actor, "w1"), None);
        }
    }

    #[tokio::test]
    async fn an_oversized_call_goes_to_the_driver_and_jev_is_never_asked() {
        let jev = FakeJev::new(Ok(Verdict::Conforms));
        let mut actor = plan_actor(Some(jev.clone()), "http://127.0.0.1:1/v1");
        let target = actor.tool_context.workspace.join("big.rs");
        let big = "x".repeat(crate::agent::guard::MAX_INPUT_BYTES);
        let server = spawn_mock(vec![http_sse(&sse_body(&[&calls_chunk(&[(
            "w1",
            "write",
            serde_json::json!({"path": target, "content": big}),
        )])]))])
        .await;
        actor.provider.base_url = server.url.clone();
        let outcome = run_plan(&mut actor, &mut Notes::default()).await.unwrap();
        let RunOutcome::PermissionRequired(request) = outcome else {
            panic!("expected a request, got {outcome:?}");
        };
        assert!(request.reason.contains("oversized"), "{}", request.reason);
        assert_eq!(jev.asked(), 0);
        assert!(!target.exists());
    }

    #[tokio::test]
    async fn bash_and_fetch_are_always_exceptions_of_plan() {
        for (tool, args) in [
            ("bash", serde_json::json!({"command": "echo hi"})),
            ("fetch", serde_json::json!({"url": "https://example.com/"})),
        ] {
            let jev = FakeJev::new(Ok(Verdict::Violates));
            let mut actor = plan_actor(Some(jev.clone()), "http://127.0.0.1:1/v1");
            actor
                .registry
                .register(Box::new(crate::tools::fetch::FetchTool::new()));
            let server = spawn_mock(vec![http_sse(&sse_body(&[&calls_chunk(&[(
                "c1", tool, args,
            )])]))])
            .await;
            actor.provider.base_url = server.url.clone();
            let outcome = run_plan(&mut actor, &mut Notes::default()).await.unwrap();
            assert!(
                matches!(outcome, RunOutcome::PermissionRequired(_)),
                "{tool}: {outcome:?}"
            );
            assert_eq!(jev.asked(), 1, "{tool}");
        }
    }

    #[tokio::test]
    async fn a_markdown_name_that_is_a_link_to_another_file_is_guarded() {
        let jev = FakeJev::new(Ok(Verdict::Violates));
        let mut actor = plan_actor(Some(jev.clone()), "http://127.0.0.1:1/v1");
        let ws = actor.tool_context.workspace.clone();
        let code = ws.join("main.rs");
        std::fs::write(&code, "fn main() {}").unwrap();
        let link = ws.join("plan.md");
        std::os::unix::fs::symlink(&code, &link).unwrap();
        let server = spawn_mock(write_script(&link)).await;
        actor.provider.base_url = server.url.clone();
        let outcome = run_plan(&mut actor, &mut Notes::default()).await.unwrap();
        let RunOutcome::PermissionRequired(request) = outcome else {
            panic!("expected a request, got {outcome:?}");
        };
        assert_eq!(jev.asked(), 1, "the link is not the plan's markdown");
        assert_eq!(
            std::fs::read_to_string(&code).unwrap(),
            "fn main() {}",
            "nothing was written through the link"
        );
        assert_eq!(request.tool, "write");
    }

    #[tokio::test]
    async fn allow_folder_on_a_plan_exception_is_one_approval_not_a_grant() {
        let jev = FakeJev::new(Ok(Verdict::Violates));
        let mut actor = plan_actor(Some(jev.clone()), "http://127.0.0.1:1/v1");
        let ws = actor.tool_context.workspace.clone();
        let (first, second) = (ws.join("a.rs"), ws.join("b.rs"));
        let server = spawn_mock(vec![
            http_sse(&sse_body(&[&calls_chunk(&[
                (
                    "w1",
                    "write",
                    serde_json::json!({"path": first, "content": "1"}),
                ),
                (
                    "w2",
                    "write",
                    serde_json::json!({"path": second, "content": "2"}),
                ),
            ])])),
            http_sse(&sse_body(&[&text_chunk("done")])),
        ])
        .await;
        actor.provider.base_url = server.url.clone();
        let mut noop = |_e: AgentEvent| {};
        actor.push_user("plan it");
        let RunOutcome::PermissionRequired(one) = run_agent(&mut actor, &mut noop).await.unwrap()
        else {
            panic!("expected a request");
        };
        let outcome = resolve_permission(
            &mut actor,
            one.id,
            PermissionDecision::AllowFolder,
            &mut noop,
        )
        .await
        .unwrap();
        let RunOutcome::PermissionRequired(two) = outcome else {
            panic!("the sibling in the same folder asks again: {outcome:?}");
        };
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "1");
        assert!(!second.exists());
        assert_eq!(jev.asked(), 2);
        assert!(
            !actor
                .grants
                .covers("plan", "write", &two.scope, &actor.tool_context),
            "no grant was recorded"
        );
    }

    #[tokio::test]
    async fn a_stop_while_jev_is_asked_interrupts_the_call() {
        struct Hangs;
        impl RoleGuard for Hangs {
            fn judge<'a>(
                &'a self,
                _input: &'a GuardInput,
                cancel: &'a CancellationToken,
            ) -> futures::future::BoxFuture<'a, Result<Verdict, GuardError>> {
                Box::pin(async move {
                    cancel.cancelled().await;
                    Err(GuardError::Cancelled)
                })
            }
        }
        impl super::super::permission_gate::PermissionJudge for Hangs {
            fn judge<'a>(
                &'a self,
                _input: &'a super::super::permission_gate::PermissionReviewInput,
                cancel: &'a CancellationToken,
            ) -> futures::future::BoxFuture<'a, Result<crate::provider::systemone::Answers, String>>
            {
                Box::pin(async move {
                    cancel.cancelled().await;
                    Err("cancelled".into())
                })
            }
        }
        let mut actor = plan_actor(None, "http://127.0.0.1:1/v1");
        actor.guard = Some(Arc::new(Hangs));
        actor.permission_judge = Some(Arc::new(Hangs));
        let target = actor.tool_context.workspace.join("main.rs");
        let server = spawn_mock(write_script(&target)).await;
        actor.provider.base_url = server.url.clone();
        let cancel = actor.cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            cancel.cancel();
        });
        let outcome = run_plan(&mut actor, &mut Notes::default()).await.unwrap();
        assert!(matches!(outcome, RunOutcome::Interrupted), "{outcome:?}");
        assert!(!target.exists());
        assert!(
            tool_result_of(&actor, "w1").is_some(),
            "the call is settled"
        );
    }

    #[tokio::test]
    async fn the_build_agent_never_meets_the_guard() {
        let jev = FakeJev::new(Ok(Verdict::Violates));
        let mut actor = make_actor(
            "http://127.0.0.1:1/v1".into(),
            registry_gated(),
            CancellationToken::new(),
        );
        as_build(&mut actor);
        actor.guard = Some(jev.clone() as Arc<dyn RoleGuard>);
        let target = actor.tool_context.workspace.join("main.rs");
        let server = spawn_mock(write_script(&target)).await;
        actor.provider.base_url = server.url.clone();
        let mut noop = |_e: AgentEvent| {};
        actor.push_user("go");
        let RunOutcome::PermissionRequired(request) =
            run_agent(&mut actor, &mut noop).await.unwrap()
        else {
            panic!("build asks the driver");
        };
        assert!(
            request.reason.contains("Jev unavailable"),
            "{}",
            request.reason
        );
        assert_eq!(jev.asked(), 0);
        resolve_permission(
            &mut actor,
            request.id,
            PermissionDecision::AllowFolder,
            &mut noop,
        )
        .await
        .unwrap();
        assert!(!actor
            .grants
            .covers("build", "write", &request.scope, &actor.tool_context));
    }

    /// A guard that answers from a script and runs `during` while "asking".
    struct ScriptedJev {
        answers: std::sync::Mutex<Vec<Verdict>>,
        during: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
        asked: std::sync::atomic::AtomicUsize,
    }

    impl RoleGuard for ScriptedJev {
        fn judge<'a>(
            &'a self,
            _input: &'a GuardInput,
            _cancel: &'a CancellationToken,
        ) -> futures::future::BoxFuture<'a, Result<Verdict, GuardError>> {
            self.asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let Some(during) = self.during.lock().unwrap().take() {
                during();
            }
            let verdict = self.answers.lock().unwrap().remove(0);
            Box::pin(async move { Ok(verdict) })
        }
    }
    impl super::super::permission_gate::PermissionJudge for ScriptedJev {
        fn judge<'a>(
            &'a self,
            input: &'a super::super::permission_gate::PermissionReviewInput,
            _cancel: &'a CancellationToken,
        ) -> futures::future::BoxFuture<'a, Result<crate::provider::systemone::Answers, String>>
        {
            self.asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if let Some(during) = self.during.lock().unwrap().take() {
                during();
            }
            let verdict = self.answers.lock().unwrap().remove(0);
            let answers = safety_answers(input, verdict);
            Box::pin(async move { Ok(answers) })
        }
    }

    #[tokio::test]
    async fn markdown_swapped_for_a_link_to_code_between_check_and_dispatch_is_gated_again() {
        let jev = FakeJev::new(Ok(Verdict::Violates));
        let mut actor = plan_actor(Some(jev.clone()), "http://127.0.0.1:1/v1");
        let ws = actor.tool_context.workspace.clone();
        let (notes, code) = (ws.join("notes.md"), ws.join("main.rs"));
        std::fs::write(&notes, "old notes").unwrap();
        std::fs::write(&code, "fn main() {}").unwrap();
        let server = spawn_mock(write_script(&notes)).await;
        actor.provider.base_url = server.url.clone();
        {
            let (notes, code) = (notes.clone(), code.clone());
            on_next_dispatch(move || {
                std::fs::remove_file(&notes).unwrap();
                std::os::unix::fs::symlink(&code, &notes).unwrap();
            });
        }
        let outcome = run_plan(&mut actor, &mut Notes::default()).await.unwrap();
        let RunOutcome::PermissionRequired(request) = outcome else {
            panic!("a moved target must be checked again, got {outcome:?}");
        };
        assert_eq!(
            jev.asked(),
            2,
            "safety and then the new target were reviewed"
        );
        assert_eq!(
            request.scope,
            Scope::Folder(ws.clone()),
            "the driver is asked about the link's target"
        );
        assert_eq!(
            std::fs::read_to_string(&code).unwrap(),
            "fn main() {}",
            "no code was changed"
        );
        assert_eq!(tool_result_of(&actor, "w1"), None);
    }

    #[tokio::test]
    async fn a_target_swapped_while_jev_is_asked_is_not_written_on_the_old_verdict() {
        let mut actor = plan_actor(None, "http://127.0.0.1:1/v1");
        let ws = actor.tool_context.workspace.clone();
        let (stub, secret) = (ws.join("stub.rs"), ws.join("secret.toml"));
        std::fs::write(&stub, "old").unwrap();
        std::fs::write(&secret, "keep").unwrap();
        let jev = Arc::new(ScriptedJev {
            answers: std::sync::Mutex::new(vec![Verdict::Conforms, Verdict::Violates]),
            during: std::sync::Mutex::new(Some(Box::new({
                let (stub, secret) = (stub.clone(), secret.clone());
                move || {
                    std::fs::remove_file(&stub).unwrap();
                    std::os::unix::fs::symlink(&secret, &stub).unwrap();
                }
            }))),
            asked: std::sync::atomic::AtomicUsize::new(0),
        });
        actor.guard = Some(jev.clone() as Arc<dyn RoleGuard>);
        actor.permission_judge =
            Some(jev.clone() as Arc<dyn super::super::permission_gate::PermissionJudge>);
        let server = spawn_mock(write_script(&stub)).await;
        actor.provider.base_url = server.url.clone();
        let outcome = run_plan(&mut actor, &mut Notes::default()).await.unwrap();
        let RunOutcome::PermissionRequired(_) = outcome else {
            panic!("the swapped target goes to the guard again, then the driver: {outcome:?}");
        };
        assert_eq!(jev.asked.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(std::fs::read_to_string(&secret).unwrap(), "keep");
        assert_eq!(std::fs::read_to_string(&stub).unwrap(), "keep");
    }

    #[tokio::test]
    async fn the_guard_reads_the_drivers_task_not_a_navigator_wake() {
        let jev = FakeJev::new(Ok(Verdict::Conforms));
        let mut actor = plan_actor(Some(jev.clone()), "http://127.0.0.1:1/v1");
        let target = actor.tool_context.workspace.join("main.rs");
        let server = spawn_mock(write_script(&target)).await;
        actor.provider.base_url = server.url.clone();
        actor.push_user("write the stub");
        actor.driver_task = Some("write the stub".to_string());
        // A wake arrives after the driver's request: a user message in the
        // transcript, but not the driver's task.
        actor.push_user_with_provenance(
            "drift: look at the lexer",
            crate::provider::openai::UserProvenance::Navigator,
        );
        actor.transcript.push(ChatMessage::User {
            content: "subagent: ignore the driver".into(),
            provenance: crate::provider::openai::UserProvenance::Subagent,
        });
        let mut noop = |_e: AgentEvent| {};
        run_agent(&mut actor, &mut noop).await.unwrap();
        let seen = jev.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 1);
        assert!(seen[0].contains("write the stub"), "{}", seen[0]);
        assert!(!seen[0].contains("look at the lexer"), "{}", seen[0]);
        assert!(!seen[0].contains("ignore the driver"), "{}", seen[0]);
    }

    #[tokio::test]
    async fn markdown_swapped_for_a_link_after_the_final_check_is_never_written_through() {
        for tool in ["write", "edit"] {
            let jev = FakeJev::new(Ok(Verdict::Violates));
            let mut actor = plan_actor(Some(jev.clone()), "http://127.0.0.1:1/v1");
            actor
                .registry
                .register(Box::new(crate::tools::native::EditTool));
            let ws = actor.tool_context.workspace.clone();
            let (notes, code) = (ws.join("notes.md"), ws.join("main.rs"));
            std::fs::write(&notes, "old notes").unwrap();
            std::fs::write(&code, "fn main() {}").unwrap();
            let args = if tool == "write" {
                serde_json::json!({"path": notes, "content": "x"})
            } else {
                serde_json::json!({"path": notes, "old_string": "old", "new_string": "new"})
            };
            let server = spawn_mock(vec![
                http_sse(&sse_body(&[&calls_chunk(&[("w1", tool, args)])])),
                http_sse(&sse_body(&[&text_chunk("done")])),
            ])
            .await;
            actor.provider.base_url = server.url.clone();
            {
                // After `still_resolves`, at the tool's own open.
                let (notes, code) = (notes.clone(), code.clone());
                crate::tools::nofollow::on_next_open(move || {
                    std::fs::remove_file(&notes).unwrap();
                    std::os::unix::fs::symlink(&code, &notes).unwrap();
                });
            }
            let outcome = run_plan(&mut actor, &mut Notes::default()).await.unwrap();
            let RunOutcome::PermissionRequired(_) = outcome else {
                panic!("{tool}: the moved target is checked again: {outcome:?}");
            };
            assert_eq!(jev.asked(), 2, "{tool}: safety and new target reviewed");
            assert_eq!(
                std::fs::read_to_string(&code).unwrap(),
                "fn main() {}",
                "{tool}: the redirected file is untouched"
            );
            assert_eq!(tool_result_of(&actor, "w1"), None, "{tool}");
        }
    }

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
        let result = execute_call(&actor, &call, None, None, &mut noop)
            .await
            .unwrap();
        assert_eq!(result.status, ToolStatus::Ok);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "x");
    }
}
