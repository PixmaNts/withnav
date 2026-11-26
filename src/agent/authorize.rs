//! The one decision a tool call goes through before it runs: the session's
//! inputs for [`crate::permission::decide`], the permission flows wired to the
//! session's flow runtime, the decision table, and the request the Captain
//! answers. See docs/adr/0015-permission-rules-and-instinct-flow.md.

use std::{
    path::PathBuf,
    sync::{Arc, OnceLock},
    time::Duration,
};

use futures::future::BoxFuture;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::agent::actor::{AgentActor, ParentCaptain};
use crate::agent::flow_hook::{
    FlowVerdict, PermissionCallContext, PermissionCommand, ProtectedCheck, RuleResult as FlowRule,
    RunScores, TOOL_TRAIL_CALLS, ToolCallContext, ToolCallFlows, ValidatedSkill,
    captain_messages_with_cut, current_request_with_cut, delegation_with_cut, recent_with_cut,
    tool_trail_with_cut,
};
use crate::agent::permissions::{PermissionRequest, Scope};
use crate::permission::{
    decide::{
        self, Control, Decision, FlowAnswer, FlowQuery, Input, PermissionFlows, RuleProposal,
        RuleResult, Verdict,
    },
    decisions::{self, CallSnapshot, Outcome},
    floor::{Floor, FloorContext, Location, location, resolve_path},
    rules::{Effect, RuleOrigin, RuleScope},
    shell,
};
use crate::provider::{ToolCall, openai::ChatMessage};
use crate::settings::ReviewMode;

/// The permission flows answer within their own budget; past this the rule
/// applies. The 500 ms past the blocking budget lets a flow that used all of it
/// still return its fallback verdict instead of being cut by this timeout.
const FLOW_DEADLINE: Duration = Duration::from_millis(instinct_flow::BLOCKING_BUDGET_MS + 500);

/// What the decision allows for one call.
pub(super) enum Authorized {
    /// Run it; the note goes on `tool.pre_call` and `tool.post_call`.
    Run(Value),
    /// Refused by a `deny` rule or the permission flow: the model reads the message.
    Denied { message: String, note: Value },
    /// The Captain decides.
    Ask(Box<PermissionRequest>),
    /// The session was stopped while the flows were asked.
    Interrupted,
}

/// The session's permission flows: the flow runtime's aggregate verdict and its
/// compiled-in protected check. Without a flow runtime the rule applies and
/// nothing can establish a protected request.
///
/// The conversation evidence is read from the transcript the first time a flow
/// is asked, never before: a call a rule, the floor, a free read or bypass
/// decides runs without scanning the whole conversation, which on a long one
/// is the costliest step before the decision.
struct SessionFlows<'s> {
    reason: std::sync::Mutex<Option<String>>,
    /// The flow runs that answered, kept for the decision row.
    runs: std::sync::Mutex<Vec<crate::agent::flow_hook::PermissionFlowAnswer>>,
    hook: Option<Arc<dyn ToolCallFlows>>,
    call: ToolCall,
    agent: Option<String>,
    workspace: PathBuf,
    commands: Option<Vec<PermissionCommand>>,
    inside_project: Option<bool>,
    location: Option<&'static str>,
    skill: Option<ValidatedSkill>,
    transcript: &'s [ChatMessage],
    parent_captain: &'s ParentCaptain,
    /// The skill the Captain selected in the composer, when this call loads it.
    captain_skill: Option<String>,
    review: ReviewMode,
    /// Where the turn running began in `transcript`: the current request starts there.
    turn_start: Option<usize>,
    /// The transcript no longer holds the turn's start where it was: the request and the trail
    /// are then reported as cut.
    turn_start_lost: bool,
    evidence: OnceLock<Evidence>,
    cancel: CancellationToken,
}

