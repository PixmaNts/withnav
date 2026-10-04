//! `EventEnvelope` → conversation projection. Replaying durable events (no `llm.delta`) must give
//! the same `conversation.blocks` as the live stream.
//!
//! Tool lifecycle: a tool is *planned* (`llm.response`, status `None`, not in `pending_tools`),
//! *running* (`tool.pre_call`, status `None`, in `pending_tools`) and then settled by
//! `tool.post_call`. Planned tools never pre-called are settled as `Interrupted` at `turn.end`
//! (`done`, `yielded`, `interrupted`, `error`; an `ask` without a pre/post pair becomes `Ok` on
//! `yielded`, its question is shown by the yield block) and at `session.end`; running tools at `turn.end`
//! (`interrupted`, `error`) and `session.end`. No new state: `Interrupted` is reused. Streamed tool-call fragments are never rendered ("preparing" is not shown): they
//! are only buffered per index until the step's `llm.response` replaces them.
use super::{block, text::strip_controls};
use crate::{
    bus::event::{ContextSnapshot, EventEnvelope, EventKind, ModelChangeReason, ModelChanged},
    provider::{StreamDelta, ToolCall, Usage},
    tools::ToolStatus,
    tui::{
        protocol::ProtocolError,
        state::{Activity, Block, BlockKind, ChildAgent, ChildStatus, PermissionPrompt, State},
    },
};
use serde::Deserialize;
use serde_json::Value;

type StepKey = (String, String, String);

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use ulid::Ulid;

    #[test]
    fn budget_yield_stays_out_of_steps_through_turn_end_and_agent_state() {
        for kind in [Some("budget"), Some("choice"), None] {
            let mut state = State::default();
            let session = Ulid::new();
            for (seq, event, payload) in [
                (
                    1,
                    EventKind::AgentYield,
                    json!({"question":"step budget exhausted", "kind":kind}),
                ),
                (2, EventKind::TurnEnd, json!({"outcome":"yielded"})),
                (3, EventKind::AgentState, json!({"to":"yielded"})),
            ] {
                apply_envelope(&mut state, EventEnvelope::new(session, seq, event, payload))
                    .unwrap();
                assert_eq!(
                    state.activity,
                    if kind == Some("budget") {
                        Activity::OutOfSteps
                    } else {
                        Activity::AwaitingUser
                    }
                );
            }
            assert_eq!(
                state.conversation.blocks[0].kind,
                if kind == Some("budget") {
                    BlockKind::Notice("Out of steps · send a message to continue".into())
                } else {
                    BlockKind::YieldQuestion("step budget exhausted".into())
                }
            );
        }
    }
}

#[derive(Deserialize)]
struct DeltaPayload {
    turn_id: String,
    step_id: String,
    delta: StreamDelta,
}
#[derive(Deserialize)]
struct ResponsePayload {
    turn_id: String,
    step_id: String,
    message: crate::provider::AssistantMessage,
    usage: Option<Usage>,
}

/// Fields to merge into a tool block; `None` keeps what is already there.
#[derive(Default)]
struct ToolPatch {
    name: Option<String>,
    args: Option<Value>,
    status: Option<ToolStatus>,
    output: Option<String>,
    metadata: Option<Value>,
    duration_ms: Option<u64>,
    exit_code: Option<i32>,
}

