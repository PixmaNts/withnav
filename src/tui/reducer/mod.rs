//! Pure event projection and UI action transitions.
//!
//! Runtime contract (the runtime owns I/O, the reducer owns the generations):
//! - Every tab has its own generation, never shared with another tab; `start_conversation` gives
//!   the active tab a new one. An event reaches the tab whose generation it carries; an event whose
//!   generation no tab holds any more (an older conversation, a closed tab) is dropped. Catalog,
//!   session-list and settings results are shared and always applied.
//! - The effects returned by `update` belong to the tab the event reached; the effects returned by
//!   `handle_action` belong to the tab active after the action. The runtime stamps them with that
//!   tab's CURRENT generation and session, and stops the stream of a generation no tab holds.
//! - Before each action the runtime copies the editor text and revision into the active tab's
//!   `draft`/`draft_revision`; when another tab comes forward the reducer returns
//!   `Effect::SetEditor` with that tab's draft.
//! - `Effect::Resume(id)` carries only the id. The runtime owns the workspace (it fills
//!   `State.workspace` at startup) and builds `CreateSessionRequest { workspace, resume: Some(id) }`.
//! - `Resync` replays the same session on the SAME stream, so it does not bump the generation:
//!   the replayed envelopes carry the current generation and must be applied.
//! - `Effect::SendAt { revision }` must be echoed in `ClientEvent::Submitted { revision }`.
mod actions;
mod command;
mod envelope;
mod text;

pub use actions::{handle_action, short_id, show_variants};
pub use command::{outcome_text, tokens};
pub use text::strip_controls;

use super::{
    protocol::{
        AnswerError, ClientEvent, Effect, Generation, ProtocolError, SessionMetadata, SubmitRequest,
    },
    state::{Activity, Block, BlockKind, ConnectionStatus, State},
};