#[cfg(test)]
thread_local! {
    /// How many times this thread read the conversation evidence.
    pub(super) static EVIDENCE_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// What the conversation says, as the permission flows read it.
#[derive(Debug, Clone, PartialEq)]
struct Evidence {
    captain_messages: Vec<String>,
    captain_messages_cut: bool,
    delegation: Vec<String>,
    delegation_cut: bool,
    recent: Vec<crate::agent::flow_hook::RecentMessage>,
    recent_cut: bool,
    current_request: Vec<crate::agent::flow_hook::RecentMessage>,
    current_request_cut: bool,
    tool_trail: Vec<crate::agent::flow_hook::ToolTrailEntry>,
    tool_trail_cut: bool,
}

impl Evidence {
    fn read(
        transcript: &[ChatMessage],
        parent_captain: &ParentCaptain,
        captain_skill: Option<&str>,
        review: ReviewMode,
        turn_start: Option<usize>,
        turn_start_lost: bool,
    ) -> Self {
        #[cfg(test)]
        EVIDENCE_READS.with(|reads| reads.set(reads.get() + 1));
        let (mut captain_messages, mut captain_messages_cut) =
            captain_messages_with_cut(transcript);
        if let Some((parent, cut)) = parent_captain
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            // The parent's words come first: they are older than the child.
            captain_messages = parent.into_iter().chain(captain_messages).collect();
            captain_messages_cut |= cut;
        }
        if let Some(name) = captain_skill {
            captain_messages.push(format!("Load the skill {name}."));
        }
        let (delegation, delegation_cut) = delegation_with_cut(transcript);
        let (recent, recent_cut) = recent_with_cut(transcript);
        // Only the consequence review reads these: with the review off the transcript is not
        // walked for them.
        let (current_request, current_request_cut, tool_trail, tool_trail_cut) = if review
            == ReviewMode::Off
        {
            (Vec::new(), false, Vec::new(), false)
        } else {
            let (request, cut) = current_request_with_cut(transcript, turn_start);
            let (trail, trail_cut) = tool_trail_with_cut(transcript, turn_start, TOOL_TRAIL_CALLS);
            (
                request,
                cut || turn_start_lost,
                trail,
                trail_cut || turn_start_lost,
            )
        };
        Self {
            captain_messages,
            captain_messages_cut,
            delegation,
            delegation_cut,
            recent,
            recent_cut,
            current_request,
            current_request_cut,
            tool_trail,
            tool_trail_cut,
        }
    }
}

impl SessionFlows<'_> {
    fn evidence(&self) -> &Evidence {
        self.evidence.get_or_init(|| {
            Evidence::read(
                self.transcript,
                self.parent_captain,
                self.captain_skill.as_deref(),
                self.review,
                self.turn_start,
                self.turn_start_lost,
            )
        })
    }

    fn context(&self, query: &FlowQuery<'_>) -> PermissionCallContext {
        let evidence = self.evidence().clone();
        PermissionCallContext {
            call: ToolCallContext {
                call_id: self.call.id.clone(),
                tool: self.call.name.clone(),
                args: self.call.args.clone(),
                agent: self.agent.clone(),
                workspace: self.workspace.clone(),
            },
            rule: query
                .matched
                .map(|rule| format!("{} {} {}", rule.action, rule.resource, rule.effect.as_str())),
            commands: self.commands.clone(),
            inside_project: self.inside_project,
            location: self.location,
            protected: matches!(query.floor, Floor::Protected(_) | Floor::NamesProtected(_)),
            skill: self.skill.clone(),
            captain_messages: evidence.captain_messages,
            captain_messages_cut: evidence.captain_messages_cut,
            delegation: evidence.delegation,
            delegation_cut: evidence.delegation_cut,
            recent: evidence.recent,
            recent_cut: evidence.recent_cut,
            current_request: evidence.current_request,
            current_request_cut: evidence.current_request_cut,
            tool_trail: evidence.tool_trail,
            tool_trail_cut: evidence.tool_trail_cut,
            review: self.review,
            protected_check: query.protected_check,
        }
    }
}

