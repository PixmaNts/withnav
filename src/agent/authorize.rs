//! The one decision a tool call goes through before it runs: the session's
//! inputs for [`crate::permission::decide`], the permission flows wired to the
//! session's flow runtime, the decision table, and the request the Captain
//! answers. See docs/adr/0015-permission-rules-and-instinct-flow.md.

use std::{path::PathBuf, sync::Arc, time::Duration};

use futures::future::BoxFuture;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::agent::actor::AgentActor;
use crate::agent::flow_hook::{
    captain_messages_with_cut, delegation_with_cut, FlowVerdict, PermissionCallContext,
    PermissionCommand, ProtectedCheck, RuleResult as FlowRule, ToolCallContext, ToolCallFlows,
    ValidatedSkill,
};
use crate::agent::permissions::{PermissionRequest, Scope};
use crate::permission::{
    decide::{
        self, ApprovedBy, Control, Decision, FlowAnswer, FlowQuery, Input, PermissionFlows,
        RuleProposal, RuleResult, Verdict,
    },
    decisions::{self, CallSnapshot, Outcome},
    floor::{resolve_path, Floor, FloorContext},
    rules::{Effect, RuleOrigin, RuleScope},
    shell,
};
use crate::provider::ToolCall;

/// The permission flows answer within their own budget; past this the rule applies.
const FLOW_DEADLINE: Duration = Duration::from_millis(2500);

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
struct SessionFlows {
    reason: std::sync::Mutex<Option<String>>,
    hook: Option<Arc<dyn ToolCallFlows>>,
    call: ToolCall,
    agent: Option<String>,
    workspace: PathBuf,
    commands: Option<Vec<PermissionCommand>>,
    inside_project: Option<bool>,
    skill: Option<ValidatedSkill>,
    captain_messages: Vec<String>,
    captain_messages_cut: bool,
    delegation: Vec<String>,
    delegation_cut: bool,
    cancel: CancellationToken,
}

impl SessionFlows {
    fn context(&self, query: &FlowQuery<'_>) -> PermissionCallContext {
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
            protected: matches!(query.floor, Floor::Protected(_)),
            skill: self.skill.clone(),
            captain_messages: self.captain_messages.clone(),
            captain_messages_cut: self.captain_messages_cut,
            delegation: self.delegation.clone(),
            delegation_cut: self.delegation_cut,
            protected_check: query.protected_check,
        }
    }
}

impl PermissionFlows for SessionFlows {
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
            match tokio::time::timeout(FLOW_DEADLINE, answer).await {
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
        Box::pin(async move {
            let Some(hook) = &self.hook else {
                return ProtectedCheck::Unavailable;
            };
            let check = hook.protected_check(self.context(query), &self.cancel);
            tokio::time::timeout(FLOW_DEADLINE, check)
                .await
                .unwrap_or(ProtectedCheck::Unavailable)
        })
    }
}

/// Where the floor and the rules look for this actor.
pub(super) fn floor_context(actor: &AgentActor) -> FloorContext {
    FloorContext {
        workspace: actor.tool_context.workspace.clone(),
        writable_roots: actor.tool_context.writable_roots.clone(),
        readable_roots: actor.tool_context.readable_roots.clone(),
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

/// Whether the target of a file tool lies inside the project.
fn inside_project(call: &ToolCall, cx: &FloorContext) -> Option<bool> {
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
        std::iter::once(&cx.workspace)
            .chain(&cx.writable_roots)
            .filter_map(|root| resolve_path(&root.to_string_lossy(), cx))
            .any(|root| target.starts_with(root)),
    )
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
    let cx = floor_context(actor);
    let agent = agent_name(actor);
    let (mut captain_messages, mut captain_messages_cut) =
        captain_messages_with_cut(&actor.transcript);
    if let Some((parent, cut)) = actor
        .parent_captain
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
    {
        // The parent's words come first: they are older than the child.
        captain_messages = parent.into_iter().chain(captain_messages).collect();
        captain_messages_cut |= cut;
    }
    // The host set this flag from the Captain's own selection, never from
    // model arguments (`run_calls` refuses it there).
    if call.name == "skill" && call.args["captain_selected"] == true {
        captain_messages.push(format!(
            "Load the skill {}.",
            call.args["name"].as_str().unwrap_or_default()
        ));
    }
    let (delegation, delegation_cut) = delegation_with_cut(&actor.transcript);
    let flows = SessionFlows {
        reason: std::sync::Mutex::new(None),
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
        inside_project: inside_project(call, &cx),
        skill: actor
            .active_validated_skill()
            .map(|(name, instructions)| ValidatedSkill { name, instructions }),
        captain_messages,
        captain_messages_cut,
        delegation,
        delegation_cut,
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
            settle_row(
                actor,
                call,
                &decision,
                previous,
                Outcome::Allowed,
                Some(approved_by.as_str()),
            )
            .await;
            Authorized::Run(approval(approved_by.as_str(), &decision))
        }
        Verdict::Deny { message, .. } => {
            settle_row(actor, call, &decision, previous, Outcome::Denied, None).await;
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
                update(actor, id, Outcome::Cancelled, None, None).await;
            }
            let mut request = PermissionRequest::new(call, scope(call, &cx), &agent);
            request.reason = reason;
            request.requires_captain = actor.permission_store.is_none() || !bypassable(&decision);
            request.always_label = always_label(&always);
            request.reusable_approval = !always.is_empty() && actor.permission_store.is_some();
            request.permission_gate = Some(Box::new(evidence(
                "captain",
                &decision,
                Some(&request.reason),
            )));
            request.decision_id = record(
                actor,
                call,
                &decision,
                Outcome::Pending,
                None,
                Some(request.id),
            )
            .await;
            request.always = always;
            Authorized::Ask(Box::new(request))
        }
    }
}

