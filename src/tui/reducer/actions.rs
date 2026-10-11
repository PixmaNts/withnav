//! UI action → effects.
use super::{push_local, refresh_model, send_effect, start_conversation};
use crate::tui::{
    commands,
    protocol::{Action, CommandRequest, CreateSessionRequest, Effect, Generation, Overlay},
    state::{
        Activity, BlockKind, State, Tab, TabView, UiState, VariantPick, DEFAULT_AGENT, MAX_TABS,
        NO_VARIANT, PLAN_AGENT,
    },
    theme::ThemeId,
};

fn submit(s: &mut State, text: String, revision: u64) -> Vec<Effect> {
    if s.read_only() {
        push_local(
            s,
            BlockKind::Notice(
                "this subagent is read-only; return to its parent to send a message".into(),
            ),
        );
        return vec![];
    }
    if s.pending_submit_text.is_some() {
        // One message waits for the conversation to be ready; a second one is not queued.
        push_local(
            s,
            BlockKind::Notice(
                "the conversation is still starting; send again once it is ready".into(),
            ),
        );
        return vec![];
    }
    if s.resuming.is_some() {
        // Sent once the resume answers, to the resumed conversation.
        s.pending_submit_text = Some(text);
        s.pending_submit = Some(revision);
        return vec![];
    }
    match s.session.as_ref().map(|m| m.session_id.clone()) {
        Some(id) if s.activity == Activity::Stopped => {
            s.pending_submit_text = Some(text);
            s.pending_submit = Some(revision);
            s.resuming = Some(id.clone());
            vec![Effect::Resume(id)]
        }
        Some(_) => vec![send_effect(text, revision)],
        None => {
            s.pending_submit_text = Some(text);
            s.pending_submit = Some(revision);
            vec![Effect::Create(CreateSessionRequest {
                workspace: s.workspace.clone(),
                model: s.start_model.clone(),
                resume: None,
                agent: s.agent.clone(),
                variant: s.start_variant.clone(),
            })]
        }
    }
}

fn clear_start_selection(s: &mut State) {
    s.start_model = None;
    s.start_variant = None;
    s.selected_model_id = None;
    s.selected_model_name = None;
    s.variant = None;
}

/// Keeps the transcript position of the active tab before another one comes forward.
fn save_view(s: &mut State, ui: &UiState) {
    s.view = TabView {
        scroll: ui.scroll,
        follow_bottom: ui.follow_bottom,
        selected_block: ui.selected_block.clone(),
        expanded: ui.expanded.clone(),
    };
}

/// Brings the tab at `index` forward: its transcript position, and its draft in the editor.
fn show(s: &mut State, ui: &mut UiState, index: usize) -> Vec<Effect> {
    ui.child_confirm = None;
    save_view(s, ui);
    // A deny message belongs to the request on screen.
    ui.deny_message = None;
    s.active = index;
    vec![restore_active_tab(s, ui)]
}

fn restore_active_tab(s: &mut State, ui: &mut UiState) -> Effect {
    ui.focus = if s.read_only() {
        crate::tui::state::Focus::Transcript
    } else {
        crate::tui::state::Focus::Editor
    };
    s.unread = false;
    let view = s.view.clone();
    ui.scroll = view.scroll;
    ui.follow_bottom = view.follow_bottom;
    ui.selected_block = view.selected_block;
    ui.expanded = view.expanded;
    Effect::SetEditor {
        text: s.draft.clone(),
        revision: s.draft_revision,
    }
}

/// Opens a blank tab after the others and brings it forward, keeping the model of the tab it was
/// opened from. With `MAX_TABS` open, nothing opens and the active tab says why.
fn open_tab(s: &mut State, ui: &mut UiState) -> Option<Vec<Effect>> {
    if s.tabs.len() >= MAX_TABS {
        push_local(
            s,
            BlockKind::Notice(format!(
                "{MAX_TABS} conversations are open, the most at once; close one with alt+w first"
            )),
        );
        return None;
    }
    let id = s.issue();
    let generation = Generation(s.issue());
    let tab = Tab {
        id,
        generation,
        agent: Some(s.default_agent.clone()),
        view: TabView {
            follow_bottom: true,
            ..TabView::default()
        },
        ..Tab::default()
    };
    s.tabs.push(tab);
    Some(show(s, ui, s.tabs.len() - 1))
}

