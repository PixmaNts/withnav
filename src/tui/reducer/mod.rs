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

pub use actions::{handle_action, occupied, short_id, show_variants, switch_blocked};
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
        ClientEvent::ChildEnvelope { generation, .. } => generation,
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
        | ClientEvent::ChildControlled { generation, .. }
        | ClientEvent::DeliveryUncertain { generation }
        | ClientEvent::ProtocolError { generation, .. }
        | ClientEvent::Git { generation, .. }
        | ClientEvent::SettingsNotSaved { generation, .. } => generation,
    }
}

pub(crate) fn block(id: impl Into<String>, kind: BlockKind) -> Block {
    Block {
        id: id.into(),
        kind,
    }
}

/// Asks for a read of the workspace's git state, unless one is running: then one more is
/// asked for when it returns, so reads never pile up.
pub(crate) fn read_git(s: &mut State) -> Option<Effect> {
    if s.git_reading {
        s.git_again = true;
        return None;
    }
    s.git_reading = true;
    Some(Effect::ReadGit)
}

/// The settled `/model` operations kept, at most.
const SETTLED_MODEL_OPS: usize = 16;

/// Records that the `/model` operation `id` is settled; ends the switch waiting on it.
pub(crate) fn settle_model_op(s: &mut State, id: &str) {
    if !s.settled_model_ops.iter().any(|o| o == id) {
        s.settled_model_ops.push_back(id.to_string());
        while s.settled_model_ops.len() > SETTLED_MODEL_OPS {
            s.settled_model_ops.pop_front();
        }
    }
    if s.model_switch
        .as_ref()
        .is_some_and(|m| m.operation_id.as_deref() == Some(id))
    {
        s.model_switch = None;
    }
}

/// The HTTP answer to this client's `/model` request `token`: it names the operation the switch
/// waits on. An acceptance for after the turn waits for that operation to settle on the stream,
/// unless it already did; anything else is settled now. The answer to an older request, replaced
/// since, changes nothing.
fn model_answered(s: &mut State, token: u64, response: &crate::tui::protocol::CommandResponse) {
    let id = response.operation_id.clone();
    let deferred = response.ok
        && response
            .data
            .as_ref()
            .is_some_and(|d| d["accepted"].as_bool() == Some(true) && d["applies"] == "after_turn");
    let settled = s.settled_model_ops.contains(&id);
    let Some(switch) = s
        .model_switch
        .as_mut()
        .filter(|m| m.token == token && m.operation_id.is_none())
    else {
        return;
    };
    if deferred && !settled {
        switch.operation_id = Some(id);
        switch.deferred = true;
    } else {
        s.model_switch = None;
    }
}