impl PermissionFlows for SessionFlows<'_> {
    fn flow<'a>(&'a self, query: &'a FlowQuery<'a>) -> BoxFuture<'a, FlowAnswer> {
        Box::pin(async move {
            let Some(hook) = &self.hook else {
                return FlowAnswer::ApplyRule;
            };
            let rule = match query.rule {
                RuleResult::Allow => FlowRule::Allow,
                RuleResult::Ask => FlowRule::Ask,
                RuleResult::NoRule => FlowRule::None,
            };
            let answer = hook.permission_pre_call(self.context(query), rule, &self.cancel);
            let answer = tokio::time::timeout(FLOW_DEADLINE, answer).await;
            if let Ok(answer) = &answer {
                *self.runs.lock().unwrap_or_else(|e| e.into_inner()) = answer.flows.clone();
            }
            match answer {
                // No flow that ran gave a verdict of its own: the rule applies,
                // and the decision says the rule, not a flow, let it through.
                Ok(answer)
                    if !matches!(answer.verdict, FlowVerdict::Deny { .. })
                        && answer.flows.iter().all(|flow| flow.used_rule) =>
                {
                    FlowAnswer::ApplyRule
                }
                Ok(answer) => match answer.verdict {
                    FlowVerdict::Allow => FlowAnswer::Allow,
                    FlowVerdict::Deny { note } => {
                        *self.reason.lock().unwrap_or_else(|e| e.into_inner()) = Some(note);
                        FlowAnswer::Deny
                    }
                    FlowVerdict::AskCaptain { note, .. } => {
                        *self.reason.lock().unwrap_or_else(|e| e.into_inner()) = Some(note);
                        FlowAnswer::AskCaptain
                    }
                },
                // A flow outage leaves the rules in force.
                Err(_) => FlowAnswer::ApplyRule,
            }
        })
    }

    fn protected_check<'a>(&'a self, query: &'a FlowQuery<'a>) -> BoxFuture<'a, ProtectedCheck> {
        Box::pin(async move { self.protected_check_traced(query).await.0 })
    }

    fn protected_check_traced<'a>(
        &'a self,
        query: &'a FlowQuery<'a>,
    ) -> BoxFuture<'a, (ProtectedCheck, Option<RunScores>)> {
        Box::pin(async move {
            let Some(hook) = &self.hook else {
                return (ProtectedCheck::Unavailable, None);
            };
            let check = hook.protected_check_traced(self.context(query), &self.cancel);
            tokio::time::timeout(FLOW_DEADLINE, check)
                .await
                .unwrap_or((ProtectedCheck::Unavailable, None))
        })
    }
}

/// Where the floor and the rules look for this actor.
pub(super) fn floor_context(actor: &AgentActor) -> FloorContext {
    FloorContext {
        workspace: actor.tool_context.workspace.clone(),
        writable_roots: actor.tool_context.writable_roots.clone(),
        worktree_roots: Vec::new(),
        readable_roots: actor.tool_context.readable_roots.clone(),
        temp_roots: actor
            .permission_dirs
            .temp
            .iter()
            .cloned()
            .chain(actor.scratch_roots.iter().cloned())
            .collect(),
        config_dir: actor.permission_dirs.config.clone(),
        data_dir: actor.permission_dirs.data.clone(),
        home: directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf()),
    }
}

pub(super) fn agent_name(actor: &AgentActor) -> String {
    actor
        .role
        .as_ref()
        .map_or("build", |role| role.id.as_str())
        .to_owned()
}

fn control(actor: &AgentActor) -> Control {
    if actor.yolo {
        Control::Yolo
    } else if actor
        .bypass
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .enabled
    {
        Control::Bypass
    } else {
        Control::Normal
    }
}

/// Whether the call acts inside the project: the target of a file tool, or
/// for bash every literal path of the line ([`location`]); `None` when unknown.
fn inside_project(call: &ToolCall, cx: &FloorContext, location: Option<Location>) -> Option<bool> {
    if call.name == "bash" {
        return match location? {
            Location::Project | Location::Scratch => Some(true),
            Location::Temp | Location::Outside => Some(false),
            Location::Unknown => None,
        };
    }
    if !matches!(
        call.name.as_str(),
        "read" | "grep" | "glob" | "write" | "edit"
    ) {
        return None;
    }
    let Some(path) = call.args.get("path").and_then(Value::as_str) else {
        return Some(call.name != "read");
    };
    let target = resolve_path(path, cx)?;
    Some(
        cx.project_roots()
            .filter_map(|root| resolve_path(&root.to_string_lossy(), cx))
            .any(|root| {
                #[cfg(windows)]
                {
                    crate::tools::win_path::path_within(&target, &root)
                }
                #[cfg(unix)]
                {
                    target.starts_with(root)
                }
            }),
    )
}

#[cfg(all(test, windows))]
#[test]
fn flow_inside_project_accepts_mixed_case_missing_suffix() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("scratch");
    let cx = FloorContext {
        workspace: dir.path().to_path_buf(),
        writable_roots: vec![root.clone()],
        ..Default::default()
    };
    let call = ToolCall {
        id: "case".into(),
        name: "write".into(),
        args: serde_json::json!({"path":root.parent().unwrap().join("SCRATCH/new.txt")}),
        args_valid: true,
    };
    assert_eq!(inside_project(&call, &cx, None), Some(true));
    // The target must be within the writable root, not merely the workspace.
    let cx = FloorContext {
        workspace: dir.path().join("project"),
        ..cx
    };
    assert_eq!(inside_project(&call, &cx, None), Some(true));
}