/// The active tab when it holds nothing, else a new tab; `None` when the limit refused it.
fn blank_tab(s: &mut State, ui: &mut UiState) -> Option<Vec<Effect>> {
    if s.is_blank() {
        Some(vec![])
    } else {
        open_tab(s, ui)
    }
}

/// Closes the active tab. The conversation is not stopped: the server keeps it, and a running turn
/// goes on there. The last tab is replaced by a blank one.
fn close_tab(s: &mut State, ui: &mut UiState) -> Vec<Effect> {
    ui.deny_message = None;
    // A message sent while the conversation is still being created or resumed is held in the tab
    // until the server answers; closing now would drop it with no way to get it back.
    if s.pending_submit_text.is_some() {
        push_local(
            s,
            BlockKind::Notice(
                "this tab is waiting for the server to open its conversation before sending your \
                 message; close it once the message is sent"
                    .to_string(),
            ),
        );
        return vec![];
    }
    let closed = s.tabs.remove(s.active);
    let notice = closed.session.as_ref().map(|m| {
        let name = s
            .sessions
            .iter()
            .find(|e| e.id == m.session_id)
            .and_then(|e| e.title.clone())
            .unwrap_or_else(|| short_id(&m.session_id));
        if closed.activity == Activity::Running {
            format!("closed {name}; its turn keeps running on the server, reopen it from /sessions")
        } else {
            format!("closed {name}; the conversation is kept, reopen it from /sessions")
        }
    });
    let effects = if s.tabs.is_empty() {
        // `show` saves the view of the active tab: there is none, so a blank one stands in first.
        s.tabs.push(Tab::default());
        s.active = 0;
        let fresh = s.issue();
        s.id = fresh;
        start_conversation(s);
        s.agent = Some(s.default_agent.clone());
        s.view.follow_bottom = true;
        let view = s.view.clone();
        ui.scroll = view.scroll;
        ui.follow_bottom = view.follow_bottom;
        ui.selected_block = None;
        ui.expanded.clear();
        vec![Effect::SetEditor {
            text: String::new(),
            revision: 0,
        }]
    } else {
        let next = closed
            .parent_session_id
            .as_deref()
            .and_then(|id| s.tab_of_session(id))
            .unwrap_or_else(|| s.active.min(s.tabs.len() - 1));
        s.active = next;
        vec![restore_active_tab(s, ui)]
    };
    if let Some(notice) = notice {
        push_local(s, BlockKind::Notice(notice));
    }
    effects
}

/// The first eight characters of a session id, enough to tell conversations apart.
pub fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

fn resume(s: &mut State, ui: &mut UiState, id: String) -> Vec<Effect> {
    if s.sessions
        .iter()
        .any(|e| e.id == id && e.parent_session_id.is_some())
        || s.tabs
            .iter()
            .any(|t| t.children.iter().any(|c| c.session_id == id))
    {
        return open_child(s, ui, id);
    }
    if let Some(index) = s.tab_of_session(&id) {
        let mut effects = if index == s.active {
            vec![]
        } else {
            show(s, ui, index)
        };
        // A stopped session keeps its projection and cursor (the subscribe after `Resumed` starts
        // from `last_applied_seq`); a live one, or one being resumed, needs nothing.
        if s.activity == Activity::Stopped && s.resuming.is_none() && !s.read_only() {
            s.resuming = Some(id.clone());
            effects.push(Effect::Resume(id));
        }
        return effects;
    }
    let Some(mut effects) = blank_tab(s, ui) else {
        return vec![];
    };
    start_conversation(s);
    s.resuming = Some(id.clone());
    s.inspecting = true;
    effects.push(Effect::FetchMetadata(id));
    effects
}