pub(super) fn apply_envelope(s: &mut State, e: EventEnvelope) -> Result<(), ProtocolError> {
    if e.schema_version != 1 {
        return Err(ProtocolError(format!(
            "unsupported schema {}",
            e.schema_version
        )));
    }
    if s.last_applied_seq.is_some_and(|seq| e.seq <= seq) {
        return Ok(());
    }
    s.last_applied_seq = Some(e.seq);
    s.latest_envelope = Some(e.clone());
    let kind = EventKind::from_wire(&e.kind);
    let bad = || ProtocolError(format!("malformed {} payload", e.kind));
    let actor = e.actor_id.clone();
    if matches!(
        e.kind.as_str(),
        "subagent.spawned" | "subagent.done" | "subagent.watchdog"
    ) {
        let v = &e.payload;
        let own = s
            .session
            .as_ref()
            .map(|m| m.session_id.as_str())
            .or(s.resuming.as_deref());
        if e.kind != "subagent.watchdog"
            && (v["parent_session_id"].as_str() != own || own.is_none())
        {
            return Ok(());
        }
        let id = v["child_session_id"].as_str().ok_or_else(bad)?;
        let at = s
            .children
            .iter()
            .position(|c| c.session_id == id)
            .unwrap_or_else(|| {
                s.children.push(ChildAgent {
                    session_id: id.into(),
                    ..Default::default()
                });
                s.children.len() - 1
            });
        let child = &mut s.children[at];
        if let Some(agent) = v["agent"].as_str() {
            child.name = strip_controls(agent);
        }
        if let Some(goal) = v["goal"].as_str() {
            child.goal = strip_controls(goal);
        }
        if let Some(delivery) = v["delivery"].as_str() {
            child.delivery = Some(strip_controls(delivery));
        }
        let initialized = child.child_seq.is_some()
            || child.status.terminal()
            || child.event_seq.is_some_and(|seq| seq >= e.seq);
        child.reason = v["reason"].as_str().map(strip_controls);
        child.event_seq = Some(e.seq);
        child.status = match e.kind.as_str() {
            "subagent.spawned" if !initialized => ChildStatus::Running,
            "subagent.spawned" => child.status,
            "subagent.done" => ChildStatus::from_wire(v["outcome"].as_str().ok_or_else(bad)?),
            _ if !child.status.terminal() => match v["verdict"].as_str() {
                Some("doubt" | "stuck") => ChildStatus::Doubt,
                Some("active") if child.status == ChildStatus::Doubt => ChildStatus::Running,
                _ => child.status,
            },
            _ => child.status,
        };
        if e.kind == "subagent.done" {
            let text = format!(
                "subagent {} · {} · {} · alt+s open transcript",
                child.name,
                child.status.label(),
                child
                    .goal
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .chars()
                    .take(100)
                    .collect::<String>()
            );
            s.conversation
                .blocks
                .push(block(e.seq.to_string(), BlockKind::Notice(text)));
        }
        return Ok(());
    }
    match kind {
        Some(EventKind::SessionStart) => s.activity = Activity::Idle,
        Some(EventKind::SessionEnd) => {
            s.activity = Activity::Stopped;
            // A stopped session switches nothing after its turn.
            s.model_switch = None;
            for b in &mut s.conversation.blocks {
                interrupt_block(b);
            }
        }
        Some(EventKind::MessageReceived) => {
            let v = e.payload;
            let source = v["source"].as_str().ok_or_else(bad)?.to_string();
            let text = v["content"].as_str().ok_or_else(bad)?;
            s.conversation.blocks.push(block(
                e.seq.to_string(),
                BlockKind::Message {
                    source,
                    text: strip_controls(text),
                },
            ));
        }
        Some(EventKind::TurnStart) => {
            let v = e.payload;
            s.conversation.current_turn = Some(v["turn_id"].as_str().ok_or_else(bad)?.into());
            s.activity = Activity::Running;
        }
        Some(EventKind::LlmDelta) => {
            let p: DeltaPayload = serde_json::from_value(e.payload).map_err(|_| bad())?;
            let key = (actor, p.turn_id.clone(), p.step_id.clone());
            s.conversation.current_step = Some(key.clone());
            let id = step_block_id(s, &key, &p.step_id);
            let (mut text, mut thinking) = existing_step(&s.conversation.blocks, &id);
            match p.delta {
                StreamDelta::Text { text: t } => text.push_str(&strip_controls(&t)),
                StreamDelta::Thinking { text: t } => thinking.push_str(&strip_controls(&t)),
                StreamDelta::ToolCallStarted { index, .. } => {
                    s.conversation
                        .pending_tool_fragments
                        .entry(index.to_string())
                        .or_default();
                }
                StreamDelta::ToolCallArgs { index, fragment } => s
                    .conversation
                    .pending_tool_fragments
                    .entry(index.to_string())
                    .or_default()
                    .push_str(&fragment),
            }
            upsert(
                &mut s.conversation.blocks,
                id,
                BlockKind::AssistantStep {
                    text,
                    thinking,
                    revision: e.seq,
                },
            );
        }
        Some(EventKind::LlmResponse) => {
            let p: ResponsePayload = serde_json::from_value(e.payload).map_err(|_| bad())?;
            let key = (actor, p.turn_id.clone(), p.step_id.clone());
            let id = step_block_id(s, &key, &p.step_id);
            s.conversation.pending_tool_fragments.clear();
            s.conversation.last_response_step = Some(key.clone());
            s.conversation
                .step_turns
                .insert(key.clone(), p.turn_id.clone());
            upsert(
                &mut s.conversation.blocks,
                id,
                BlockKind::AssistantStep {
                    text: strip_controls(&p.message.text),
                    thinking: strip_controls(&p.message.thinking),
                    revision: e.seq,
                },
            );
            for ToolCall {
                id: call_id,
                name,
                args,
                ..
            } in p.message.tool_calls
            {
                let patch = ToolPatch {
                    name: Some(name),
                    args: Some(args),
                    ..Default::default()
                };
                tool_merge(s, &key.0, &p.turn_id, call_id, patch);
            }
            if let Some(u) = p.usage.or(p.message.usage) {
                count_usage(s, key, u)
            }
        }
        Some(EventKind::ToolPreCall) => {
            let v = e.payload;
            let id = v["tool_call_id"].as_str().ok_or_else(bad)?.to_string();
            let name = v["name"].as_str().ok_or_else(bad)?.to_string();
            let turn = s.conversation.current_turn.clone().unwrap_or_default();
            s.conversation
                .pending_tools
                .entry(id.clone())
                .or_insert_with(|| turn.clone());
            let patch = ToolPatch {
                name: Some(name),
                args: Some(v["args"].clone()).filter(|a| !a.is_null()),
                ..Default::default()
            };
            tool_merge(s, &actor, &turn, id, patch);
        }
        Some(EventKind::ToolPostCall) => {
            let v = e.payload;
            let id = v["tool_call_id"].as_str().ok_or_else(bad)?.to_string();
            let status: ToolStatus =
                serde_json::from_value(v["status"].clone()).map_err(|_| bad())?;
            let meta = v["metadata"].clone();
            let turn = s
                .conversation
                .pending_tools
                .get(&id)
                .cloned()
                .or_else(|| s.conversation.current_turn.clone())
                .unwrap_or_default();
            let patch = ToolPatch {
                name: v["name"].as_str().map(str::to_string),
                status: Some(status),
                output: Some(strip_controls(v["output"].as_str().unwrap_or(""))),
                duration_ms: meta["duration_ms"].as_u64(),
                exit_code: meta["exit_code"].as_i64().map(|x| x as i32),
                metadata: Some(meta),
                ..Default::default()
            };
            tool_merge(s, &actor, &turn, id, patch);
        }
        Some(EventKind::AgentYield) => {
            let v = e.payload;
            s.watchdog_paused = v["kind"] == "watchdog" || v["kind"] == "budget";
            let q = v["question"].as_str().ok_or_else(bad)?;
            let budget = v["kind"].as_str() == Some("budget");
            s.activity = if budget {
                Activity::OutOfSteps
            } else {
                Activity::AwaitingUser
            };
            s.conversation.blocks.push(block(
                e.seq.to_string(),
                if budget {
                    BlockKind::Notice("Out of steps · send a message to continue".into())
                } else {
                    BlockKind::YieldQuestion(strip_controls(q))
                },
            ));
        }
        Some(EventKind::TurnEnd) => {
            let v = e.payload;
            let outcome = v["outcome"].as_str().unwrap_or("");
            if outcome != "yielded" {
                s.watchdog_paused = false;
            }
            s.activity = match outcome {
                "yielded" if s.activity == Activity::OutOfSteps => Activity::OutOfSteps,
                "yielded" => Activity::AwaitingUser,
                "interrupted" | "done" | "error" => Activity::Idle,
                _ => s.activity,
            };
            let turn = v["turn_id"].as_str().map(str::to_string);
            if let Some(error) = v.get("error").filter(|x| x.is_object()) {
                let code = error["code"].as_str().unwrap_or("error");
                let message = error["message"].as_str().unwrap_or("");
                s.conversation.blocks.push(block(
                    format!("error:{}", e.seq),
                    BlockKind::Error(error_text(code, message)),
                ));
            } else if outcome == "interrupted" {
                s.conversation.blocks.push(block(
                    format!("notice:{}", e.seq),
                    BlockKind::Notice("turn interrupted".into()),
                ));
            }
            if let Some(u) = v
                .get("usage")
                .and_then(|x| serde_json::from_value::<Usage>(x.clone()).ok())
            {
                // Fallback for a final step that reported no usage of its own.
                if let Some(key) = s.conversation.last_response_step.clone() {
                    if turn.as_deref().is_none_or(|t| key.1 == t)
                        && !s.conversation.counted_steps.contains(&key)
                    {
                        count_usage(s, key, u);
                    }
                }
            }
            if let Some(t) = &turn {
                let uncounted = s
                    .conversation
                    .step_turns
                    .iter()
                    .any(|(k, v)| v == t && !s.conversation.counted_steps.contains(k));
                s.conversation.missing_usage |= uncounted;
                settle_turn_tools(s, &actor, t, outcome);
            }
        }
        Some(EventKind::PermissionRequested) => {
            let v = e.payload;
            let child = v["child_session_id"].as_str();
            let child_name = child
                .and_then(|id| s.children.iter().find(|c| c.session_id == id))
                .map(|c| c.name.clone());
            if let Some(child) = child.and_then(|id| {
                s.children
                    .iter_mut()
                    .find(|c| c.session_id == id && !c.status.terminal() && c.child_seq.is_none())
            }) {
                child.status = ChildStatus::Permission;
                child.event_seq = Some(e.seq);
            }
            let text = |key: &str| -> Result<String, ProtocolError> {
                Ok(strip_controls(v[key].as_str().ok_or_else(bad)?))
            };
            let request_id = text("request_id")?;
            let id = format!("permission:{request_id}");
            if s.conversation.blocks.iter().any(|b| b.id == id) {
                return Ok(());
            }
            let prompt = PermissionPrompt {
                child_session_id: child.map(str::to_string),
                request_id,
                tool_call_id: text("tool_call_id")?,
                tool: text("tool")?,
                arguments: text("arguments")?,
                scope_kind: strip_controls(v["scope"]["kind"].as_str().ok_or_else(bad)?),
                scope: strip_controls(v["scope"]["value"].as_str().ok_or_else(bad)?),
                agent: strip_controls(v["agent"].as_str().unwrap_or_default()),
                reason: strip_controls(&match child {
                    Some(child) => format!(
                        "subagent {}: {}",
                        child_name
                            .as_deref()
                            .filter(|name| !name.is_empty())
                            .unwrap_or(child),
                        v["reason"].as_str().unwrap_or_default()
                    ),
                    None => v["reason"].as_str().unwrap_or_default().to_string(),
                }),
                resolution: None,
                message: None,
                stale: false,
            };
            // A fresh request for the call just approved (it now lands
            // elsewhere) replaces that prompt instead of adding a second one.
            let retarget = s
                .conversation
                .blocks
                .iter()
                .rposition(|b| matches!(&b.kind, BlockKind::Permission(_)))
                .filter(|&at| {
                    matches!(
                        &s.conversation.blocks[at].kind,
                        BlockKind::Permission(old) if old.tool_call_id == prompt.tool_call_id
                            && old.resolution.as_deref().is_none_or(|r| r.starts_with("allow"))
                    )
                })
                .filter(|_| child.is_none())
                .filter(|_| {
                    let tool_id = format!("tool:{}", prompt.tool_call_id);
                    !s.conversation.blocks.iter().any(|b| {
                        b.id == tool_id
                            && matches!(
                                &b.kind,
                                BlockKind::ToolCall {
                                    status: Some(_),
                                    ..
                                }
                            )
                    })
                });
            s.conversation.open_permissions.push(id.clone());
            match retarget {
                Some(at) => {
                    let old = std::mem::replace(
                        &mut s.conversation.blocks[at],
                        block(id, BlockKind::Permission(prompt)),
                    );
                    s.conversation
                        .open_permissions
                        .retain(|open| open != &old.id);
                }
                None => s
                    .conversation
                    .blocks
                    .push(block(id, BlockKind::Permission(prompt))),
            }
            if child.is_none() {
                s.activity = Activity::AwaitingUser;
            }
        }
        Some(EventKind::PermissionResolved) => {
            let v = e.payload;
            if let Some(child) = v["child_session_id"].as_str().and_then(|id| {
                s.children
                    .iter_mut()
                    .find(|c| c.session_id == id && !c.status.terminal() && c.child_seq.is_none())
            }) {
                child.status = ChildStatus::Running;
                child.event_seq = Some(e.seq);
            }
            let request_id = v["request_id"].as_str().ok_or_else(bad)?;
            let resolution = strip_controls(v["resolution"].as_str().ok_or_else(bad)?);
            let id = format!("permission:{request_id}");
            s.conversation.open_permissions.retain(|open| open != &id);
            super::clear_answered(s, request_id);
            if let Some(Block {
                kind: BlockKind::Permission(prompt),
                ..
            }) = s.conversation.blocks.iter_mut().find(|b| b.id == id)
            {
                prompt.message = v["message"].as_str().map(strip_controls);
                prompt.resolution = Some(resolution.clone());
            }
            let call_id = strip_controls(v["tool_call_id"].as_str().unwrap_or_default());
            if v["child_session_id"].is_null() {
                settle_held_tools(s, &call_id, &resolution);
            }
        }
        Some(EventKind::ContextCompacted) => {
            let v = e.payload;
            s.conversation.last_compaction = serde_json::from_value(v.clone()).ok();
            let tokens = |key: &str| super::tokens(v[key].as_u64().unwrap_or(0));
            s.conversation.blocks.push(block(
                format!("notice:{}", e.seq),
                BlockKind::Notice(format!(
                    "context compacted: ~{} -> ~{}",
                    tokens("estimate_before"),
                    tokens("estimate_after")
                )),
            ));
        }
        Some(EventKind::ModelChanged) => {
            let changed: ModelChanged = serde_json::from_value(e.payload).map_err(|_| bad())?;
            let new = &changed.new;
            s.selected_model_alias = Some(strip_controls(&new.alias));
            s.selected_model_name = Some(strip_controls(&new.model));
            s.context_window = Some(changed.window);
            s.variant = changed.variant.as_deref().map(strip_controls);
            s.model_seq = Some(e.seq);
            s.variant_seq = Some(e.seq);
            // Only the switch this selection came from stops waiting.
            if let Some(op) = changed.operation_id {
                super::settle_model_op(s, &op.to_string());
            }
            if changed.reason == ModelChangeReason::Switch {
                let from = changed
                    .previous
                    .as_ref()
                    .map(|p| strip_controls(&p.alias))
                    .unwrap_or_else(|| "—".into());
                let mut text = format!("model: {from} → {}", strip_controls(&new.alias));
                if let Some(variant) = &s.variant {
                    text.push_str(&format!(" · {variant}"));
                }
                text.push_str(&format!(" · {}", super::tokens(changed.window)));
                s.conversation
                    .blocks
                    .push(block(format!("notice:{}", e.seq), BlockKind::Notice(text)));
            }
        }
        Some(EventKind::SessionAgentChanged) => {
            let agent = e.payload["agent"].as_str().ok_or_else(bad)?;
            s.agent = Some(strip_controls(agent));
            s.agent_seq = Some(e.seq);
        }
        Some(EventKind::CommandResult) => {
            let v = e.payload;
            if v["name"] == "watchdog" {
                let text = v["data"]["message"].as_str().ok_or_else(bad)?;
                s.conversation.blocks.push(block(
                    e.seq.to_string(),
                    BlockKind::Notice(strip_controls(text)),
                ));
                return Ok(());
            }
            let name = v["name"].as_str().ok_or_else(bad)?.to_string();
            let ok = v["ok"].as_bool().ok_or_else(bad)?;
            // `/model`: the result of an operation settles the switch waiting on it (a deferred
            // acceptance excepted: that one keeps waiting). A switch already made shows as its
            // `model.changed` notice; a refusal says why, in the block below.
            if name == "model" {
                let data = v.get("data").filter(|d| !d.is_null());
                let deferred = ok
                    && data.is_some_and(|d| {
                        d["accepted"].as_bool() == Some(true) && d["applies"] == "after_turn"
                    });
                let made = ok && data.is_some_and(|d| d["changed"].as_bool() == Some(true));
                let op = v["operation_id"].as_str().map(str::to_string);
                if let (false, Some(op)) = (deferred, op.as_deref()) {
                    super::settle_model_op(s, op);
                }
                if made {
                    return Ok(());
                }
                if deferred {
                    // This tab's own switch, accepted: it waits for its operation (a replay of
                    // an acceptance already settled marks nothing).
                    let settled = op.as_ref().is_some_and(|o| s.settled_model_ops.contains(o));
                    if let Some(m) = s.model_switch.as_mut() {
                        if op.is_some() && m.operation_id == op && !settled {
                            m.deferred = true;
                        }
                    }
                    // Named only when it is this tab's switch, known by its operation id, or
                    // when the server names the alias; otherwise (another client's switch, or
                    // one whose answer has not arrived) the line stays generic.
                    let target = data
                        .and_then(|d| d["alias"].as_str())
                        .map(str::to_string)
                        .or_else(|| {
                            s.model_switch
                                .as_ref()
                                .filter(|m| op.is_some() && m.operation_id == op)
                                .map(|m| m.alias.clone())
                        });
                    let text = match target {
                        Some(alias) => format!("switches to {alias} after this turn"),
                        None => "model switch accepted, applies after this turn".into(),
                    };
                    s.conversation.blocks.push(block(
                        e.seq.to_string(),
                        BlockKind::Command {
                            name,
                            ok,
                            text: strip_controls(&text),
                        },
                    ));
                    return Ok(());
                }
            }
            // `/variant` answers with the variant in use, after a change or not.
            if name == "variant" && ok {
                if let Some(data) = v.get("data") {
                    if let Some(variant) = data.get("variant") {
                        s.variant = variant.as_str().map(strip_controls);
                        s.variant_seq = Some(e.seq);
                    }
                    super::set_variant_offer(s, data);
                }
            }
            let text = super::command::outcome_text(
                &name,
                ok,
                v.get("data").filter(|d| !d.is_null()),
                v["error"].as_str(),
            );
            s.conversation.blocks.push(block(
                e.seq.to_string(),
                BlockKind::Command {
                    name,
                    ok,
                    text: strip_controls(&text),
                },
            ));
        }
        Some(EventKind::ContextUpdated) => {
            let snapshot = serde_json::from_value(e.payload).map_err(|_| bad())?;
            keep_newer_context(s, snapshot);
        }
        Some(EventKind::AgentState) => {
            s.activity = match e.payload["to"].as_str().unwrap_or("") {
                "running" => Activity::Running,
                "yielded" if s.activity == Activity::OutOfSteps => Activity::OutOfSteps,
                "yielded" => Activity::AwaitingUser,
                "idle" => Activity::Idle,
                _ => s.activity,
            };
        }
        _ => {}
    }
    Ok(())
}

