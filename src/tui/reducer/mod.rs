//! Pure event projection and UI action transitions.
//!
//! Runtime contract (the runtime owns I/O, the reducer owns `State.generation`):
//! - `start_conversation` bumps `State.generation`. After every `update` / `handle_action` call the
//!   runtime must stamp the `ClientEvent`s of the effects it spawns with the CURRENT
//!   `State.generation`; results of older generations are dropped.
//! - `Effect::Resume(id)` carries only the id. The runtime owns the workspace (it fills
//!   `State.workspace` at startup) and builds `CreateSessionRequest { workspace, resume: Some(id) }`.
//! - `Resync` replays the same session on the SAME stream, so it does not bump the generation:
//!   the replayed envelopes carry the current generation and must be applied.
//! - `Effect::SendAt { revision }` must be echoed in `ClientEvent::Submitted { revision }`.
mod actions;
mod envelope;
mod text;

pub use actions::handle_action;
pub use text::strip_controls;

use super::{
    protocol::{ClientEvent, Effect, Generation, ProtocolError, SessionMetadata, SubmitRequest},
    state::{Activity, Block, BlockKind, ConnectionStatus, State},
};

fn generation(e: &ClientEvent) -> &Generation {
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
        | ClientEvent::ProtocolError { generation, .. }
        | ClientEvent::SettingsNotSaved { generation, .. } => generation,
    }
}

pub(crate) fn block(id: impl Into<String>, kind: BlockKind) -> Block {
    Block {
        id: id.into(),
        kind,
    }
}

/// Appends a reducer-made block with a unique, stable id (`local:<n>`).
pub(crate) fn push_local(s: &mut State, kind: BlockKind) {
    s.local_block_seq += 1;
    let id = format!("local:{}", s.local_block_seq);
    s.conversation.blocks.push(block(id, kind));
}

/// Clears the conversation projection and the cursor; the replay re-derives everything.
fn reset_projection(s: &mut State) {
    s.conversation = Default::default();
    s.last_applied_seq = None;
    s.latest_envelope = None;
    s.activity = Activity::Idle;
}

/// Switches to a different conversation: resets the projection, drops the session and any
/// unsent submit of the old one, and bumps the generation so in-flight results are dropped.
pub(crate) fn start_conversation(s: &mut State) {
    reset_projection(s);
    s.session = None;
    s.pending_submit_text = None;
    s.pending_submit = None;
    s.delivery_uncertain = false;
    s.generation = Generation(s.generation.0 + 1);
}

/// Refreshes the display name and context window from the catalog for the selected alias.
pub(crate) fn refresh_model(s: &mut State) {
    let Some(alias) = s.selected_model_alias.clone() else {
        return;
    };
    let found = s.catalog.iter().find(|m| m.alias == alias);
    s.selected_model_name = found.map(|m| m.model.clone());
    s.context_window = found.and_then(|m| m.context_window);
}

pub(crate) fn send_effect(text: String, revision: u64) -> Effect {
    Effect::SendAt {
        request: SubmitRequest {
            target_actor_id: "agent:main".into(),
            mode: "next-turn".into(),
            content: vec![crate::bus::command::ContentBlock::Text { text }],
        },
        revision,
    }
}

pub fn update(s: &mut State, e: ClientEvent) -> Result<Vec<Effect>, ProtocolError> {
    if generation(&e) != &s.generation {
        return Ok(vec![]);
    }
    let mut effects = vec![];
    match e {
        ClientEvent::Envelope { envelope, .. } => envelope::apply_envelope(s, envelope)?,
        ClientEvent::Resync { .. } => {
            // Keeps catalog, model, session list, pending submit, notice and the session itself.
            reset_projection(s);
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
                refresh_model(s);
            }
        }
        ClientEvent::Sessions { result, .. } => {
            if let Ok(b) = result {
                s.sessions = b.sessions
            }
        }
        ClientEvent::Created { result, .. } | ClientEvent::Resumed { result, .. } => match result {
            Ok(r) => {
                s.session = Some(SessionMetadata {
                    session_id: r.session_id.clone(),
                    actor_state: r.actor_state,
                    workspace: s.workspace.clone(),
                    model: s.selected_model_alias.clone().unwrap_or_default(),
                    current_seq: s.last_applied_seq.unwrap_or(0),
                });
                if s.activity == Activity::Stopped {
                    s.activity = Activity::Idle;
                }
                effects.push(Effect::FetchMetadata(r.session_id.clone()));
                // `None` for a fresh conversation; the kept cursor when resuming the same one.
                effects.push(Effect::Subscribe {
                    session_id: r.session_id,
                    cursor: s.last_applied_seq,
                });
                if let Some(text) = s.pending_submit_text.take() {
                    let revision = s.pending_submit.take().unwrap_or(0);
                    effects.push(send_effect(text, revision));
                }
            }
            Err(x) => {
                s.pending_submit_text = None;
                s.pending_submit = None;
                s.notice = Some(x)
            }
        },
        ClientEvent::Metadata { result, .. } => {
            if let Ok(m) = result {
                // Only reached for the current generation, so `m` describes this conversation.
                s.selected_model_alias = Some(m.model.clone());
                refresh_model(s);
                s.session = Some(m)
            }
        }
        ClientEvent::Submitted {
            revision, result, ..
        } => match result {
            Ok(r) => {
                effects.push(Effect::ClearEditor { revision });
                if let Some(n) = r.notice {
                    push_local(s, BlockKind::Notice(n));
                }
            }
            Err(x) => push_local(s, BlockKind::Error(x)),
        },
        ClientEvent::DeliveryUncertain { .. } => {
            s.delivery_uncertain = true;
            push_local(
                s,
                BlockKind::Notice(
                    "delivery uncertain; check the conversation before retrying".into(),
                ),
            );
        }
        ClientEvent::Stopped { .. } => s.activity = Activity::Stopped,
        ClientEvent::SettingsNotSaved { message, .. } => push_local(s, BlockKind::Notice(message)),
    }
    Ok(effects)
}