fn open_child(s: &mut State, ui: &mut UiState, id: String) -> Vec<Effect> {
    if let Some(index) = s.tab_of_session(&id) {
        return show(s, ui, index);
    }
    let link = s
        .tabs
        .iter()
        .find_map(|t| {
            t.children
                .iter()
                .find(|c| c.session_id == id)
                .and_then(|c| {
                    t.session
                        .as_ref()
                        .map(|m| (m.session_id.clone(), c.name.clone()))
                })
        })
        .or_else(|| {
            s.sessions.iter().find(|e| e.id == id).and_then(|e| {
                e.parent_session_id
                    .clone()
                    .map(|parent| (parent, e.agent.clone().unwrap_or_default()))
            })
        });
    let Some((parent, agent)) = link else {
        return vec![];
    };
    let status = s
        .tabs
        .iter()
        .flat_map(|t| &t.children)
        .find(|c| c.session_id == id)
        .map(|c| c.status)
        .or_else(|| {
            s.sessions.iter().find(|e| e.id == id).map(|e| {
                crate::tui::state::ChildStatus::from_wire(e.child_status.as_deref().unwrap_or(""))
            })
        })
        .unwrap_or_default();
    let Some(mut effects) = blank_tab(s, ui) else {
        return vec![];
    };
    start_conversation(s);
    s.parent_session_id = Some(parent);
    s.child_status = status;
    s.agent = Some(agent);
    s.resuming = Some(id.clone());
    s.selected_model_id = None;
    s.selected_model_name = None;
    s.context_window = None;
    ui.focus = crate::tui::state::Focus::Transcript;
    effects.push(Effect::FetchMetadata(id.clone()));
    effects.push(Effect::Subscribe {
        session_id: id,
        cursor: None,
    });
    effects
}

/// Points the active tab at `model_id` (a new tab when the active one holds a conversation: a
/// conversation keeps its model). `variant` is the start variant chosen in the picker; `None`
/// leaves the model_id's default. `None` when the tab limit refused a new tab.
fn pick_model(
    s: &mut State,
    ui: &mut UiState,
    model_id: String,
    variant: Option<Option<String>>,
) -> Option<Vec<Effect>> {
    if reject_non_chat_model(s, &model_id) {
        return None;
    }
    // A tab with a conversation, or one being created or resumed, keeps its model.
    let occupied = s.session.is_some() || s.resuming.is_some() || s.pending_submit_text.is_some();
    let effects = if occupied { open_tab(s, ui)? } else { vec![] };
    s.selected_model_id = Some(model_id);
    s.start_model = s.selected_model_id.clone();
    s.model_source = "override".into();
    s.variant_source = if variant.is_some() {
        "override"
    } else {
        "model_default"
    }
    .into();
    refresh_model(s);
    let default = s
        .catalog
        .iter()
        .find(|m| Some(&m.name) == s.selected_model_id.as_ref())
        .and_then(|m| m.default_variant.clone());
    s.variant = variant.clone().unwrap_or(default);
    s.start_variant = variant;
    let catalog = s.catalog.clone();
    super::refresh_start_variant(s, &catalog);
    Some(effects)
}

/// Why the active conversation cannot switch model now: the server refuses while the agent
/// waits for an answer or a decision. `None` when it can (a running turn only defers it).
pub fn switch_blocked(tab: &Tab) -> Option<&'static str> {
    if tab.read_only() {
        Some("this subagent is read-only; its parent chooses its model")
    } else if tab.open_permission().is_some() {
        Some("a tool call is waiting for approval: answer it first, then switch model")
    } else if tab.activity == Activity::AwaitingUser && !tab.watchdog_paused {
        Some("the agent is waiting for your answer: reply first, then switch model")
    } else {
        None
    }
}

/// Whether the active tab holds a conversation (live, starting or being resumed): the model
/// picker then switches it rather than choosing the model a new one starts on.
pub fn occupied(tab: &Tab) -> bool {
    tab.session.is_some() || tab.resuming.is_some() || tab.pending_submit_text.is_some()
}

