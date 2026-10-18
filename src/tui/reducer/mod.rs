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

pub use actions::{handle_action, occupied, short_id, show_variants, switch_blocked};
pub use withnav_client_state::{outcome_text, strip_controls, tokens};

use withnav_client_state::{
    EnvelopeContext, apply_envelope, apply_selection, block, clear_answered, keep_bypass,
    keep_newer_context, keep_newer_mcp, model_answered, restore_context, set_variant_offer,
    take_context,
};

use super::{
    protocol::{
        AnswerError, ClientEvent, Effect, Generation, ProtocolError, SessionMetadata, SubmitRequest,
    },
    state::{Activity, BlockKind, ConnectionStatus, State},
};

/// The generation an event is tagged with: the tab it belongs to.
pub fn generation(e: &ClientEvent) -> &Generation {
    match e {
        ClientEvent::BypassSet { generation, .. } => generation,
        ClientEvent::ConfigLoaded { generation, .. }
        | ClientEvent::ConfigWritten { generation, .. }
        | ClientEvent::GatewayTested { generation, .. } => generation,
        ClientEvent::Agents { generation, .. } | ClientEvent::Layers { generation, .. } => {
            generation
        }
        ClientEvent::Mcp { generation, .. } => generation,
        ClientEvent::ChildEnvelope { generation, .. } => generation,
        ClientEvent::Envelope { generation, .. }
        | ClientEvent::Resync { generation }
        | ClientEvent::StreamConnected { generation }
        | ClientEvent::StreamDisconnected { generation }
        | ClientEvent::Reconnecting { generation }
        | ClientEvent::Models { generation, .. }
        | ClientEvent::Skills { generation, .. }
        | ClientEvent::Sessions { generation, .. }
        | ClientEvent::SessionDeleted { generation, .. }
        | ClientEvent::Created { generation, .. }
        | ClientEvent::Resumed { generation, .. }
        | ClientEvent::Metadata { generation, .. }
        | ClientEvent::Context { generation, .. }
        | ClientEvent::Submitted { generation, .. }
        | ClientEvent::Commands { generation, .. }
        | ClientEvent::PermissionAnswered { generation, .. }
        | ClientEvent::QuestionAnswered { generation, .. }
        | ClientEvent::CommandRan { generation, .. }
        | ClientEvent::Stopped { generation, .. }
        | ClientEvent::ChildControlled { generation, .. }
        | ClientEvent::DeliveryUncertain { generation }
        | ClientEvent::ProtocolError { generation, .. }
        | ClientEvent::Git { generation, .. }
        | ClientEvent::SettingsNotSaved { generation, .. } => generation,
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

/// Appends a reducer-made block with a unique, stable id (`local:<n>`).
pub(crate) fn push_local(s: &mut State, kind: BlockKind) {
    s.local_block_seq += 1;
    let id = format!("local:{}", s.local_block_seq);
    s.conversation.blocks.push(block(id, kind));
}

/// Clears the conversation projection and the cursor; the replay re-derives everything.
fn reset_projection(s: &mut State) {
    s.watchdog_paused = false;
    s.pause_token = None;
    s.conversation = Default::default();
    s.children.clear();
    s.last_applied_seq = None;
    s.activity = Activity::Idle;
}

fn clear_pending_session(s: &mut State) {
    s.resuming = None;
    s.pending_submit_text = None;
    s.pending_submit = None;
}

/// Switches the active tab to a different conversation: resets the projection, drops the session
/// and any unsent submit of the old one, and gives the tab a new generation so in-flight results
/// are dropped.
pub(crate) fn start_conversation(s: &mut State) {
    s.bypass = false;
    s.bypass_seq = 0;
    s.bypass_pending = false;
    reset_projection(s);
    s.session = None;
    s.resuming = None;
    s.inspecting = false;
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

/// Refreshes the display name and context window from the catalog for the selected model_id, for a
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
    let Some(model_id) = tab.selected_model_id.clone() else {
        return;
    };
    let found = catalog.iter().find(|m| m.name == model_id);
    tab.selected_model_name = found.map(|m| m.name.clone());
    tab.context_window = found.and_then(|m| m.context_window);
    refresh_start_variant(tab, catalog);
}

pub(super) fn refresh_start_variant(
    tab: &mut super::state::Tab,
    catalog: &[super::protocol::ModelEntry],
) {
    if occupied(tab) {
        return;
    }
    let (base, source) = if let Some(model) = &tab.start_model {
        (
            catalog
                .iter()
                .find(|m| &m.name == model)
                .and_then(|m| m.default_variant.clone()),
            "model_default".to_string(),
        )
    } else {
        tab.agents
            .iter()
            .find(|a| a.id == tab.agent_name())
            .and_then(|a| a.effective.as_ref())
            .map(|selection| (selection.variant.clone(), selection.variant_source.clone()))
            .unwrap_or((None, "model_default".into()))
    };
    tab.variant = tab.start_variant.clone().unwrap_or(base);
    tab.variant_source = if tab.start_variant.is_some() {
        "override".into()
    } else {
        source
    };
}

pub(crate) fn send_effect(text: String, revision: u64) -> Effect {
    let content = if let Some(arguments) = text
        .strip_prefix("/skill")
        .filter(|arguments| arguments.starts_with(char::is_whitespace))
    {
        let arguments = arguments.trim();
        let (name, request) = arguments
            .split_once(char::is_whitespace)
            .unwrap_or((arguments, ""));
        let request = request.trim();
        vec![
            crate::bus::command::ContentBlock::Text {
                text: if request.is_empty() {
                    format!("Apply the {name} skill.")
                } else {
                    request.to_string()
                },
            },
            crate::bus::command::ContentBlock::Skill {
                name: name.to_string(),
            },
        ]
    } else {
        let text = if text.starts_with("//skill") {
            text[1..].to_string()
        } else {
            text
        };
        vec![crate::bus::command::ContentBlock::Text { text }]
    };
    Effect::SendAt {
        request: SubmitRequest {
            answer: None,
            target_actor_id: "agent:main".into(),
            mode: "next-turn".into(),
            content,
        },
        revision,
    }
}

pub fn update(s: &mut State, e: ClientEvent) -> Result<Vec<Effect>, ProtocolError> {
    let shared = matches!(
        e,
        ClientEvent::Models { .. }
            | ClientEvent::Layers { .. }
            | ClientEvent::Mcp { .. }
            | ClientEvent::Sessions { .. }
            | ClientEvent::Commands { .. }
            | ClientEvent::SettingsNotSaved { .. }
            | ClientEvent::ConfigLoaded { .. }
            | ClientEvent::ConfigWritten { .. }
            | ClientEvent::GatewayTested { .. }
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
            if envelope.kind != crate::bus::event::EventKind::ContextUpdated.as_str()
                && envelope.kind != crate::bus::event::EventKind::McpStatus.as_str())
    {
        s.tabs[index].unread = true;
    }
    // The event is applied with its tab in front, then the tab on screen comes back.
    let shown = std::mem::replace(&mut s.active, index);
    let result = apply(s, e, background);
    s.active = shown;
    result
}

/// Applies `e` to the active tab; `background` tells that the Captain is looking at another one.
fn apply(s: &mut State, e: ClientEvent, background: bool) -> Result<Vec<Effect>, ProtocolError> {
    let mut effects = vec![];
    match e {
        ClientEvent::SessionDeleted { .. } => {}
        ClientEvent::ConfigLoaded {
            request, result, ..
        } => effects.extend(crate::tui::config::loaded(&mut s.config, request, result)),
        ClientEvent::ConfigWritten {
            key, value, result, ..
        } => effects.extend(crate::tui::config::written(
            &mut s.config,
            key,
            value,
            result,
        )),
        ClientEvent::GatewayTested { name, result, .. } => {
            s.config.testing_gateway = None;
            s.config.gateway_tests.insert(name, result);
        }
        ClientEvent::ChildEnvelope {
            child_session_id,
            envelope,
            ..
        } => {
            project_child(s, &child_session_id, &envelope);
        }
        ClientEvent::Envelope { envelope, .. } => {
            let selection_changed = envelope.kind == "model.changed";
            let spawned = (envelope.kind == "subagent.spawned"
                && envelope.payload["parent_session_id"].as_str()
                    == s.session
                        .as_ref()
                        .map(|m| m.session_id.as_str())
                        .or(s.resuming.as_deref()))
            .then(|| {
                envelope.payload["child_session_id"]
                    .as_str()
                    .map(str::to_string)
            })
            .flatten();
            // A turn may have changed files: the branch and the changes are read again.
            let turn_end = envelope.kind == crate::bus::event::EventKind::TurnEnd.as_str();
            if s.read_only()
                && s.last_applied_seq.is_none_or(|seq| envelope.seq > seq)
                && let Some(id) = s
                    .session
                    .as_ref()
                    .map(|m| m.session_id.clone())
                    .or(s.resuming.clone())
            {
                if super::state::ChildStatus::terminal_event(&envelope).is_some()
                    || (envelope.kind == "turn.end"
                        && matches!(envelope.payload["outcome"].as_str(), Some("done" | "error")))
                {
                    effects.push(Effect::FetchChildTerminalMetadata(id.clone()));
                }
                project_child(s, &id, &envelope);
            }
            let own_session = s
                .session
                .as_ref()
                .map(|m| m.session_id.clone())
                .or(s.resuming.clone());
            let read_only = s.read_only();
            let default_agent = s.tabs[s.active].default_agent.clone();
            let State {
                tabs,
                active,
                mcp,
                catalog,
                ..
            } = s;
            apply_envelope(
                &mut tabs[*active].projection,
                envelope,
                EnvelopeContext {
                    own_session: own_session.as_deref(),
                    read_only,
                    default_agent: default_agent.as_deref(),
                    catalog,
                    mcp,
                },
            )?;
            if selection_changed
                && let Some(id) = s
                    .session
                    .as_ref()
                    .map(|m| m.session_id.clone())
                    .or(s.resuming.clone())
            {
                effects.push(Effect::FetchAgents(Some(id)));
            }
            let terminal = s
                .children
                .iter()
                .filter(|c| c.status.terminal())
                .map(|c| (c.session_id.clone(), c.status))
                .collect::<Vec<_>>();
            for (id, status) in terminal {
                for tab in &mut s.tabs {
                    if tab
                        .session
                        .as_ref()
                        .map(|m| m.session_id.as_str())
                        .or(tab.resuming.as_deref())
                        == Some(id.as_str())
                    {
                        tab.child_status = status;
                    }
                }
            }
            effects.extend(
                s.children
                    .iter()
                    .filter(|c| c.status.terminal())
                    .map(|c| Effect::UnwatchChild(c.session_id.clone())),
            );
            if let Some(child) = spawned {
                effects.push(watch_child(s, child));
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
            let context = take_context(s);
            let children = std::mem::take(&mut s.children);
            reset_projection(s);
            s.children = children;
            // The replay says again whether the switch still waits: hidden until its acceptance
            // comes back and nothing settles it.
            if let Some(m) = s.model_switch.as_mut() {
                m.deferred = false;
            }
            restore_context(s, context);
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
            // `context.updated` and `mcp.status` are live-only: what was missed before this
            // subscription (a reconnect, or the gap since the session was created) is read back.
            s.conversation.context_epoch += 1;
            if let Some(m) = &s.session {
                effects.push(Effect::FetchContext {
                    session_id: m.session_id.clone(),
                    epoch: s.conversation.context_epoch,
                });
            }
            effects.push(Effect::FetchMcp);
        }
        ClientEvent::StreamDisconnected { .. } => s.connection = ConnectionStatus::Disconnected,
        ClientEvent::Reconnecting { .. } => s.connection = ConnectionStatus::Reconnecting,
        ClientEvent::ProtocolError { message, .. } => s.last_protocol_error = Some(message),
        ClientEvent::Skills { result, .. } => {
            s.skills = match result {
                Ok(skills) => skills,
                Err(error) => crate::bus::event::SkillsBody {
                    skills: vec![],
                    disabled_reason: Some(error),
                },
            };
        }
        ClientEvent::Models {
            workspace,
            request,
            result,
            ..
        } => {
            if workspace != s.workspace || request < s.models_request {
                return Ok(vec![]);
            }
            s.models_request = request;
            if let Ok(body) = result {
                s.catalog = body.models;
                if let Some(agent) = body.default_agent {
                    s.default_agent = agent;
                }
                // Every tab, on screen or not: a default where none is chosen, and the display
                // name and window refreshed from the new catalog.
                let State { tabs, catalog, .. } = s;
                for tab in tabs {
                    refresh_tab_model(catalog, tab);
                }
            }
        }
        ClientEvent::Agents {
            session_id,
            workspace,
            request,
            result,
            ..
        } => {
            let current = s
                .session
                .as_ref()
                .map(|m| m.session_id.as_str())
                .or(s.resuming.as_deref());
            if workspace != s.workspace
                || request < s.agents_request
                || session_id.as_deref() != current
            {
                return Ok(vec![]);
            }
            s.agents_request = request;
            if let Ok(body) = result {
                s.default_agent = body.default_agent;
                s.tabs[s.active].default_agent = Some(s.default_agent.clone());
                s.agents = body.agents;
                if !occupied(s) && s.start_model.is_none() {
                    let agent = s.agent.clone().unwrap_or_else(|| s.default_agent.clone());
                    if let Some(choice) = s
                        .agents
                        .iter()
                        .find(|a| a.id == agent)
                        .and_then(|a| a.effective.clone())
                    {
                        s.selected_model_id = Some(choice.model.clone());
                        s.selected_model_name = Some(choice.model);
                        s.variant = s.start_variant.clone().unwrap_or(choice.variant);
                        s.model_source = choice.source;
                        s.variant_source = if s.start_variant.is_some() {
                            "override".into()
                        } else {
                            choice.variant_source
                        };
                        refresh_model(s);
                    }
                }
            }
        }
        ClientEvent::Layers {
            workspace,
            request,
            result,
            ..
        } => {
            if workspace != s.workspace || request < s.layers_request {
                return Ok(vec![]);
            }
            s.layers_request = request;
            if let Ok(body) = result {
                s.layers = body.layers;
            }
        }
        // A failed read keeps what is shown: the next event or reconnection brings it back.
        ClientEvent::Mcp { result, .. } => {
            if let Ok(snapshot) = result {
                keep_newer_mcp(&mut s.mcp, snapshot);
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
        ClientEvent::QuestionAnswered {
            call_id, result, ..
        } => {
            if let Err(error) = result {
                for block in &mut s.conversation.blocks {
                    if let BlockKind::Question(p) = &mut block.kind
                        && p.call_id == call_id
                        && p.answer.is_none()
                    {
                        p.sending = false;
                        p.error = Some(error.clone());
                    }
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
                if name == "bypass" {
                    s.bypass_pending = false;
                    if response.ok
                        && let Some(data) = response.data.clone()
                        && let Ok(r) =
                            serde_json::from_value::<super::protocol::BypassResponse>(data)
                    {
                        keep_bypass(s, r.enabled, r.bypass_seq);
                    }
                }
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
                            .map(strip_controls);
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
                if name == "bypass" {
                    s.bypass_pending = false;
                }
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
        ClientEvent::BypassSet { result, .. } => {
            s.bypass_pending = false;
            match result {
                Ok(r) => keep_bypass(s, r.enabled, r.bypass_seq),
                Err(error) => push_local(s, BlockKind::Error(error)),
            }
        }
        ClientEvent::Sessions { result, .. } => {
            if let Ok(mut b) = result {
                for entry in &mut b.sessions {
                    entry.id = strip_controls(&entry.id);
                    entry.title = entry.title.as_deref().map(strip_controls);
                    entry.agent = entry.agent.as_deref().map(strip_controls);
                    entry.created_at = strip_controls(&entry.created_at);
                    entry.updated_at = strip_controls(&entry.updated_at);
                    entry.parent_session_id =
                        entry.parent_session_id.as_deref().map(strip_controls);
                    entry.child_status = entry.child_status.as_deref().map(strip_controls);
                }
                s.sessions = b.sessions;
                let State { sessions, tabs, .. } = s;
                for tab in tabs {
                    if let Some(entry) = sessions.iter().find(|entry| {
                        tab.session
                            .as_ref()
                            .is_some_and(|m| m.session_id == entry.id)
                    }) && entry.bypass_seq >= tab.bypass_seq
                    {
                        tab.bypass = entry.bypass;
                        tab.bypass_seq = entry.bypass_seq;
                    }
                }
                seed_children(s);
                effects.extend(child_watches(s));
            }
        }
        ClientEvent::Created { result, .. } | ClientEvent::Resumed { result, .. } => match result {
            Ok(r) => {
                s.resuming = None;
                s.bypass = false;
                // A fresh runtime has no switch waiting for a turn.
                s.model_switch = None;
                // A fetch issued before this (re)start may describe the old runtime.
                s.conversation.context_epoch += 1;
                s.session = Some(SessionMetadata {
                    yolo: false,
                    permissions_unavailable: None,
                    bypass: false,
                    bypass_seq: 0,
                    incarnation: None,
                    session_id: r.session_id.clone(),
                    actor_state: r.actor_state,
                    workspace: s.workspace.clone(),
                    model: s.selected_model_id.clone().unwrap_or_default(),
                    agent: s.agent.clone(),
                    variant: None,
                    parent_session_id: None,
                    promptable: true,
                    child_status: None,
                    source: String::new(),
                    variant_source: String::new(),
                    read_only: false,
                    resume_disabled_reason: None,
                    current_seq: s.last_applied_seq.unwrap_or(0),
                });
                if s.activity == Activity::Stopped {
                    s.activity = Activity::Idle;
                }
                effects.push(Effect::FetchMetadata(r.session_id.clone()));
                effects.push(Effect::FetchAgents(Some(r.session_id.clone())));
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
                clear_pending_session(s);
                s.notice = Some(x)
            }
        },
        ClientEvent::Metadata { result, .. } => {
            if let Ok(mut m) = result {
                m.session_id = strip_controls(&m.session_id);
                m.actor_state = strip_controls(&m.actor_state);
                m.workspace = strip_controls(&m.workspace);
                m.model = strip_controls(&m.model);
                m.agent = m.agent.as_deref().map(strip_controls);
                m.variant = m.variant.as_deref().map(strip_controls);
                m.parent_session_id = m.parent_session_id.as_deref().map(strip_controls);
                m.child_status = m.child_status.as_deref().map(strip_controls);
                if std::mem::take(&mut s.inspecting) {
                    if m.parent_session_id.is_some()
                        || !m.promptable
                        || m.read_only
                        || m.incarnation.is_some()
                    {
                        s.parent_session_id = m.parent_session_id.clone();
                        s.child_status = super::state::ChildStatus::from_wire(
                            m.child_status.as_deref().unwrap_or(""),
                        );
                        clear_pending_session(s);
                        effects.push(Effect::Subscribe {
                            session_id: m.session_id.clone(),
                            cursor: None,
                        });
                    } else {
                        effects.push(Effect::Resume(m.session_id.clone()));
                    }
                }
                if s.read_only() {
                    s.resuming = None;
                    let status = super::state::ChildStatus::from_wire(
                        m.child_status.as_deref().unwrap_or(""),
                    );
                    if status.terminal() {
                        s.child_status = status;
                        for tab in &mut s.tabs {
                            if let Some(child) = tab
                                .children
                                .iter_mut()
                                .find(|c| c.session_id == m.session_id)
                            {
                                child.status = status;
                            }
                        }
                    }
                }
                // Only reached for the current generation, so `m` describes this conversation.
                // A summary read before a change the stream already confirmed is older than it.
                let agent = m.agent.clone().unwrap_or_else(|| s.default_agent.clone());
                apply_selection(
                    &mut s.tabs[s.active].projection,
                    super::protocol::ModelIdentity {
                        agent,
                        selection: super::protocol::Selection {
                            model: m.model.clone(),
                            variant: m.variant.clone(),
                            source: m.source.clone(),
                            variant_source: m.variant_source.clone(),
                        },
                    },
                    m.current_seq,
                    None,
                    &s.catalog,
                );
                keep_bypass(s, m.bypass, m.bypass_seq);
                s.session = Some(m);
                seed_children(s);
                effects.extend(child_watches(s));
            } else {
                s.inspecting = false;
                clear_pending_session(s);
                if let Err(error) = result {
                    push_local(s, BlockKind::Error(error));
                }
            }
        }
        // An older server has no endpoint: the gauge stays on the provider's count.
        ClientEvent::Context { epoch, result, .. } => {
            if let Ok(snapshot) = result
                && epoch == s.conversation.context_epoch
            {
                keep_newer_context(s, snapshot);
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
            pause_token,
            ..
        } => match result {
            Ok(()) => {
                if pause_token.is_some() && s.pause_token != pause_token {
                    return Ok(vec![]);
                }
                let text = match control {
                    super::protocol::ChildControl::Stop => {
                        "stop accepted; waiting for the subagent to finish"
                    }
                    super::protocol::ChildControl::Continue => {
                        "continue accepted; waiting for the agent"
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

fn project_child(s: &mut State, id: &str, envelope: &crate::bus::event::EventEnvelope) {
    for tab in &mut s.tabs {
        if let Some(child) = tab.children.iter_mut().find(|c| c.session_id == id) {
            super::state::ChildStatus::project(&mut child.status, &mut child.child_seq, envelope);
        }
        if tab.read_only()
            && tab
                .session
                .as_ref()
                .map(|m| m.session_id.as_str())
                .or(tab.resuming.as_deref())
                == Some(id)
        {
            super::state::ChildStatus::project(
                &mut tab.child_status,
                &mut tab.child_status_seq,
                envelope,
            );
        }
    }
}

fn watch_child(tab: &super::state::Tab, child_session_id: String) -> Effect {
    Effect::WatchChild {
        parent_session_id: tab
            .session
            .as_ref()
            .map(|m| m.session_id.clone())
            .or(tab.resuming.clone())
            .unwrap_or_default(),
        tab_id: tab.id,
        generation: tab.generation.clone(),
        child_session_id,
    }
}

fn child_watches(s: &State) -> Vec<Effect> {
    s.tabs
        .iter()
        .flat_map(|tab| {
            tab.children.iter().map(|child| {
                if child.status.terminal() {
                    Effect::UnwatchChild(child.session_id.clone())
                } else {
                    watch_child(tab, child.session_id.clone())
                }
            })
        })
        .collect()
}

fn seed_children(s: &mut State) {
    for tab in &mut s.tabs {
        if let Some(entry) = s.sessions.iter().find(|e| {
            tab.session
                .as_ref()
                .map(|m| m.session_id.as_str())
                .or(tab.resuming.as_deref())
                == Some(e.id.as_str())
        }) {
            let status =
                super::state::ChildStatus::from_wire(entry.child_status.as_deref().unwrap_or(""));
            if status.terminal() {
                tab.child_status = status;
            }
        }
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
            if let Some(child) = tab
                .projection
                .children
                .iter_mut()
                .find(|c| c.session_id == entry.id)
            {
                if (child.event_seq.is_none()
                    && child.child_seq.is_none()
                    && !child.status.terminal())
                    || super::state::ChildStatus::from_wire(
                        entry.child_status.as_deref().unwrap_or(""),
                    )
                    .terminal()
                {
                    child.status = super::state::ChildStatus::from_wire(
                        entry.child_status.as_deref().unwrap_or(""),
                    );
                }
            } else {
                tab.projection.children.push(super::state::ChildAgent {
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

#[cfg(test)]
mod skill_tests {
    use super::*;
    use crate::bus::command::ContentBlock;

    #[test]
    fn selected_skill_sends_a_typed_selection_and_captain_request() {
        for (draft, expected) in [
            ("/skill review check this", "check this"),
            ("/skill review ", "Apply the review skill."),
        ] {
            let Effect::SendAt { request, revision } = send_effect(draft.into(), 42) else {
                panic!("send")
            };
            assert_eq!(revision, 42);
            assert_eq!(
                request.content,
                vec![
                    ContentBlock::Text {
                        text: expected.into()
                    },
                    ContentBlock::Skill {
                        name: "review".into()
                    }
                ]
            );
        }
        let routed =
            crate::tui::commands::route("//skill review", crate::tui::protocol::Mode::NextTurn);
        let crate::tui::commands::Routed::Message(text) = routed else {
            panic!("literal message")
        };
        let Effect::SendAt { request, .. } = send_effect(text, 0) else {
            panic!("send")
        };
        assert_eq!(
            request.content,
            vec![ContentBlock::Text {
                text: "/skill review".into()
            }]
        );
    }
}