/// Decide `call` once. `previous` is the pending decision row a bypass answer
/// settles with this new decision.
pub(super) async fn authorize(
    actor: &AgentActor,
    call: &ToolCall,
    repeats: usize,
    previous: Option<i64>,
    validated: bool,
) -> Authorized {
    let commands = if call.name == "bash" {
        shell::split(
            call.args
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        )
    } else {
        Vec::new()
    };
    let mut cx = floor_context(actor);
    let agent = agent_name(actor);
    let scratch = actor.scratch_roots.clone();
    let mut place = location(call, &commands, &cx, &scratch);
    // A call that would leave the project may be in another worktree of the repository. The list
    // is read on this call, never kept: a worktree created a moment ago is already known, and a
    // call inside the project never runs git.
    if matches!(place, Some(Location::Outside | Location::Temp)) {
        let worktrees = crate::workspace::worktrees::list(&cx.workspace).await;
        if !worktrees.is_empty() {
            cx.worktree_roots = worktrees;
            place = location(call, &commands, &cx, &scratch);
        }
    }
    let (turn_start, turn_start_lost) = actor.request_boundary();
    let flows = SessionFlows {
        reason: std::sync::Mutex::new(None),
        runs: Default::default(),
        hook: actor.tool_flows.clone(),
        call: call.clone(),
        agent: actor.role.as_ref().map(|role| role.id.as_str().to_owned()),
        workspace: actor.tool_context.workspace.clone(),
        commands: (call.name == "bash").then(|| {
            commands
                .iter()
                .map(|command| PermissionCommand {
                    raw: command.raw.clone(),
                    normalized: command.normalized.clone(),
                    prefix: command.prefix.join(" "),
                })
                .collect()
        }),
        inside_project: inside_project(call, &cx, place),
        location: place.map(Location::as_str),
        skill: actor
            .active_validated_skill()
            .map(|(name, instructions)| ValidatedSkill { name, instructions }),
        transcript: &actor.transcript,
        parent_captain: &actor.parent_captain,
        // The host set this flag from the Captain's own selection, never from
        // model arguments (`run_calls` refuses it there).
        captain_skill: (call.name == "skill" && call.args["captain_selected"] == true)
            .then(|| call.args["name"].as_str().unwrap_or_default().to_owned()),
        review: actor.review,
        turn_start,
        turn_start_lost,
        evidence: OnceLock::new(),
        cancel: actor.cancel.clone(),
    };
    let control = control(actor);
    // The rules, the floor and the paths are read on the blocking pool: a
    // locked database waits there, never on a runtime thread.
    let lookup = {
        let (store, call, commands, cx, agent) = (
            actor.permission_store.clone(),
            call.clone(),
            commands.clone(),
            cx.clone(),
            agent.clone(),
        );
        let read = tokio::task::spawn_blocking(move || {
            decide::lookup(&Input {
                control,
                call: &call,
                commands: &commands,
                cx: &cx,
                store: store.as_deref(),
                agent: &agent,
                repeats,
                validated,
            })
        });
        tokio::select! {
            biased;
            _ = actor.cancel.cancelled() => return Authorized::Interrupted,
            read = read => read.unwrap_or_else(|error| {
                Err(crate::permission::store::Error::Invalid(error.to_string()))
            }),
        }
    };
    let input = Input {
        control,
        call,
        commands: &commands,
        cx: &cx,
        store: actor.permission_store.as_deref(),
        agent: &agent,
        repeats,
        validated,
    };
    let mut decision = tokio::select! {
        biased;
        _ = actor.cancel.cancelled() => return Authorized::Interrupted,
        decision = decide::decide_with(&input, lookup, &flows) => decision,
    };
    decision.trace.flow_runs =
        std::mem::take(&mut *flows.runs.lock().unwrap_or_else(|e| e.into_inner()));
    if let Some(note) = flows
        .reason
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
        .filter(|note| !note.is_empty())
    {
        match &mut decision.verdict {
            Verdict::Deny { message, .. } => *message = note,
            Verdict::AskCaptain { reason, .. } => *reason = note,
            Verdict::Run { .. } => {}
        }
    }
    match decision.verdict.clone() {
        Verdict::Run { approved_by } => {
            // A read inside the roots that no rule looked at was not decided
            // by anything, so the table gets no row for it; the call still says
            // `approved_by: rule` on its tool events.
            if !decision.trace.unconstrained {
                settle_row(
                    actor,
                    call,
                    &decision,
                    previous,
                    Outcome::Allowed,
                    Some(approved_by.as_str()),
                );
            }
            Authorized::Run(approval(approved_by.as_str(), &decision))
        }
        Verdict::Deny { message, .. } => {
            settle_row(actor, call, &decision, previous, Outcome::Denied, None);
            let by = if decision.trace.effect == Some(Effect::Deny) {
                "rule"
            } else {
                "flow"
            };
            Authorized::Denied {
                note: json!({
                    "denied_by": by,
                    "permission_gate": evidence(by, &decision, Some(&message)),
                }),
                message,
            }
        }
        Verdict::AskCaptain { reason, always } => {
            if let Some(id) = previous {
                update(actor, id, Outcome::Cancelled, None, None);
            }
            let mut request = PermissionRequest::new(call, scope(call, &cx), &agent);
            // The scores behind the question, so that the card explains itself.
            request.reason = match flow_trace(&decision)
                .map(|trace| decisions::trace_summary(&trace))
                .filter(|scores| !scores.is_empty())
            {
                Some(scores) => format!("{reason} (scores: {scores})"),
                None => reason,
            };
            request.requires_captain = actor.permission_store.is_none() || !bypassable(&decision);
            request.always_label = always_label(&always);
            request.reusable_approval = !always.is_empty() && actor.permission_store.is_some();
            request.permission_gate = Some(Box::new(evidence(
                "captain",
                &decision,
                Some(&request.reason),
            )));
            request.decision_id = record_pending(
                actor,
                row(
                    actor,
                    call,
                    &decision,
                    Outcome::Pending,
                    None,
                    Some(request.id),
                ),
            )
            .await;
            request.always = always;
            if reviewable(actor, call, &decision, request.requires_captain)
                && !flows.evidence().current_request_cut
            {
                request.review = Some(Box::new(crate::agent::permissions::ReviewTicket {
                    input: review_input(actor, call, &flows),
                    rule_id: decision.trace.rule_id,
                }));
            }
            Authorized::Ask(Box::new(request))
        }
    }
}