fn reject_non_chat_model(s: &mut State, model_id: &str) -> bool {
    if s.catalog
        .iter()
        .any(|model| model.name == model_id && model.api == crate::tui::protocol::ApiKind::Chat)
    {
        return false;
    }
    push_local(
        s,
        BlockKind::Notice(format!(
            "cannot select {model_id}: conversations require a model with the chat API"
        )),
    );
    true
}

/// Moves the active conversation to `model_id` through `/model`: the model only, the server applies
/// the model_id's default variant (`/variant` changes it after). A tab with no conversation yet
/// starts on it instead.
fn switch_model(s: &mut State, ui: &mut UiState, model_id: String) -> Vec<Effect> {
    if reject_non_chat_model(s, &model_id) {
        return vec![];
    }
    if !occupied(s) {
        return pick_model(s, ui, model_id, None).unwrap_or_default();
    }
    if let Some(why) = switch_blocked(s) {
        push_local(
            s,
            BlockKind::Command {
                name: "model".into(),
                ok: false,
                text: why.into(),
            },
        );
        return vec![];
    }
    // The model in use: nothing to switch (a switch waiting for the turn is left as it is).
    if s.selected_model_id.as_deref() == Some(model_id.as_str()) && s.pending_model().is_none() {
        push_local(
            s,
            BlockKind::Notice(format!(
                "already on {model_id}; use /variant to change the variant"
            )),
        );
        return vec![];
    }
    let effects = run_command(s, "model".into(), model_id.clone(), None);
    // A new switch replaces the one shown, whatever became of it.
    if !effects.is_empty() {
        s.model_switch = Some(crate::tui::state::ModelSwitch {
            agent: s.agent_name().into(),
            model_id,
            token: s.command_token,
            operation_id: None,
            deferred: false,
        });
    }
    effects
}

/// Opens the `/variant` list on `current` (the variant in use, `None` for none), unless another
/// overlay took the screen.
pub fn show_variants(s: &State, ui: &mut UiState, current: Option<&str>) {
    if ui.overlay.is_some() {
        return;
    }
    let pick = VariantPick::Current;
    let (rows, _) = commands::variant_choices(s, &pick);
    let wanted = current.unwrap_or(NO_VARIANT);
    let start = rows
        .iter()
        .position(|r| r == wanted)
        .unwrap_or(rows.len().saturating_sub(1));
    ui.overlay = Some(Overlay::Variants);
    ui.filter.clear();
    ui.selection
        .insert(format!("{:?}", Overlay::Variants), start);
    ui.variant_pick = Some(pick);
}

/// Build to plan and back. A conversation switches through `/agent` (the server may refuse, and
/// says why in its `command.result`); one not created yet just starts as the other agent.
fn toggle_agent(s: &mut State) -> Vec<Effect> {
    let next = if s.agent_name() == PLAN_AGENT {
        DEFAULT_AGENT
    } else {
        PLAN_AGENT
    };
    if s.session.is_none() && s.resuming.is_none() && s.pending_submit_text.is_none() {
        s.agent = Some(next.to_string());
        s.start_model = None;
        s.start_variant = None;
        s.selected_model_id = None;
        s.variant = None;
        return vec![Effect::FetchAgents(None)];
    }
    run_command(s, "agent".into(), next.into(), None)
}