/// `code: message` with terminal controls removed, whitespace collapsed and the length capped.
fn error_text(code: &str, message: &str) -> String {
    const MAX_CHARS: usize = 300;
    let clean = strip_controls(&format!("{code}: {message}"));
    let flat = clean.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= MAX_CHARS {
        return flat;
    }
    let mut cut: String = flat.chars().take(MAX_CHARS).collect();
    cut.push('…');
    cut
}

fn step_block_id(s: &mut State, key: &StepKey, step_id: &str) -> String {
    s.conversation
        .step_blocks
        .entry(key.clone())
        .or_insert_with(|| format!("step:{step_id}"))
        .clone()
}

fn existing_step(blocks: &[Block], id: &str) -> (String, String) {
    for b in blocks {
        if let (true, BlockKind::AssistantStep { text, thinking, .. }) = (b.id == id, &b.kind) {
            return (text.clone(), thinking.clone());
        }
    }
    Default::default()
}

fn upsert(v: &mut Vec<Block>, id: String, kind: BlockKind) {
    if let Some(b) = v.iter_mut().find(|b| b.id == id) {
        b.kind = kind
    } else {
        v.push(block(id, kind))
    }
}

/// How many retired incarnations the conversation remembers.
const RETIRED_INCARNATIONS: usize = 8;