/// Forgets that an answer was sent for `request_id`, if it was the one.
pub(crate) fn clear_answered(s: &mut State, request_id: &str) {
    if s.answered_permission.as_deref() == Some(request_id) {
        s.answered_permission = None;
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
    s.children.clear();
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
    s.parent_session_id = None;
    s.agent_seq = None;
    s.variant_seq = None;
    s.model_seq = None;
    s.model_switch = None;
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

/// Refreshes the display name and context window from the catalog for the selected alias, for a
/// tab whose conversation has not reported its selection yet. Once a `model.changed` committed it,
/// the server's identity and window stay: the catalog only fills the picker.
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
    if tab.model_seq.is_some() {
        return;
    }
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
        ClientEvent::ChildEnvelope {
            child_session_id,
            envelope,
            ..
        } => {
            if let Some(child) = s
                .children
                .iter_mut()
                .find(|c| c.session_id == child_session_id)
            {
                if child.child_seq.is_none_or(|seq| envelope.seq > seq) && !child.status.terminal()
                {
                    child.child_seq = Some(envelope.seq);
                    child.status = match envelope.kind.as_str() {
                        "turn.start" => super::state::ChildStatus::Running,
                        "agent.yield" if envelope.payload["kind"] == "budget" => {
                            super::state::ChildStatus::OutOfSteps
                        }
                        "permission.requested" => super::state::ChildStatus::Permission,
                        "permission.resolved" => super::state::ChildStatus::Running,
                        _ => child.status,
                    };
                }
            }
        }
        ClientEvent::Envelope { envelope, .. } => {
            let spawned = (envelope.kind == "subagent.spawned")
                .then(|| {
                    envelope.payload["child_session_id"]
                        .as_str()
                        .map(str::to_string)
                })
                .flatten();
            // A turn may have changed files: the branch and the changes are read again.
            let turn_end = envelope.kind == crate::bus::event::EventKind::TurnEnd.as_str();
            let child_status = if s.read_only() {
                match envelope.kind.as_str() {
                    "turn.start" => Some(super::state::ChildStatus::Running),
                    "agent.yield" if envelope.payload["kind"] == "budget" => {
                        Some(super::state::ChildStatus::OutOfSteps)
                    }
                    "permission.requested" => Some(super::state::ChildStatus::Permission),
                    "permission.resolved" => Some(super::state::ChildStatus::Running),
                    _ => None,
                }
            } else {
                None
            };
            envelope::apply_envelope(s, envelope)?;
            if let Some(child) = spawned {
                effects.push(Effect::WatchChild(child));
            }
            if let Some(status) = child_status {
                s.child_status = status;
                let child_id = s
                    .session
                    .as_ref()
                    .map(|m| m.session_id.clone())
                    .or(s.resuming.clone());
                if let Some(child_id) = child_id {
                    for tab in &mut s.tabs {
                        if let Some(child) = tab
                            .children
                            .iter_mut()
                            .find(|c| c.session_id == child_id && !c.status.terminal())
                        {
                            child.status = status;
                        }
                    }
                }
            }
            if turn_end {
                effects.extend(read_git(s));
            }
        }
        ClientEvent::Git { status, .. } => {
            s.git = status;
            s.git_reading = false;
            if std::mem::take(&mut s.git_again) {
                effects.extend(read_git(s));
            }
        }
        ClientEvent::Resync { .. } => {
            // Keeps catalog, model, session list, pending submit, notice and the session itself.
            // The gauge is server state, not derived from the replay (`context.updated` is
            // live-only), so it stays; the reconnect that follows refreshes it.
            let context = envelope::take_context(s);
            reset_projection(s);
            // The replay says again whether the switch still waits: hidden until its acceptance
            // comes back and nothing settles it.
            if let Some(m) = s.model_switch.as_mut() {
                m.deferred = false;
            }
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
            match result {
                // The request stays answered until `permission.resolved` arrives on the stream
                // (which may already have come).
                Ok(()) => {}
                Err(AnswerError::Stale) => {
                    clear_answered(s, &request_id);
                    // Not a decision: the prompt is marked, the server's resolution says how
                    // it ended when it arrives.
                    let id = format!("permission:{request_id}");
                    s.conversation.open_permissions.retain(|open| open != &id);
                    if let Some(BlockKind::Permission(p)) = s
                        .conversation
                        .blocks
                        .iter_mut()
                        .find(|b| b.id == id)
                        .map(|b| &mut b.kind)
                    {
                        p.stale = true;
                    }
                    push_local(
                        s,
                        BlockKind::Notice(
                            "that permission request was no longer waiting; nothing was changed"
                                .into(),
                        ),
                    );
                }
                Err(AnswerError::Other(error)) => {
                    // Not taken: the keys answer it again.
                    clear_answered(s, &request_id);
                    push_local(
                        s,
                        BlockKind::Notice(format!(
                            "the answer to the permission request did not reach the server: {error}"
                        )),
                    )
                }
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
            token,
            result,
            ..
        } => match result {
            // The outcome itself arrives as `command.result` on the stream.
            Ok(response) => {
                if name == "model" {
                    model_answered(s, token, &response);
                }
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
                // The command never reached the server, or its answer was lost: the switch it
                // asked for is not waiting. An older request failing leaves a newer one alone.
                if name == "model" && s.model_switch.as_ref().is_some_and(|m| m.token == token) {
                    s.model_switch = None;
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
                s.sessions = b.sessions;
                seed_children(s);
                effects.extend(
                    s.children
                        .iter()
                        .filter(|c| !c.status.terminal())
                        .map(|c| Effect::WatchChild(c.session_id.clone())),
                );
            }
        }
        ClientEvent::Created { result, .. } | ClientEvent::Resumed { result, .. } => match result {
            Ok(r) => {
                s.resuming = None;
                // A fresh runtime has no switch waiting for a turn.
                s.model_switch = None;
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
                effects.extend(read_git(s));
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
                if s.read_only() {
                    s.resuming = None;
                }
                // Only reached for the current generation, so `m` describes this conversation.
                // A summary read before a change the stream already confirmed is older than it.
                let newer = |seq: Option<u64>| seq.is_none_or(|seq| m.current_seq >= seq);
                if m.agent.is_some() && newer(s.agent_seq) {
                    s.agent = m.agent.clone();
                }
                if newer(s.variant_seq) {
                    s.variant = m.variant.clone();
                }
                if newer(s.model_seq) {
                    s.selected_model_alias = Some(m.model.clone());
                    refresh_model(s);
                }
                s.session = Some(m);
                seed_children(s);
                effects.extend(
                    s.children
                        .iter()
                        .filter(|c| !c.status.terminal())
                        .map(|c| Effect::WatchChild(c.session_id.clone())),
                );
            } else if s.read_only() {
                if let Err(error) = result {
                    push_local(s, BlockKind::Error(error));
                }
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
        ClientEvent::ChildControlled {
            child_session_id,
            control,
            result,
            ..
        } => match result {
            Ok(()) => {
                let text = match control {
                    super::protocol::ChildControl::Stop => {
                        "stop accepted; waiting for the subagent to finish"
                    }
                    super::protocol::ChildControl::Continue => {
                        "continue accepted; waiting for the subagent"
                    }
                };
                push_local(s, BlockKind::Notice(text.into()));
            }
            Err(error) => push_local(
                s,
                BlockKind::Error(format!(
                    "subagent {child_session_id}: {}",
                    strip_controls(&error)
                )),
            ),
        },
        ClientEvent::Stopped { result, .. } => match result {
            Ok(response) if response.accepted => {
                s.activity = Activity::Stopped;
                s.model_switch = None;
            }
            Ok(response) => push_local(
                s,
                BlockKind::Error(format!(
                    "stop was refused: {}",
                    strip_controls(&response.actor_state)
                )),
            ),
            Err(error) => push_local(s, BlockKind::Error(strip_controls(&error))),
        },
        ClientEvent::SettingsNotSaved { message, .. } => push_local(s, BlockKind::Notice(message)),
    }
    Ok(effects)
}

fn seed_children(s: &mut State) {
    for tab in &mut s.tabs {
        let Some(parent) = tab
            .session
            .as_ref()
            .map(|m| &m.session_id)
            .or(tab.resuming.as_ref())
        else {
            continue;
        };
        for entry in s
            .sessions
            .iter()
            .filter(|e| e.parent_session_id.as_ref() == Some(parent))
        {
            if let Some(child) = tab.children.iter_mut().find(|c| c.session_id == entry.id) {
                if child.event_seq.is_none() {
                    child.status = super::state::ChildStatus::from_wire(
                        entry.child_status.as_deref().unwrap_or(""),
                    );
                }
            } else {
                tab.children.push(super::state::ChildAgent {
                    session_id: entry.id.clone(),
                    name: entry.agent.clone().unwrap_or_default(),
                    status: super::state::ChildStatus::from_wire(
                        entry.child_status.as_deref().unwrap_or(""),
                    ),
                    ..Default::default()
                });
            }
        }
    }
}
