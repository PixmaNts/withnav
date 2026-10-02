//! Pure event projection and UI action transitions.
use super::{
    protocol::{
        Action, ClientEvent, CreateSessionRequest, Effect, Overlay, ProtocolError, SubmitRequest,
    },
    state::{Activity, Block, BlockKind, ConnectionStatus, State, UiState},
};
use crate::{
    bus::event::{EventEnvelope, EventKind},
    provider::{StreamDelta, ToolCall, Usage},
    tools::ToolStatus,
};
use serde::Deserialize;
use serde_json::Value;

fn generation(e: &ClientEvent) -> &super::protocol::Generation {
    match e {
        ClientEvent::Envelope { generation, .. }
        | ClientEvent::Resync { generation }
        | ClientEvent::StreamConnected { generation }
        | ClientEvent::StreamDisconnected { generation }
        | ClientEvent::Reconnecting { generation }
        | ClientEvent::Models { generation, .. }
        | ClientEvent::Sessions { generation, .. }
        | ClientEvent::Created { generation, .. }
        | ClientEvent::Resumed { generation, .. }
        | ClientEvent::Metadata { generation, .. }
        | ClientEvent::Submitted { generation, .. }
        | ClientEvent::Stopped { generation, .. }
        | ClientEvent::DeliveryUncertain { generation }
        | ClientEvent::ProtocolError { generation, .. } => generation,
    }
}