/// What a resync keeps of the gauge.
pub(super) struct ContextKept {
    snapshot: Option<ContextSnapshot>,
    epoch: u64,
    retired: std::collections::VecDeque<ulid::Ulid>,
}

pub(super) fn take_context(s: &mut State) -> ContextKept {
    ContextKept {
        snapshot: s.conversation.context.take(),
        epoch: s.conversation.context_epoch,
        retired: std::mem::take(&mut s.conversation.retired_incarnations),
    }
}

pub(super) fn restore_context(s: &mut State, kept: ContextKept) {
    s.conversation.context = kept.snapshot;
    s.conversation.context_epoch = kept.epoch;
    s.conversation.retired_incarnations = kept.retired;
}

/// Shows `snapshot` unless it is stale. Incarnation ids carry no order, so staleness is
/// decided here: within the incarnation shown, a lower revision is stale; a snapshot of a
/// new incarnation replaces the one shown, which is then retired; a snapshot of one of the
/// last eight retired incarnations is never shown again, however late it arrives (an older
/// HTTP answer is also dropped by its fetch epoch).
pub(super) fn keep_newer_context(s: &mut State, snapshot: ContextSnapshot) {
    let conversation = &mut s.conversation;
    if conversation
        .retired_incarnations
        .contains(&snapshot.incarnation)
    {
        return;
    }
    if let Some(held) = &conversation.context {
        if held.incarnation == snapshot.incarnation {
            if snapshot.revision < held.revision {
                return;
            }
        } else {
            conversation
                .retired_incarnations
                .push_back(held.incarnation);
            while conversation.retired_incarnations.len() > RETIRED_INCARNATIONS {
                conversation.retired_incarnations.pop_front();
            }
        }
    }
    conversation.context = Some(snapshot);
}

