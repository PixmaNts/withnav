//! UI action → effects.
use super::{push_local, refresh_model, send_effect, start_conversation};
use crate::tui::{
    protocol::{Action, CommandRequest, CreateSessionRequest, Effect, Generation, Overlay},
    state::{Activity, BlockKind, State, Tab, TabView, UiState, MAX_TABS},
    theme::ThemeId,
};

fn submit(s: &mut State, text: String, revision: u64) -> Vec<Effect> {
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
                model: s.selected_model_alias.clone(),
                resume: None,
            })]
        }
    }
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
    save_view(s, ui);
    s.active = index;
    s.unread = false;
    let view = s.view.clone();
    ui.scroll = view.scroll;
    ui.follow_bottom = view.follow_bottom;
    ui.selected_block = view.selected_block;
    ui.expanded = view.expanded;
    vec![Effect::SetEditor {
        text: s.draft.clone(),
        revision: s.draft_revision,
    }]
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
        selected_model_alias: s.selected_model_alias.clone(),
        selected_model_name: s.selected_model_name.clone(),
        context_window: s.context_window,
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
        s.selected_model_alias = closed.selected_model_alias.clone();
        refresh_model(s);
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
        let next = s.active.min(s.tabs.len() - 1);
        s.active = next;
        s.unread = false;
        let view = s.view.clone();
        ui.scroll = view.scroll;
        ui.follow_bottom = view.follow_bottom;
        ui.selected_block = view.selected_block;
        ui.expanded = view.expanded;
        vec![Effect::SetEditor {
            text: s.draft.clone(),
            revision: s.draft_revision,
        }]
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
    if let Some(index) = s.tab_of_session(&id) {
        let mut effects = if index == s.active {
            vec![]
        } else {
            show(s, ui, index)
        };
        // A stopped session keeps its projection and cursor (the subscribe after `Resumed` starts
        // from `last_applied_seq`); a live one, or one being resumed, needs nothing.
        if s.activity == Activity::Stopped && s.resuming.is_none() {
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
    effects.push(Effect::Resume(id));
    effects
}

/// Sends a server command to the active conversation. With no conversation yet (or one being
/// started or resumed) there is nothing to run it in, and the driver is told so.
fn run_command(
    s: &mut State,
    name: String,
    arguments: String,
    revision: Option<u64>,
) -> Vec<Effect> {
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
    vec![Effect::RunCommand {
        session_id,
        request: CommandRequest { name, arguments },
        revision,
    }]
}

pub fn handle_action(s: &mut State, ui: &mut UiState, a: Action) -> Vec<Effect> {
    match a {
        Action::Submit(text) => submit(s, text, 0),
        Action::SubmitAt { text, revision } => submit(s, text, revision),
        Action::NewConversation => blank_tab(s, ui).unwrap_or_default(),
        Action::ResumeSession(id) => resume(s, ui, id),
        Action::PickModel(alias) => {
            if s.session.is_some() {
                // A conversation keeps its model: the picked one starts a new conversation.
                let Some(effects) = open_tab(s, ui) else {
                    return vec![];
                };
                s.selected_model_alias = Some(alias);
                refresh_model(s);
                effects
            } else {
                s.selected_model_alias = Some(alias);
                refresh_model(s);
                vec![]
            }
        }
        Action::RunCommand {
            name,
            arguments,
            revision,
        } => run_command(s, name, arguments, revision),
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