/// Reads the rules and the floor again for a call the parallel review allowed: true when
/// nothing changed since the question (see [`decide::Lookup::unchanged_since_question`]). A
/// rule table that cannot be read counts as changed.
pub(super) async fn review_still_holds(
    actor: &AgentActor,
    call: &ToolCall,
    repeats: usize,
    rule_id: Option<i64>,
) -> bool {
    let commands = if call.name == "bash" {
        shell::split(
            call.args
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        )
    } else {
        Vec::new()
    };
    let (store, call, cx, agent, control) = (
        actor.permission_store.clone(),
        call.clone(),
        floor_context(actor),
        agent_name(actor),
        control(actor),
    );
    tokio::task::spawn_blocking(move || {
        decide::lookup(&Input {
            control,
            call: &call,
            commands: &commands,
            cx: &cx,
            store: store.as_deref(),
            agent: &agent,
            repeats,
            validated: false,
        })
        .is_ok_and(|lookup| lookup.unchanged_since_question(rule_id))
    })
    .await
    .unwrap_or(false)
}

/// Bypass answers what a rule `ask` or the permission flow raised, never the
/// fixed floor, a loop, or a rule table that could not be read.
fn bypassable(decision: &Decision) -> bool {
    decision.trace.floor.is_none()
        && !decision.trace.doom_loop
        && decision.trace.flow_verdict.is_some()
}