fn count_usage(s: &mut State, key: StepKey, u: Usage) {
    if s.conversation.counted_steps.insert(key.clone()) {
        s.conversation.session_tokens =
            Some(s.conversation.session_tokens.unwrap_or(0) + u.total_tokens);
        s.conversation.usage_by_step.insert(key, u);
    }
    s.conversation.context_tokens = Some(u.prompt_tokens);
    s.conversation.completion_tokens = Some(u.completion_tokens);
}

fn interrupt_block(b: &mut Block) {
    if let BlockKind::ToolCall { status, .. } = &mut b.kind {
        if status.is_none() {
            *status = Some(ToolStatus::Interrupted)
        }
    }
}

/// Settles the unfinished tools of `(actor, turn)` per the module rules.
fn settle_turn_tools(s: &mut State, actor: &str, turn: &str, outcome: &str) {
    let all = matches!(outcome, "interrupted" | "error");
    if !all && !matches!(outcome, "done" | "yielded") {
        return;
    }
    let yielded = outcome == "yielded";
    let ids = s
        .conversation
        .tools_by_turn
        .get(&(actor.to_string(), turn.to_string()))
        .cloned()
        .unwrap_or_default();
    // A turn that yields for a permission holds its gated call and the calls behind it: they run
    // (or not) once the request is answered, so none of them is settled here.
    if yielded && ids.iter().any(|id| open_permission_for(s, id)) {
        return;
    }
    for id in ids {
        if all || !s.conversation.pending_tools.contains_key(&id) {
            let bid = format!("tool:{id}");
            if let Some(b) = s.conversation.blocks.iter_mut().find(|b| b.id == bid) {
                // An `ask` has no pre/post pair: yielding is its execution, the question shows in
                // the yield block.
                if let BlockKind::ToolCall { name, status, .. } = &mut b.kind {
                    if yielded && name == "ask" && status.is_none() {
                        *status = Some(ToolStatus::Ok);
                        continue;
                    }
                }
                interrupt_block(b);
            }
        }
    }
}