/// Sends a server command to the active conversation. With no conversation yet (or one being
/// started or resumed) there is nothing to run it in, and the Captain is told so.
fn run_command(
    s: &mut State,
    name: String,
    arguments: String,
    revision: Option<u64>,
) -> Vec<Effect> {
    if name == "bypass" && !arguments.trim().is_empty() && (s.bypass_pending || s.archive()) {
        return vec![];
    }
    if s.read_only() {
        push_local(
            s,
            BlockKind::Notice("this subagent is read-only; run commands in its parent".into()),
        );
        return vec![];
    }
    let live = s
        .session
        .as_ref()
        .filter(|_| s.resuming.is_none() && s.activity != Activity::Stopped)
        .map(|m| m.session_id.clone());
    let Some(session_id) = live else {
        let why = if s.activity == Activity::Stopped {
            "the conversation is stopped; send a message to resume it, then run the command"
        } else {
            "no conversation yet; send a message first, then run the command"
        };
        push_local(
            s,
            BlockKind::Command {
                name,
                ok: false,
                text: why.into(),
            },
        );
        return vec![];
    };
    s.command_token += 1;
    if name == "bypass" && !arguments.trim().is_empty() {
        if s.bypass_pending {
            return vec![];
        }
        s.bypass_pending = true;
    }
    vec![Effect::RunCommand {
        session_id,
        request: CommandRequest { name, arguments },
        revision,
        token: s.command_token,
    }]
}