/// Whether the parallel metacognition review may answer this question. Only an ordinary question
/// of the built-in `permission-default` flow's own verdict qualifies: never a written deny (it
/// stopped the call before), the fixed floor, a credential, a protected check, a loop, another
/// flow's objection, a custom flow's ask, a failure fallback (the built-in flow falls back to
/// `apply_rule`, so its answer then uses the rule), a skill validation, a child session's call
/// or a question the Captain must answer. With the setting `off` nothing is prepared at all.
/// The caller also refuses a request whose current request was cut: the review would judge a
/// request it cannot see whole.
fn reviewable(
    actor: &AgentActor,
    call: &ToolCall,
    decision: &Decision,
    requires_captain: bool,
) -> bool {
    use crate::agent::flow_hook::FlowAction;
    let asks: Vec<_> = decision
        .trace
        .flow_runs
        .iter()
        .filter(|run| run.action == FlowAction::AskCaptain)
        .collect();
    actor.review != ReviewMode::Off
        && !requires_captain
        && bypassable(decision)
        && decision.trace.protected_check.is_none()
        && decision.trace.flow_verdict == Some(decide::FlowAnswer::AskCaptain.as_str())
        && !asks.is_empty()
        && asks
            .iter()
            .all(|run| run.flow == "permission-default" && !run.used_rule)
        && call.name != "skill"
        && actor
            .parent_captain
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_none()
}

/// The review's input bytes, its instructions excluded: the 16 KiB of the whole call minus 1 KiB
/// kept for the instructions. Requests of 45 to 54 KB on the call path were what made the
/// earlier per-call review too slow (2026-10).
pub const REVIEW_INPUT_BYTES: usize = 15 * 1024;

/// What the review reads: the current request (with the agent's proposal it may accept,
/// labelled), a delegation labelled as such, the validated skill, the call with the session's
/// secrets redacted, where it acts and its parsed commands; the goal and the tools already called
/// as context only. Never memory, tool outputs, compaction prose or an earlier request: the
/// projection carries none of them.
fn review_input(actor: &AgentActor, call: &ToolCall, flows: &SessionFlows<'_>) -> String {
    let evidence = flows.evidence();
    let goal = actor
        .goal
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .active()
        .map(|goal| goal.objective.clone());
    let mut input = json!({
        "current_request": evidence.current_request,
        "call": {"tool": call.name, "arguments": call.args},
        "location": flows.location,
        "inside_project": flows.inside_project,
        "commands": flows.commands.as_ref().map(|commands| {
            commands.iter().map(|command| command.raw.clone()).collect::<Vec<_>>()
        }),
    });
    if !evidence.delegation.is_empty() {
        input["delegation_from_parent_agent_not_the_captain"] = json!(evidence.delegation);
    }
    if let Some(skill) = &flows.skill {
        input["validated_skill"] = json!({
            "name": skill.name,
            "instructions": crate::tools::bound_output(&skill.instructions, 2048).0,
        });
    }
    input["context_only"] = json!({"goal": goal, "tools_already_called": evidence.tool_trail});
    if let Some(secrets) = &actor.tool_context.secrets {
        secrets.redact_json(&mut input);
    }
    bound_review_input(input)
}

/// Fits the input in [`REVIEW_INPUT_BYTES`]: the context goes first, then the call's arguments
/// are cut, then the oldest messages of the request; a last cut bounds whatever remains.
fn bound_review_input(mut input: Value) -> String {
    let fits = |input: &Value| input.to_string().len() <= REVIEW_INPUT_BYTES;
    if !fits(&input) {
        input
            .as_object_mut()
            .expect("the input is an object")
            .remove("context_only");
    }
    if !fits(&input) {
        let args = input["call"]["arguments"].to_string();
        input["call"]["arguments"] = json!(crate::tools::bound_output(&args, 4096).0);
    }
    while !fits(&input)
        && input["current_request"]
            .as_array()
            .is_some_and(|messages| messages.len() > 1)
    {
        if let Some(messages) = input["current_request"].as_array_mut() {
            messages.remove(0);
        }
    }
    crate::tools::bound_output(&input.to_string(), REVIEW_INPUT_BYTES).0
}

fn scope(call: &ToolCall, cx: &FloorContext) -> Scope {
    let path = call.args.get("path").and_then(Value::as_str);
    match call.name.as_str() {
        "fetch" => {
            if let Some(origin) = call
                .args
                .get("url")
                .and_then(Value::as_str)
                .and_then(|url| reqwest::Url::parse(url.trim()).ok())
                .filter(|url| url.host_str().is_some())
                .map(|url| url.origin().ascii_serialization())
            {
                return Scope::Origin(origin);
            }
        }
        "grep" | "glob" => {
            if let Some(target) = path.and_then(|path| resolve_path(path, cx)) {
                return Scope::Folder(target);
            }
        }
        "read" | "write" | "edit" => {
            if let Some(parent) = path
                .and_then(|path| resolve_path(path, cx))
                .and_then(|target| target.parent().map(PathBuf::from))
            {
                return Scope::Folder(parent);
            }
        }
        _ => {}
    }
    Scope::Folder(
        crate::tools::canonical_path(&cx.workspace).unwrap_or_else(|_| cx.workspace.clone()),
    )
}

