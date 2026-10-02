//! UI action → effects.
use super::{refresh_model, send_effect, start_conversation};
use crate::tui::{
    protocol::{Action, CreateSessionRequest, Effect, Overlay},
    state::{Activity, State, UiState},
    theme::ThemeId,
};

fn submit(s: &mut State, text: String, revision: u64) -> Vec<Effect> {
    match &s.session {
        Some(session) if s.activity == Activity::Stopped => {
            s.pending_submit_text = Some(text);
            s.pending_submit = Some(revision);
            vec![Effect::Resume(session.session_id.clone())]
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

pub fn handle_action(s: &mut State, ui: &mut UiState, a: Action) -> Vec<Effect> {
    match a {
        Action::Submit(text) => submit(s, text, 0),
        Action::SubmitAt { text, revision } => submit(s, text, revision),
        Action::NewConversation => {
            start_conversation(s);
            vec![Effect::Unsubscribe]
        }
        Action::ResumeSession(id) => {
            let current = s.session.as_ref().map(|m| m.session_id.as_str());
            if current == Some(id.as_str()) {
                // Same id: a stopped session keeps its projection and cursor (the subscribe after
                // `Resumed` starts from `last_applied_seq`); a live one needs nothing.
                if s.activity == Activity::Stopped {
                    vec![Effect::Resume(id)]
                } else {
                    vec![]
                }
            } else {
                start_conversation(s);
                vec![Effect::Unsubscribe, Effect::Resume(id)]
            }
        }
        Action::PickModel(alias) => {
            s.selected_model_alias = Some(alias);
            refresh_model(s);
            if s.session.is_some() {
                start_conversation(s);
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
            match ui.theme_on_open.take() {
                Some(previous) if previous == theme => vec![],
                _ => vec![Effect::SaveTheme(theme)],
            }
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