/// The generation an event is tagged with: the tab it belongs to.
pub fn generation(e: &ClientEvent) -> &Generation {
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
        | ClientEvent::Context { generation, .. }
        | ClientEvent::Submitted { generation, .. }
        | ClientEvent::Commands { generation, .. }
        | ClientEvent::PermissionAnswered { generation, .. }
        | ClientEvent::CommandRan { generation, .. }
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

/// Switches the active tab to a different conversation: resets the projection, drops the session
/// and any unsent submit of the old one, and gives the tab a new generation so in-flight results
/// are dropped.
pub(crate) fn start_conversation(s: &mut State) {
    reset_projection(s);
    s.session = None;
    s.resuming = None;
    s.agent_seq = None;
    s.variant_seq = None;
    s.variant_offer = None;
    s.variant_list_wanted = false;
    s.pending_submit_text = None;
    s.pending_submit = None;
    s.delivery_uncertain = false;
    s.unread = false;
    let generation = Generation(s.issue());
    s.generation = generation;
}

/// Keeps what a `/variant` answer says the conversation offers.
pub(crate) fn set_variant_offer(s: &mut State, data: &serde_json::Value) {
    if let Some(offered) = data["offered"].as_array() {
        s.variant_offer = Some(
            offered
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(text::strip_controls)
                .collect(),
        );
    }
}

/// Refreshes the display name and context window from the catalog for the selected alias.
pub(crate) fn refresh_model(s: &mut State) {
    let State {
        tabs,
        active,
        catalog,
        ..
    } = s;
    refresh_tab_model(catalog, &mut tabs[*active]);
}

fn refresh_tab_model(catalog: &[super::protocol::ModelEntry], tab: &mut super::state::Tab) {
    let Some(alias) = tab.selected_model_alias.clone() else {
        return;
    };
    let found = catalog.iter().find(|m| m.alias == alias);
    tab.selected_model_name = found.map(|m| m.model.clone());
    tab.context_window = found.and_then(|m| m.context_window);
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
    let shared = matches!(
        e,
        ClientEvent::Models { .. }
            | ClientEvent::Sessions { .. }
            | ClientEvent::Commands { .. }
            | ClientEvent::SettingsNotSaved { .. }
    );
    if shared {
        return apply(s, e, false);
    }
    let Some(index) = s.tab_of(generation(&e)) else {
        return Ok(vec![]);
    };
    let background = index != s.active;
    // A gauge refresh is not news: only conversation events mark a background tab unread.
    if background
        && matches!(&e, ClientEvent::Envelope { envelope, .. }
            if envelope.kind != crate::bus::event::EventKind::ContextUpdated.as_str())
    {
        s.tabs[index].unread = true;
    }
    // The event is applied with its tab in front, then the tab on screen comes back.
    let shown = std::mem::replace(&mut s.active, index);
    let result = apply(s, e, background);
    s.active = shown;
    result
}

/// Applies `e` to the active tab; `background` tells that the driver is looking at another one.
fn apply(s: &mut State, e: ClientEvent, background: bool) -> Result<Vec<Effect>, ProtocolError> {
    let mut effects = vec![];
    match e {
        ClientEvent::Envelope { envelope, .. } => envelope::apply_envelope(s, envelope)?,
        ClientEvent::Resync { .. } => {
            // Keeps catalog, model, session list, pending submit, notice and the session itself.
            // The gauge is server state, not derived from the replay (`context.updated` is
            // live-only), so it stays; the reconnect that follows refreshes it.
            let context = envelope::take_context(s);
            reset_projection(s);
            envelope::restore_context(s, context);
            effects.push(Effect::Subscribe {
                session_id: s
                    .session
                    .as_ref()
                    .map(|m| m.session_id.clone())
                    .unwrap_or_default(),
                cursor: None,
            });
        }
        ClientEvent::StreamConnected { .. } => {
            s.connection = ConnectionStatus::Connected;
            // `context.updated` is live-only: what was missed before this subscription (a
            // reconnect, or the gap since the session was created) is read back here.
            s.conversation.context_epoch += 1;
            if let Some(m) = &s.session {
                effects.push(Effect::FetchContext {
                    session_id: m.session_id.clone(),
                    epoch: s.conversation.context_epoch,
                });
            }
        }
        ClientEvent::StreamDisconnected { .. } => s.connection = ConnectionStatus::Disconnected,
        ClientEvent::Reconnecting { .. } => s.connection = ConnectionStatus::Reconnecting,
        ClientEvent::ProtocolError { message, .. } => s.last_protocol_error = Some(message),
        ClientEvent::Models { result, .. } => {
            if let Ok(body) = result {
                s.catalog = body.models;
                let default = body
                    .default
                    .filter(|a| s.catalog.iter().any(|m| &m.alias == a))
                    .or_else(|| {
                        s.catalog
                            .iter()
                            .find(|m| m.alias == "default")
                            .map(|m| m.alias.clone())
                    });
                // Every tab, on screen or not: a default where none is chosen, and the display
                // name and window refreshed from the new catalog.
                let State { tabs, catalog, .. } = s;
                for tab in tabs {
                    if tab.selected_model_alias.is_none() {
                        tab.selected_model_alias = default.clone();
                    }
                    refresh_tab_model(catalog, tab);
                }
            }
        }
        ClientEvent::PermissionAnswered {
            request_id, result, ..
        } => {
            s.answering_permission = None;
            match result {
                // The prompt clears when `permission.resolved` arrives on the stream.
                Ok(()) => {}
                Err(AnswerError::Stale) => {
                    let id = format!("permission:{request_id}");
                    s.conversation.open_permissions.retain(|open| open != &id);
                    push_local(
                        s,
                        BlockKind::Notice(
                            "that permission request was no longer waiting; nothing was changed"
                                .into(),
                        ),
                    );
                }
                Err(AnswerError::Other(error)) => push_local(
                    s,
                    BlockKind::Notice(format!(
                        "the answer to the permission request did not reach the server: {error}"
                    )),
                ),
            }
        }
        ClientEvent::Commands { result, .. } => {
            if let Ok(body) = result {
                s.server_commands = body.commands;
            }
        }
        ClientEvent::CommandRan {
            name,
            revision,
            result,
            ..
        } => match result {
            // The outcome itself arrives as `command.result` on the stream.
            Ok(response) => {
                if name == "variant" {
                    if let Some(data) = response.data.as_ref().filter(|_| response.ok) {
                        set_variant_offer(s, data);
                    }
                    if std::mem::take(&mut s.variant_list_wanted) && response.ok && !background {
                        // The offer and the variant in use come from the same answer, which may
                        // arrive before its `command.result` on the stream.
                        let current = response
                            .data
                            .as_ref()
                            .and_then(|d| d["variant"].as_str())
                            .map(text::strip_controls);
                        effects.push(Effect::ShowVariants { current });
                    }
                }
                if let Some(revision) = revision {
                    if background {
                        if s.draft_revision == revision {
                            s.draft.clear();
                        }
                    } else {
                        effects.push(Effect::ClearEditor { revision });
                    }
                }
            }
            Err(error) => {
                if name == "variant" {
                    s.variant_list_wanted = false;
                }
                push_local(
                    s,
                    BlockKind::Command {
                        name,
                        ok: false,
                        text: error,
                    },
                )
            }
        },
        ClientEvent::Sessions { result, .. } => {
            if let Ok(b) = result {
                s.sessions = b.sessions
            }
        }
        ClientEvent::Created { result, .. } | ClientEvent::Resumed { result, .. } => match result {
            Ok(r) => {
                s.resuming = None;
                // A fetch issued before this (re)start may describe the old runtime.
                s.conversation.context_epoch += 1;
                s.session = Some(SessionMetadata {
                    session_id: r.session_id.clone(),
                    actor_state: r.actor_state,
                    workspace: s.workspace.clone(),
                    model: s.selected_model_alias.clone().unwrap_or_default(),
                    agent: s.agent.clone(),
                    variant: None,
                    current_seq: s.last_applied_seq.unwrap_or(0),
                });
                if s.activity == Activity::Stopped {
                    s.activity = Activity::Idle;
                }
                effects.push(Effect::FetchMetadata(r.session_id.clone()));
                // `None` for a fresh conversation; the kept cursor when resuming the same one.
                effects.push(Effect::Subscribe {
                    session_id: r.session_id.clone(),
                    cursor: s.last_applied_seq,
                });
                if let Some(text) = s.pending_submit_text.take() {
                    let revision = s.pending_submit.take().unwrap_or(0);
                    effects.push(send_effect(text, revision));
                }
            }
            Err(x) => {
                s.resuming = None;
                s.pending_submit_text = None;
                s.pending_submit = None;
                s.notice = Some(x)
            }
        },
        ClientEvent::Metadata { result, .. } => {
            if let Ok(m) = result {
                // Only reached for the current generation, so `m` describes this conversation.
                // A summary read before a change the stream already confirmed is older than it.
                let newer = |seq: Option<u64>| seq.is_none_or(|seq| m.current_seq >= seq);
                if m.agent.is_some() && newer(s.agent_seq) {
                    s.agent = m.agent.clone();
                }
                if newer(s.variant_seq) {
                    s.variant = m.variant.clone();
                }
                s.selected_model_alias = Some(m.model.clone());
                refresh_model(s);
                s.session = Some(m)
            }
        }
        // An older server has no endpoint: the gauge stays on the provider's count.
        ClientEvent::Context { epoch, result, .. } => {
            if let Ok(snapshot) = result {
                if epoch == s.conversation.context_epoch {
                    envelope::keep_newer_context(s, snapshot);
                }
            }
        }
        ClientEvent::Submitted {
            revision, result, ..
        } => match result {
            Ok(r) => {
                if !background {
                    effects.push(Effect::ClearEditor { revision });
                } else if s.draft_revision == revision {
                    // The tab is not on screen: its kept draft is the editor text that was sent.
                    s.draft.clear();
                }
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