fn scope_name(scope: &RuleScope) -> &'static str {
    match scope {
        RuleScope::Global => "global",
        RuleScope::Project(_) => "project",
        RuleScope::Agent(_) => "agent",
    }
}

/// The button text of "Always", e.g. `Always (project): bash git push *`;
/// `None` when "Always" is not offered.
pub(super) fn always_label(always: &[RuleProposal]) -> Option<String> {
    let first = always.first()?;
    let rule = |p: &RuleProposal| format!("{} {}", p.action, p.resource);
    Some(
        if always
            .iter()
            .all(|p| scope_name(&p.scope) == scope_name(&first.scope))
        {
            format!(
                "Always ({}): {}",
                scope_name(&first.scope),
                always.iter().map(rule).collect::<Vec<_>>().join("; ")
            )
        } else {
            format!(
                "Always: {}",
                always
                    .iter()
                    .map(|p| format!("({}) {}", scope_name(&p.scope), rule(p)))
                    .collect::<Vec<_>>()
                    .join("; ")
            )
        },
    )
}

/// Save every proposal of an "Always" answer in one transaction.
pub(super) async fn save_always(
    actor: &AgentActor,
    always: &[RuleProposal],
) -> Result<Vec<i64>, String> {
    let rules: Vec<_> = always
        .iter()
        .map(|p| {
            (
                p.action.clone(),
                p.resource.clone(),
                Effect::Allow,
                p.scope.clone(),
            )
        })
        .collect();
    actor
        .permission_store
        .as_ref()
        .ok_or("permission rules unavailable")?
        .run(move |store| store.add_all(&rules, RuleOrigin::Always))
        .await
        .map(|rules| rules.into_iter().map(|rule| rule.id).collect())
        .map_err(|error| {
            tracing::warn!(%error, "the Always rules were not saved");
            decisions::public_reason(&error).to_owned()
        })
}

/// What the flows answered for this decision, for the decision table and the
/// permission card: the protected check's verdict, run and score, and each
/// flow run with its scores, and the model and flow digest behind them.
fn flow_trace(decision: &Decision) -> Option<Value> {
    let mut trace = serde_json::Map::new();
    if let Some(check) = decision.trace.protected_check {
        let run = decision.trace.protected_run.as_ref();
        let mut entry = json!({
            "verdict": check.as_str(),
            "run_id": run.map(|run| run.run_id.as_str()),
            "score": run.and_then(|run| run.scores.get("protected_requested")),
        });
        if let Some(run) = run {
            versions(
                &mut entry,
                run.definition_digest.as_str(),
                run.model.as_str(),
            );
        }
        trace.insert("protected_check".into(), entry);
    }
    if !decision.trace.flow_runs.is_empty() {
        trace.insert(
            "flows".into(),
            json!(
                decision
                    .trace
                    .flow_runs
                    .iter()
                    .map(|run| {
                        let mut entry =
                            json!({"flow": run.flow, "run_id": run.run_id, "scores": run.scores});
                        versions(&mut entry, &run.definition_digest, &run.model);
                        entry
                    })
                    .collect::<Vec<_>>()
            ),
        );
    }
    (!trace.is_empty()).then_some(Value::Object(trace))
}

/// Name the flow text and the model behind a run's scores in its trace entry.
/// Both are left out when empty (no flow ran), so such an entry keeps the
/// shape it had before they were recorded.
fn versions(entry: &mut Value, definition_digest: &str, model: &str) {
    for (key, value) in [("definition_digest", definition_digest), ("model", model)] {
        if !value.is_empty() {
            entry[key] = json!(value);
        }
    }
}

fn evidence(source: &str, decision: &Decision, reason: Option<&str>) -> Value {
    let mut gate = json!({
        "policy_version": 3,
        "source": source,
        "reason": reason,
        "rule_id": decision.trace.rule_id,
        "effect": decision.trace.effect,
        "flow_verdict": decision.trace.flow_verdict,
        "floor": decision.trace.floor,
        "truncated": false,
    });
    if let Some(Value::Object(trace)) = flow_trace(decision) {
        gate.as_object_mut()
            .expect("the gate is an object")
            .extend(trace);
    }
    gate
}