pub fn handle_action(s: &mut State, ui: &mut UiState, a: Action) -> Vec<Effect> {
    match a {
        Action::OpenConfig => {
            ui.screen = crate::tui::config::Screen::Config;
            ui.overlay = None;
            vec![
                s.config.fetch(),
                Effect::FetchModels,
                Effect::FetchAgents(None),
            ]
        }
        Action::Config(action) => crate::tui::config::handle(s, ui, action),
        Action::Submit(text) => submit(s, text, 0),
        Action::SubmitAt { text, revision } => submit(s, text, revision),
        Action::NewConversation => {
            if s.tabs.len() >= MAX_TABS && !s.is_blank() {
                return blank_tab(s, ui).unwrap_or_default();
            }
            let mut effects = blank_tab(s, ui).unwrap_or_default();
            s.agent = None;
            clear_start_selection(s);
            s.model_source.clear();
            s.variant_source.clear();
            ui.overlay = Some(Overlay::Agents);
            ui.filter.clear();
            let selected = s
                .agents
                .iter()
                .filter(|a| matches!(a.mode.as_str(), "primary" | "all"))
                .position(|a| a.id == s.default_agent)
                .unwrap_or(0);
            ui.selection.insert("Agents".into(), selected);
            effects.push(Effect::FetchAgents(None));
            effects
        }
        Action::PickAgent(agent) => {
            if !s
                .agents
                .iter()
                .any(|a| a.id == agent && matches!(a.mode.as_str(), "primary" | "all"))
            {
                push_local(s, BlockKind::Notice(format!("agent {agent} is not available for a root conversation; choose a primary/all agent")));
                return vec![];
            }
            let Some(mut effects) = blank_tab(s, ui) else {
                return vec![];
            };
            s.agent = Some(agent);
            clear_start_selection(s);
            effects.push(Effect::FetchAgents(None));
            effects
        }
        Action::ResumeSession(id) => resume(s, ui, id),
        Action::RequestDeleteSession(id) => {
            if ui.overlay == Some(Overlay::Sessions)
                && ui.delete_pending.is_none()
                && s.sessions.iter().any(|e| e.id == id)
            {
                ui.delete_confirm = Some(id);
            }
            vec![]
        }
        Action::CancelDeleteSession => {
            ui.delete_confirm = None;
            vec![]
        }
        Action::ConfirmDeleteSession => {
            let Some(id) = ui.delete_confirm.take() else {
                return vec![];
            };
            ui.delete_pending = Some(id.clone());
            vec![Effect::DeleteSession(id)]
        }
        Action::SessionDeleted { session_id, result } => {
            ui.delete_pending = None;
            if let Err(error) = result {
                push_local(s, BlockKind::Error(error));
                return vec![];
            }
            let mut removed = vec![session_id.clone()];
            removed.extend(
                s.sessions
                    .iter()
                    .filter(|e| e.parent_session_id.as_deref() == Some(&session_id))
                    .map(|e| e.id.clone()),
            );
            let mut effects = vec![];
            let shown = s.tabs[s.active].id;
            for index in (0..s.tabs.len()).rev() {
                if s.tabs[index]
                    .session
                    .as_ref()
                    .is_some_and(|m| removed.contains(&m.session_id))
                    || s.tabs[index].parent_session_id.as_deref() == Some(&session_id)
                {
                    s.active = index;
                    s.pending_submit_text = None;
                    // A deleted conversation is not merely parked on the server.
                    s.session = None;
                    effects.extend(close_tab(s, ui));
                }
            }
            if let Some(index) = s.tabs.iter().position(|t| t.id == shown) {
                s.active = index;
            }
            for tab in &mut s.tabs {
                tab.children
                    .retain(|child| !removed.contains(&child.session_id));
            }
            effects.extend(removed.iter().cloned().map(Effect::UnwatchChild));
            s.sessions.retain(|e| !removed.contains(&e.id));
            let count = super::super::input::overlay_names(Overlay::Sessions, s, ui).len();
            let selected = ui.selection.entry("Sessions".into()).or_default();
            *selected = (*selected).min(count.saturating_sub(1));
            if result == Ok(true) {
                push_local(
                    s,
                    BlockKind::Notice("Deleted session and its subagent sessions.".into()),
                );
            }
            ui.overlay = Some(Overlay::Sessions);
            effects.push(Effect::SetEditor {
                text: s.draft.clone(),
                revision: s.draft_revision,
            });
            effects.push(Effect::ListSessions);
            effects
        }
        Action::OpenChild(id) => open_child(s, ui, id),
        Action::ContinueChild => confirm_child(s, ui, crate::tui::protocol::ChildControl::Continue),
        Action::ConfirmChildControl => {
            if s.archive() {
                ui.child_confirm = None;
                ui.child_confirm_token = None;
                return vec![];
            }
            let Some((parent_session_id, child_session_id, control)) = ui.child_confirm.take()
            else {
                return vec![];
            };
            let token = ui.child_confirm_token.take();
            let root_self = s.parent_session_id.is_none()
                && parent_session_id == child_session_id
                && s.watchdog_paused
                && s.pause_token == token;
            if (!root_self && s.parent_session_id.as_ref() != Some(&parent_session_id))
                || s.session.as_ref().map(|m| &m.session_id) != Some(&child_session_id)
            {
                return vec![];
            }
            vec![Effect::ControlChild {
                parent_session_id,
                child_session_id,
                control,
                pause_token: token,
            }]
        }
        Action::CancelChildControl => {
            ui.child_confirm = None;
            ui.child_confirm_token = None;
            vec![]
        }
        Action::PickModel(model_id) if ui.settings_target.is_some() => {
            crate::tui::config::handle(s, ui, crate::tui::config::ConfigAction::PickModel(model_id))
        }
        Action::PickModel(model_id) => pick_model(s, ui, model_id, None).unwrap_or_default(),
        Action::SwitchModel(model_id) => switch_model(s, ui, model_id),
        Action::PickVariant(name) => {
            let pick = ui.variant_pick.take();
            ui.overlay = None;
            let chosen = match name.as_str() {
                "inherit" => None,
                NO_VARIANT => Some(None),
                _ => Some(Some(name.clone())),
            };
            match pick {
                Some(VariantPick::NewConversation(model_id)) => {
                    pick_model(s, ui, model_id, chosen).unwrap_or_default()
                }
                Some(VariantPick::Current) if !occupied(s) => {
                    s.start_variant = chosen;
                    s.variant = s.start_variant.clone().flatten();
                    s.variant_source = if s.start_variant.is_some() {
                        "override"
                    } else {
                        "model_default"
                    }
                    .into();
                    let catalog = s.catalog.clone();
                    super::refresh_start_variant(s, &catalog);
                    vec![]
                }
                Some(VariantPick::Current) => run_command(s, "variant".into(), name, None),
                None => vec![],
            }
        }
        Action::RunCommand {
            name,
            arguments,
            revision,
        } => run_command(s, name, arguments, revision),
        Action::ToggleAgent => toggle_agent(s),
        Action::ToggleBypass => {
            if s.bypass_pending
                || s.read_only()
                || s.archive()
                || s.activity == Activity::Stopped
                || s.resuming.is_some()
            {
                return vec![];
            }
            let Some(session_id) = s.session.as_ref().map(|m| m.session_id.clone()) else {
                push_local(
                    s,
                    BlockKind::Notice(
                        "no conversation yet; send a message first, then toggle bypass".into(),
                    ),
                );
                return vec![];
            };
            s.bypass_pending = true;
            vec![Effect::SetBypass {
                session_id,
                enabled: !s.bypass,
            }]
        }
        Action::AnswerPermission { request_id, answer } => {
            let open = s.open_permission().map(|p| p.request_id.clone());
            let session = if s.read_only() {
                None
            } else {
                s.session.as_ref().map(|m| m.session_id.clone())
            };
            match (open, session) {
                (Some(open), Some(session_id))
                    if open == request_id
                        && s.answered_permission.as_deref() != Some(request_id.as_str()) =>
                {
                    s.answered_permission = Some(request_id.clone());
                    vec![Effect::AnswerPermission {
                        session_id,
                        request_id,
                        answer,
                    }]
                }
                _ => vec![],
            }
        }
        Action::QuestionMove(_)
        | Action::QuestionToggle
        | Action::QuestionDismiss
        | Action::QuestionCustom
        | Action::AnswerQuestion(_) => {
            if s.read_only() {
                return vec![];
            }
            let session = s.session.as_ref().map(|m| m.session_id.clone());
            let prompt = s
                .conversation
                .blocks
                .iter_mut()
                .rev()
                .find_map(|b| match &mut b.kind {
                    BlockKind::Question(p) if p.answer.is_none() => Some(p),
                    _ => None,
                });
            let Some(p) = prompt else {
                return vec![];
            };
            if p.sending {
                return vec![];
            }
            match a {
                Action::QuestionMove(delta) => {
                    let count = p.choices.options.len() + usize::from(p.choices.allow_custom);
                    p.cursor = (p.cursor as i32 + delta).rem_euclid(count as i32) as usize;
                }
                Action::QuestionToggle => {
                    if p.cursor < p.choices.options.len() && p.choices.multiple {
                        if p.selected.contains(&p.cursor) {
                            p.selected.retain(|i| *i != p.cursor);
                        } else {
                            p.selected.push(p.cursor);
                            p.selected.sort_unstable();
                        }
                    }
                }
                Action::QuestionDismiss => {
                    p.dismissed = true;
                    p.custom = false;
                }
                Action::QuestionCustom => {
                    p.dismissed = true;
                    p.custom = true;
                    return vec![Effect::SetEditor {
                        text: String::new(),
                        revision: 0,
                    }];
                }
                Action::AnswerQuestion(answer) => {
                    if p.call_id != answer.question_call_id
                        || p.choices.answer_text(&p.text, &answer).is_err()
                    {
                        return vec![];
                    }
                    if let Some(session_id) = session {
                        p.sending = true;
                        p.error = None;
                        return vec![Effect::AnswerQuestion { session_id, answer }];
                    }
                }
                _ => {}
            }
            vec![]
        }
        Action::ShowNotice(text) => {
            push_local(s, BlockKind::Notice(text));
            vec![]
        }
        Action::SwitchTab(index) => {
            if index < s.tabs.len() && index != s.active {
                show(s, ui, index)
            } else {
                vec![]
            }
        }
        Action::CloseTab => close_tab(s, ui),
        Action::ToggleSidebar => {
            ui.sidebar_hidden = !ui.sidebar_hidden;
            vec![]
        }
        Action::TogglePanel => {
            // Flips what the Captain saw, whether the width or an earlier toggle decided it.
            ui.panel_shown = Some(!ui.panel_drawn);
            vec![]
        }
        Action::Stop if s.read_only() => {
            confirm_child(s, ui, crate::tui::protocol::ChildControl::Stop)
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
                Overlay::Agents => vec![Effect::FetchAgents(
                    s.session.as_ref().map(|m| m.session_id.clone()),
                )],
                Overlay::Skills => {
                    ui.filter.clear();
                    ui.selection.insert("Skills".into(), 0);
                    s.skills = crate::bus::event::SkillsBody {
                        skills: vec![],
                        disabled_reason: Some(if s.read_only() {
                            "this subagent is read-only; choose a skill in its parent".into()
                        } else if s.session.is_none() {
                            "send a message to start a conversation before choosing a skill".into()
                        } else {
                            "loading skills…".into()
                        }),
                    };
                    if s.read_only() {
                        vec![]
                    } else {
                        s.session
                            .as_ref()
                            .map(|session| vec![Effect::FetchSkills(session.session_id.clone())])
                            .unwrap_or_default()
                    }
                }
                Overlay::Models => {
                    ui.variant_pick = None;
                    vec![Effect::FetchModels]
                }
                Overlay::Variants => {
                    if !occupied(s) {
                        ui.variant_pick = Some(VariantPick::Current);
                        s.variant_offer = s
                            .catalog
                            .iter()
                            .find(|m| Some(&m.name) == s.selected_model_id.as_ref())
                            .map(|m| m.variants.clone());
                        return vec![];
                    }
                    // `/variant` asks the server what this conversation offers; its answer opens
                    // the list. With no live conversation `run_command` says why.
                    ui.overlay = None;
                    let effects = run_command(s, "variant".into(), String::new(), None);
                    s.variant_list_wanted = !effects.is_empty();
                    effects
                }
                Overlay::Sessions => vec![Effect::ListSessions],
                Overlay::Themes => {
                    ui.theme_on_open = Some(ui.theme);
                    let current = ThemeId::ALL.iter().position(|t| *t == ui.theme);
                    ui.selection
                        .insert(format!("{o:?}"), current.unwrap_or_default());
                    vec![]
                }
                _ => vec![],
            }
        }
        Action::CloseOverlay => {
            ui.overlay = None;
            ui.variant_pick = None;
            // Leaving the theme picker without choosing puts back the theme it opened with.
            if let Some(theme) = ui.theme_on_open.take() {
                ui.theme = theme;
            }
            vec![]
        }
        Action::PreviewTheme(theme) => {
            ui.theme = theme;
            vec![]
        }
        Action::PickTheme(theme) => {
            ui.overlay = None;
            ui.theme = theme;
            ui.theme_on_open = None;
            // Saved on every confirmation, even of the theme in effect, so a failed save can be
            // retried by picking the same theme again.
            vec![Effect::SaveTheme(theme)]
        }
        Action::SaveFavoriteModels(names) => vec![Effect::SaveFavoriteModels(names)],
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

fn confirm_child(
    s: &State,
    ui: &mut UiState,
    control: crate::tui::protocol::ChildControl,
) -> Vec<Effect> {
    if s.archive() {
        return vec![];
    }
    if control == crate::tui::protocol::ChildControl::Continue
        && s.parent_session_id.is_none()
        && s.watchdog_paused
        && s.open_permission().is_none()
    {
        if let Some(session) = &s.session {
            ui.child_confirm = Some((
                session.session_id.clone(),
                session.session_id.clone(),
                control,
            ));
            ui.child_confirm_token = s.pause_token;
        }
        return vec![];
    }
    let status = s.active_child_status();
    let allowed = match control {
        crate::tui::protocol::ChildControl::Stop => matches!(
            status,
            crate::tui::state::ChildStatus::Running
                | crate::tui::state::ChildStatus::Permission
                | crate::tui::state::ChildStatus::Doubt
                | crate::tui::state::ChildStatus::OutOfSteps
        ),
        crate::tui::protocol::ChildControl::Continue => {
            matches!(
                status,
                crate::tui::state::ChildStatus::OutOfSteps | crate::tui::state::ChildStatus::Doubt
            )
        }
    };
    if allowed {
        if let (Some(parent), Some(child)) = (&s.parent_session_id, &s.session) {
            ui.child_confirm = Some((parent.clone(), child.session_id.clone(), control));
        }
    }
    vec![]
}