/// Whether an open permission request holds the tool call `call_id`.
fn open_permission_for(s: &State, call_id: &str) -> bool {
    s.conversation.blocks.iter().any(
        |b| matches!(&b.kind, BlockKind::Permission(p) if p.is_open() && p.tool_call_id == call_id),
    )
}

/// Settles the tools a permission request held, when the server sends no `tool.post_call` for
/// them: a denied call is `Denied`; on a cancel or an expiry it and the calls behind it (still
/// planned in the same turn) are `Interrupted`. An allowed call runs and reports itself.
fn settle_held_tools(s: &mut State, call_id: &str, resolution: &str) {
    let ids: Vec<String> = match resolution {
        "deny" => vec![call_id.to_string()],
        "cancelled" | "expired" => s
            .conversation
            .tools_by_turn
            .values()
            .find(|ids| ids.iter().any(|id| id == call_id))
            .cloned()
            .unwrap_or_default(),
        _ => return,
    };
    for id in ids {
        let bid = format!("tool:{id}");
        if let Some(b) = s.conversation.blocks.iter_mut().find(|b| b.id == bid) {
            if let BlockKind::ToolCall { status, .. } = &mut b.kind {
                if status.is_none() {
                    *status = Some(if resolution == "deny" {
                        ToolStatus::Denied
                    } else {
                        ToolStatus::Interrupted
                    });
                }
            }
        }
    }
}