fn approval(approved_by: &str, decision: &Decision) -> Value {
    let mut gate = evidence(approved_by, decision, None);
    gate["approved_by"] = json!(approved_by);
    json!({"approved_by": approved_by, "permission_gate": gate})
}

/// The note of a call the Captain (or bypass) let run after a question.
pub(super) fn answered_approval(request: &PermissionRequest, approved_by: &str) -> Value {
    let mut gate = request
        .permission_gate
        .as_deref()
        .cloned()
        .unwrap_or_else(|| json!({"policy_version": 3, "reason": request.reason}));
    gate["source"] = json!(approved_by);
    gate["approved_by"] = json!(approved_by);
    json!({"approved_by": approved_by, "permission_gate": gate})
}

/// The row this decision becomes, before it is written.
fn row(
    actor: &AgentActor,
    call: &ToolCall,
    decision: &Decision,
    outcome: Outcome,
    approved_by: Option<&str>,
    request_id: Option<ulid::Ulid>,
) -> decisions::Decision {
    decisions::Decision {
        id: 0,
        time: crate::bus::event::now_rfc3339(),
        session_id: actor.permission_session.clone(),
        turn_id: actor.permission_turn.clone(),
        call_id: call.id.clone(),
        request_id: request_id.map(|id| id.to_string()),
        agent: agent_name(actor),
        workspace: actor.tool_context.workspace.clone(),
        tool: call.name.clone(),
        call_snapshot: CallSnapshot {
            arguments: call.args.to_string(),
            truncated: false,
        },
        command_matches: decision.trace.command_matches.clone(),
        rule_id: decision.trace.rule_id,
        effect: decision.trace.effect,
        flow_verdict: decision.trace.flow_verdict.map(str::to_owned),
        flow_trace: flow_trace(decision),
        captain_answer: None,
        outcome,
        approved_by: approved_by.map(str::to_owned),
    }
}

/// Writes the decision row of a call that has already run or been refused, off
/// the tool path: nothing waits for the insert, so dispatch never waits on
/// SQLite. The row is lost if the process dies first, which a history of the
/// decisions that were made can afford.
fn record_detached(actor: &AgentActor, row: decisions::Decision) {
    let Some(store) = actor.permission_store.clone() else {
        return;
    };
    store.detach(move |store| {
        if let Err(error) = store.record(&row) {
            tracing::warn!(%error, "permission decision not recorded");
        }
    });
}

/// Writes the `pending` row of a question before the turn yields: its id is
/// what the Captain's answer settles, so this one is awaited. No tool runs on
/// this path.
async fn record_pending(actor: &AgentActor, row: decisions::Decision) -> Option<i64> {
    let store = actor.permission_store.as_ref()?;
    store
        .run(move |store| store.record(&row))
        .await
        .inspect_err(|error| tracing::warn!(%error, "permission decision not recorded"))
        .ok()
}

/// A bypass answer settles the pending row with the fresh decision; any
/// other decision is a new row. Both leave the caller free straight away.
fn settle_row(
    actor: &AgentActor,
    call: &ToolCall,
    decision: &Decision,
    previous: Option<i64>,
    outcome: Outcome,
    approved_by: Option<&str>,
) {
    match previous {
        Some(id) => update(
            actor,
            id,
            outcome,
            Some(&json!({"answer": "bypass"})),
            approved_by,
        ),
        None => record_detached(
            actor,
            row(actor, call, decision, outcome, approved_by, None),
        ),
    }
}

/// Settles a row already written, off the call path: a Captain's answer and a
/// cancellation both reach here while a tool is about to run.
pub(super) fn update(
    actor: &AgentActor,
    id: i64,
    outcome: Outcome,
    answer: Option<&Value>,
    approved_by: Option<&str>,
) {
    let Some(store) = actor.permission_store.clone() else {
        return;
    };
    let (answer, approved_by) = (answer.cloned(), approved_by.map(str::to_owned));
    store.detach(move |store| {
        let updated = store.update_outcome(id, outcome, answer.as_ref(), approved_by.as_deref());
        if let Err(error) = updated {
            tracing::warn!(%error, "permission decision outcome not recorded");
        }
    });
}