/// Bypass answers what a rule `ask` or the permission flow raised, never the
/// fixed floor, a loop, or a rule table that could not be read.
fn bypassable(decision: &Decision) -> bool {
    decision.trace.floor.is_none()
        && !decision.trace.doom_loop
        && decision.trace.flow_verdict.is_some()
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
        cx.workspace
            .canonicalize()
            .unwrap_or_else(|_| cx.workspace.clone()),
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
        .map_err(|error| decisions::bounded_error(&error.to_string()))
}

fn evidence(source: &str, decision: &Decision, reason: Option<&str>) -> Value {
    json!({
        "policy_version": 3,
        "source": source,
        "reason": reason,
        "rule_id": decision.trace.rule_id,
        "effect": decision.trace.effect,
        "flow_verdict": decision.trace.flow_verdict,
        "floor": decision.trace.floor,
        "truncated": false,
    })
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

async fn record(
    actor: &AgentActor,
    call: &ToolCall,
    decision: &Decision,
    outcome: Outcome,
    approved_by: Option<&str>,
    request_id: Option<ulid::Ulid>,
) -> Option<i64> {
    let store = actor.permission_store.as_ref()?;
    let row = decisions::Decision {
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
        captain_answer: None,
        outcome,
        approved_by: approved_by.map(str::to_owned),
    };
    store
        .run(move |store| store.record(&row))
        .await
        .inspect_err(|error| tracing::warn!(%error, "permission decision not recorded"))
        .ok()
}

/// A bypass answer settles the pending row with the fresh decision; any
/// other decision is a new row.
async fn settle_row(
    actor: &AgentActor,
    call: &ToolCall,
    decision: &Decision,
    previous: Option<i64>,
    outcome: Outcome,
    approved_by: Option<&str>,
) {
    match previous {
        Some(id) => {
            update(
                actor,
                id,
                outcome,
                Some(&json!({"answer": "bypass"})),
                approved_by,
            )
            .await
        }
        None => {
            record(actor, call, decision, outcome, approved_by, None).await;
        }
    }
}

pub(super) async fn update(
    actor: &AgentActor,
    id: i64,
    outcome: Outcome,
    answer: Option<&Value>,
    approved_by: Option<&str>,
) {
    let Some(store) = &actor.permission_store else {
        return;
    };
    let (answer, approved_by) = (answer.cloned(), approved_by.map(str::to_owned));
    let updated = store
        .run(move |store| {
            store.update_outcome(id, outcome, answer.as_ref(), approved_by.as_deref())
        })
        .await;
    if let Err(error) = updated {
        tracing::warn!(%error, "permission decision outcome not recorded");
    }
}

/// Yolo checks nothing, and the decision table says so.
pub(super) async fn record_yolo(actor: &AgentActor, call: &ToolCall) {
    let decision = Decision {
        verdict: Verdict::Run {
            approved_by: ApprovedBy::Yolo,
        },
        trace: Default::default(),
    };
    record(
        actor,
        call,
        &decision,
        Outcome::Allowed,
        Some(ApprovedBy::Yolo.as_str()),
        None,
    )
    .await;
}