/// Creates or updates `tool:<id>`, owned by `(actor, turn)`; absent patch fields keep their value.
fn tool_merge(s: &mut State, actor: &str, turn: &str, id: String, p: ToolPatch) {
    if !s
        .conversation
        .tools_by_turn
        .values()
        .any(|ids| ids.contains(&id))
    {
        s.conversation
            .tools_by_turn
            .entry((actor.to_string(), turn.to_string()))
            .or_default()
            .push(id.clone());
    }
    let bid = format!("tool:{id}");
    if let Some(Block {
        kind:
            BlockKind::ToolCall {
                name,
                args,
                status,
                output,
                metadata,
                duration_ms,
                exit_code,
                ..
            },
        ..
    }) = s.conversation.blocks.iter_mut().find(|b| b.id == bid)
    {
        if let Some(n) = p.name {
            *name = n;
        }
        if let Some(a) = p.args {
            *args = a;
        }
        if p.status.is_some() {
            *status = p.status;
        }
        if let Some(o) = p.output {
            *output = o;
        }
        if let Some(m) = p.metadata {
            *metadata = m;
        }
        if p.duration_ms.is_some() {
            *duration_ms = p.duration_ms;
        }
        if p.exit_code.is_some() {
            *exit_code = p.exit_code;
        }
        return;
    }
    s.conversation.blocks.push(block(
        bid,
        BlockKind::ToolCall {
            id,
            name: p.name.unwrap_or_else(|| "tool".into()),
            args: p.args.unwrap_or(Value::Null),
            status: p.status,
            output: p.output.unwrap_or_default(),
            metadata: p.metadata.unwrap_or(Value::Null),
            duration_ms: p.duration_ms,
            exit_code: p.exit_code,
        },
    ));
}