pub fn update(s: &mut State, e: ClientEvent) -> Result<Vec<Effect>, ProtocolError> {
    if generation(&e) != &s.generation {
        return Ok(vec![]);
    }
    let mut effects = vec![];
    match e {
        ClientEvent::Envelope { envelope, .. } => apply_envelope(s, envelope)?,
        ClientEvent::Resync { .. } => {
            let session = s.session.clone();
            *s = State {
                generation: s.generation.clone(),
                session,
                ..State::default()
            };
            effects.push(Effect::Subscribe {
                session_id: s
                    .session
                    .as_ref()
                    .map(|m| m.session_id.clone())
                    .unwrap_or_default(),
                cursor: None,
            });
        }
        ClientEvent::StreamConnected { .. } => s.connection = ConnectionStatus::Connected,
        ClientEvent::StreamDisconnected { .. } => s.connection = ConnectionStatus::Disconnected,
        ClientEvent::Reconnecting { .. } => s.connection = ConnectionStatus::Reconnecting,
        ClientEvent::ProtocolError { message, .. } => s.last_protocol_error = Some(message),
        ClientEvent::Models { result, .. } => {
            if let Ok(body) = result {
                s.catalog = body.models;
                if s.selected_model_alias.is_none() {
                    s.selected_model_alias = body
                        .default
                        .filter(|a| s.catalog.iter().any(|m| &m.alias == a))
                        .or_else(|| {
                            s.catalog
                                .iter()
                                .find(|m| m.alias == "default")
                                .map(|m| m.alias.clone())
                        });
                }
                if let Some(m) = s
                    .catalog
                    .iter()
                    .find(|m| Some(&m.alias) == s.selected_model_alias.as_ref())
                {
                    s.selected_model_name = Some(m.model.clone());
                    s.context_window = m.context_window;
                }
            }
        }
        ClientEvent::Sessions { result, .. } => {
            if let Ok(b) = result {
                s.sessions = b.sessions
            }
        }
        ClientEvent::Created { result, .. } | ClientEvent::Resumed { result, .. } => match result {
            Ok(r) => {
                s.session = Some(super::protocol::SessionMetadata {
                    session_id: r.session_id.clone(),
                    actor_state: r.actor_state,
                    workspace: String::new(),
                    model: s.selected_model_alias.clone().unwrap_or_default(),
                    current_seq: 0,
                });
                effects.push(Effect::FetchMetadata(r.session_id.clone()));
                effects.push(Effect::Subscribe {
                    session_id: r.session_id,
                    cursor: None,
                });
                if let Some(text) = s.pending_submit_text.take() {
                    effects.push(send_effect(text));
                }
            }
            Err(x) => s.notice = Some(x),
        },
        ClientEvent::Metadata { result, .. } => {
            if let Ok(m) = result {
                s.selected_model_alias = Some(m.model.clone());
                s.session = Some(m)
            }
        }
        ClientEvent::Submitted {
            revision, result, ..
        } => match result {
            Ok(r) => {
                effects.push(Effect::ClearEditor { revision });
                if let Some(n) = r.notice {
                    s.conversation
                        .blocks
                        .push(block("notice", BlockKind::Notice(n)));
                }
            }
            Err(x) => s
                .conversation
                .blocks
                .push(block("error", BlockKind::Error(x))),
        },
        ClientEvent::DeliveryUncertain { .. } => {
            s.delivery_uncertain = true;
            s.conversation.blocks.push(block(
                "delivery-uncertain",
                BlockKind::Notice(
                    "delivery uncertain; check the conversation before retrying".into(),
                ),
            ));
        }
        ClientEvent::Stopped { .. } => s.activity = Activity::Stopped,
    }
    Ok(effects)
}
fn block(id: impl Into<String>, kind: BlockKind) -> Block {
    Block {
        id: id.into(),
        kind,
    }
}
fn send_effect(text: String) -> Effect {
    Effect::Send(SubmitRequest {
        target_actor_id: "agent:main".into(),
        mode: "next-turn".into(),
        content: vec![crate::bus::command::ContentBlock::Text { text }],
    })
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
fn apply_envelope(s: &mut State, e: EventEnvelope) -> Result<(), ProtocolError> {
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
    match kind {
        Some(EventKind::SessionStart) => {
            s.activity = Activity::Idle;
        }
        Some(EventKind::SessionEnd) => {
            s.activity = Activity::Stopped;
            for b in &mut s.conversation.blocks {
                if let BlockKind::ToolCall { status, .. } = &mut b.kind {
                    if status.is_none() {
                        *status = Some(ToolStatus::Interrupted)
                    }
                }
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
            let key = (e.actor_id, p.turn_id.clone(), p.step_id.clone());
            s.conversation.current_step = Some(key.clone());
            let id = s
                .conversation
                .step_blocks
                .entry(key.clone())
                .or_insert_with(|| format!("step:{}", p.step_id))
                .clone();
            let mut text = String::new();
            let mut thinking = String::new();
            for b in &s.conversation.blocks {
                if b.id == id {
                    if let BlockKind::AssistantStep {
                        text: t,
                        thinking: th,
                        ..
                    } = &b.kind
                    {
                        text = t.clone();
                        thinking = th.clone()
                    }
                }
            }
            match p.delta {
                StreamDelta::Text { text: t } => text.push_str(&strip_controls(&t)),
                StreamDelta::Thinking { text: t } => thinking.push_str(&strip_controls(&t)),
                StreamDelta::ToolCallStarted { name, .. } => {
                    s.conversation
                        .pending_tool_fragments
                        .insert(name.clone(), String::new());
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
            let key = (e.actor_id, p.turn_id.clone(), p.step_id.clone());
            let id = s
                .conversation
                .step_blocks
                .entry(key.clone())
                .or_insert_with(|| format!("step:{}", p.step_id))
                .clone();
            s.conversation.step_turns.insert(key.clone(), p.turn_id);
            upsert(
                &mut s.conversation.blocks,
                id,
                BlockKind::AssistantStep {
                    text: strip_controls(&p.message.text),
                    thinking: strip_controls(&p.message.thinking),
                    revision: e.seq,
                },
            );
            for call in p.message.tool_calls {
                tool_upsert(s, call);
            }
            if let Some(u) = p.usage.or(p.message.usage) {
                count_usage(s, key, u)
            }
        }
        Some(EventKind::ToolPreCall) => {
            let v = e.payload;
            let id = v["tool_call_id"].as_str().ok_or_else(bad)?.to_string();
            let name = v["name"].as_str().ok_or_else(bad)?.to_string();
            s.conversation.pending_tools.insert(
                id.clone(),
                s.conversation.current_turn.clone().unwrap_or_default(),
            );
            tool_set(
                s,
                id,
                name,
                v["args"].clone(),
                None,
                String::new(),
                Value::Null,
                None,
            );
        }
        Some(EventKind::ToolPostCall) => {
            let v = e.payload;
            let id = v["tool_call_id"].as_str().ok_or_else(bad)?.to_string();
            let name = v["name"].as_str().unwrap_or("tool").to_string();
            let status: ToolStatus =
                serde_json::from_value(v["status"].clone()).map_err(|_| bad())?;
            let out = v["output"].as_str().unwrap_or("");
            let meta = v["metadata"].clone();
            let dur = meta["duration_ms"].as_u64();
            let exit = meta["exit_code"].as_i64().map(|x| x as i32);
            tool_set(
                s,
                id,
                name,
                Value::Null,
                Some(status),
                strip_controls(out),
                meta,
                dur,
            );
            if let Some(b) = s.conversation.blocks.last_mut() {
                if let BlockKind::ToolCall { exit_code, .. } = &mut b.kind {
                    *exit_code = exit;
                }
            }
        }
        Some(EventKind::AgentYield) => {
            let v = e.payload;
            let q = v["question"].as_str().ok_or_else(bad)?;
            s.activity = Activity::AwaitingUser;
            s.conversation.blocks.push(block(
                e.seq.to_string(),
                BlockKind::YieldQuestion(strip_controls(q)),
            ));
        }
        Some(EventKind::TurnEnd) => {
            let v = e.payload;
            s.activity = match v["outcome"].as_str().unwrap_or("") {
                "yielded" => Activity::AwaitingUser,
                "interrupted" | "done" | "error" => Activity::Idle,
                _ => s.activity,
            };
            if let Some(u) = v
                .get("usage")
                .and_then(|x| serde_json::from_value::<Usage>(x.clone()).ok())
            {
                if let Some(key) = s.conversation.current_step.clone() {
                    if !s.conversation.counted_steps.contains(&key) {
                        count_usage(s, key, u);
                    }
                }
            }
        }
        Some(EventKind::AgentState) => {
            s.activity = match e.payload["to"].as_str().unwrap_or("") {
                "running" => Activity::Running,
                "yielded" => Activity::AwaitingUser,
                "idle" => Activity::Idle,
                _ => s.activity,
            };
        }
        _ => {}
    }
    Ok(())
}
fn upsert(v: &mut Vec<Block>, id: String, kind: BlockKind) {
    if let Some(b) = v.iter_mut().find(|b| b.id == id) {
        b.kind = kind
    } else {
        v.push(block(id, kind))
    }
}
fn count_usage(s: &mut State, key: (String, String, String), u: Usage) {
    if s.conversation.counted_steps.insert(key.clone()) {
        s.conversation.session_tokens =
            Some(s.conversation.session_tokens.unwrap_or(0) + u.total_tokens);
        s.conversation.usage_by_step.insert(key, u);
    }
    s.conversation.context_tokens = Some(u.prompt_tokens);
    s.conversation.completion_tokens = Some(u.completion_tokens);
}
fn tool_upsert(s: &mut State, c: ToolCall) {
    tool_set(
        s,
        c.id,
        c.name,
        c.args,
        None,
        String::new(),
        Value::Null,
        None,
    )
}
#[allow(clippy::too_many_arguments)]
fn tool_set(
    s: &mut State,
    id: String,
    name: String,
    args: Value,
    status: Option<ToolStatus>,
    output: String,
    metadata: Value,
    duration_ms: Option<u64>,
) {
    let turn = s
        .conversation
        .pending_tools
        .get(&id)
        .cloned()
        .or_else(|| s.conversation.current_turn.clone())
        .unwrap_or_default();
    let key = ("agent:main".to_string(), turn);
    if !s
        .conversation
        .tools_by_turn
        .values()
        .any(|ids| ids.contains(&id))
    {
        s.conversation
            .tools_by_turn
            .entry(key)
            .or_default()
            .push(id.clone());
    }
    upsert(
        &mut s.conversation.blocks,
        format!("tool:{id}"),
        BlockKind::ToolCall {
            id,
            name,
            args,
            status,
            output,
            metadata,
            duration_ms,
            exit_code: None,
        },
    );
}
pub fn strip_controls(input: &str) -> String {
    let mut out = String::new();
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.next() {
                Some('[') => {
                    for x in chars.by_ref() {
                        if ('@'..='~').contains(&x) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(x) = chars.next() {
                        if x == '\x07' {
                            break;
                        }
                        if x == '\x1b' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                Some(_) | None => {}
            }
        } else if c == '\n' || c == '\t' || !c.is_control() {
            out.push(c)
        }
    }
    out
}

pub fn handle_action(s: &mut State, ui: &mut UiState, a: Action) -> Vec<Effect> {
    match a {
        Action::SubmitAt { text, .. } => handle_action(s, ui, Action::Submit(text)),
        Action::Submit(text) => {
            s.pending_submit_text = Some(text.clone());
            s.editor_revision = s.editor_revision.wrapping_add(1);
            if let Some(session) = &s.session {
                if s.activity == Activity::Stopped {
                    vec![Effect::Resume(session.session_id.clone())]
                } else {
                    vec![send_effect(text)]
                }
            } else {
                vec![Effect::Create(CreateSessionRequest {
                    workspace: String::new(),
                    model: s.selected_model_alias.clone(),
                    resume: None,
                })]
            }
        }
        Action::NewConversation => {
            s.session = None;
            s.conversation = Default::default();
            s.activity = Activity::Idle;
            vec![Effect::Unsubscribe]
        }
        Action::ResumeSession(id) => vec![Effect::Resume(id)],
        Action::PickModel(alias) => {
            s.selected_model_alias = Some(alias.clone());
            if let Some(m) = s.catalog.iter().find(|m| m.alias == alias) {
                s.selected_model_name = Some(m.model.clone());
                s.context_window = m.context_window;
            }
            if s.session.is_some() {
                s.session = None;
                s.conversation = Default::default();
                vec![Effect::Unsubscribe]
            } else {
                vec![]
            }
        }
        Action::Stop => s
            .session
            .as_ref()
            .map(|m| vec![Effect::Stop(m.session_id.clone())])
            .unwrap_or_default(),
        Action::Quit => vec![Effect::Quit],
        Action::OpenOverlay(o) => {
            ui.overlay = Some(o);
            match o {
                Overlay::Models => vec![Effect::FetchModels],
                Overlay::Sessions => vec![Effect::ListSessions],
                _ => vec![],
            }
        }
        Action::CloseOverlay => {
            ui.overlay = None;
            vec![]
        }
        Action::Select(_) => vec![],
        Action::Scroll(n) => {
            ui.scroll = ui.scroll.saturating_add_signed(n as i16);
            vec![]
        }
        Action::ToggleBlock(id) => {
            if !ui.expanded.insert(id.clone()) {
                ui.expanded.remove(&id);
            }
            vec![]
        }
        Action::SetMode(m) => {
            ui.mode = m;
            vec![]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strips_control_sequences() {
        assert_eq!(strip_controls("a\x1b[31mred\x1b[0m\x07\n\tb"), "ared\n\tb");
    }
}
