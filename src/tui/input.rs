//! The message editor and key routing: turns terminal keys and pastes into
//! editor changes or `Action`s. No networking, and no change to `State`.
use super::{
    commands,
    protocol::{Action, Overlay},
    state::{Activity, Focus, State, UiState},
};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui_textarea::TextArea;

/// Multiline message editor. `revision` grows on every edit, so the reducer
/// can clear only the text it actually sent, never what was typed after it.
#[derive(Debug, Default)]
pub struct Editor {
    area: TextArea<'static>,
    revision: u64,
}

impl Editor {
    pub fn new() -> Self {
        Self::default()
    }

    /// The whole text, lines joined with `\n`.
    pub fn text(&self) -> String {
        self.area.lines().join("\n")
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Replaces the text and counts as an edit.
    pub fn set_text(&mut self, text: &str) {
        let lines = text.split('\n').map(str::to_string).collect();
        self.area = TextArea::new(lines);
        self.revision += 1;
    }

    /// Clears the editor only if nothing was edited since `revision`; returns
    /// whether it cleared.
    pub fn clear_if_revision(&mut self, revision: u64) -> bool {
        if self.revision != revision {
            return false;
        }
        self.area = TextArea::default();
        self.revision += 1;
        true
    }

    /// The underlying widget, for drawing and for key handling in this module.
    pub fn area(&self) -> &TextArea<'static> {
        &self.area
    }

    pub fn area_mut(&mut self) -> &mut TextArea<'static> {
        &mut self.area
    }

    /// Records an edit made through `area_mut`.
    pub fn bump(&mut self) {
        self.revision += 1;
    }
}

/// Routes one key: overlays first, then global keys, then the editor.
/// Enter on a non-empty editor gives `Action::SubmitAt` and leaves the text in
/// place; the reducer clears it once the server accepted it.
pub fn route_key(
    key: KeyEvent,
    state: &State,
    ui: &mut UiState,
    editor: &mut Editor,
) -> Option<Action> {
    if matches!(key.kind, KeyEventKind::Release) {
        return None;
    }
    if let Some(overlay) = ui.overlay {
        let names: Vec<String> = match overlay {
            Overlay::Models => state
                .catalog
                .iter()
                .filter(|x| {
                    x.alias.to_lowercase().contains(&ui.filter.to_lowercase())
                        || x.model.to_lowercase().contains(&ui.filter.to_lowercase())
                })
                .map(|x| x.alias.clone())
                .collect(),
            Overlay::Sessions => state
                .sessions
                .iter()
                .filter(|x| {
                    x.id.to_lowercase().contains(&ui.filter.to_lowercase())
                        || x.title
                            .as_deref()
                            .unwrap_or("")
                            .to_lowercase()
                            .contains(&ui.filter.to_lowercase())
                })
                .map(|x| x.id.clone())
                .collect(),
            Overlay::Commands => commands::CATALOG
                .iter()
                .filter(|x| {
                    x.name.contains(&ui.filter.to_lowercase())
                        || x.description.contains(&ui.filter.to_lowercase())
                })
                .map(|x| x.name.to_string())
                .collect(),
            Overlay::Help => Vec::new(),
        };
        let selected = ui.selection.entry(format!("{overlay:?}")).or_default();
        match key.code {
            KeyCode::Esc => {
                ui.overlay = None;
                ui.filter.clear();
                return Some(Action::CloseOverlay);
            }
            KeyCode::Up => *selected = selected.saturating_sub(1),
            KeyCode::Down => *selected = (*selected + 1).min(names.len().saturating_sub(1)),
            KeyCode::PageUp => *selected = selected.saturating_sub(8),
            KeyCode::PageDown => *selected = (*selected + 8).min(names.len().saturating_sub(1)),
            KeyCode::Enter => {
                let name = names.get(*selected)?;
                let action = match overlay {
                    Overlay::Models => Action::PickModel(name.clone()),
                    Overlay::Sessions => Action::ResumeSession(name.clone()),
                    Overlay::Commands => commands::command_action(name, ui.mode)?,
                    Overlay::Help => return None,
                };
                ui.overlay = None;
                ui.filter.clear();
                return Some(action);
            }
            KeyCode::Backspace if overlay != Overlay::Help => {
                ui.filter.pop();
                *selected = 0;
            }
            KeyCode::Char(c)
                if overlay != Overlay::Help && !key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                ui.filter.push(c);
                *selected = 0;
            }
            _ => {}
        }
        return None;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    if key.code == KeyCode::Tab || key.code == KeyCode::BackTab {
        ui.focus = if ui.focus == Focus::Editor {
            Focus::Transcript
        } else {
            Focus::Editor
        };
        return None;
    }
    if key.code == KeyCode::Esc && ui.quit_confirm {
        ui.quit_confirm = false;
        return None;
    }
    if key.code == KeyCode::End && ctrl {
        ui.follow_bottom = true;
        ui.scroll = 0;
        return None;
    }
    if key.code == KeyCode::Char('c') && ctrl && state.activity != Activity::Running {
        if ui.quit_confirm {
            return Some(Action::Quit);
        }
        ui.quit_confirm = true;
        return None;
    }
    if key.code == KeyCode::Char('q')
        && ctrl
        && (!editor.text().trim().is_empty() || state.activity == Activity::Running)
        && !ui.quit_confirm
    {
        ui.quit_confirm = true;
        return None;
    }
    if let Some(action) = commands::key_to_action(key) {
        if let Action::OpenOverlay(overlay) = action {
            ui.overlay = Some(overlay);
            ui.filter.clear();
            ui.selection.insert(format!("{overlay:?}"), 0);
        }
        if matches!(action, Action::Scroll(_)) {
            ui.follow_bottom = false;
        }
        return Some(action);
    }
    if ui.focus == Focus::Transcript {
        match key.code {
            KeyCode::Up | KeyCode::Down => {
                let blocks = &state.conversation.blocks;
                if blocks.is_empty() {
                    return None;
                }
                let idx = ui
                    .selected_block
                    .as_ref()
                    .and_then(|id| blocks.iter().position(|b| &b.id == id))
                    .unwrap_or(blocks.len() - 1);
                let next = if key.code == KeyCode::Up {
                    idx.saturating_sub(1)
                } else {
                    (idx + 1).min(blocks.len() - 1)
                };
                ui.selected_block = Some(blocks[next].id.clone());
                ui.follow_bottom = false;
                return None;
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                return ui.selected_block.clone().map(Action::ToggleBlock)
            }
            _ => return None,
        }
    }
    if key.code == KeyCode::Char('/') && editor.text().is_empty() {
        ui.overlay = Some(Overlay::Commands);
        ui.filter.clear();
        ui.selection.insert("Commands".into(), 0);
        return Some(Action::OpenOverlay(Overlay::Commands));
    }
    if key.code == KeyCode::Enter && !alt && !key.modifiers.contains(KeyModifiers::SHIFT) {
        let text = editor.text();
        if let Some(action) = commands::command_action(text.trim(), ui.mode) {
            if let Action::OpenOverlay(overlay) = action {
                ui.overlay = Some(overlay);
                ui.filter.clear();
            }
            return Some(action);
        }
        return (!text.trim().is_empty()).then(|| Action::SubmitAt {
            text,
            revision: editor.revision(),
        });
    }
    let changed = if (key.code == KeyCode::Enter
        && (alt || key.modifiers.contains(KeyModifiers::SHIFT)))
        || (key.code == KeyCode::Char('j') && ctrl)
    {
        editor.area_mut().insert_newline();
        true
    } else {
        editor.area_mut().input_without_shortcuts(key)
    };
    if changed {
        editor.bump();
    }
    None
}

/// Inserts pasted text into the editor (newlines kept), unless an overlay
/// has the focus.
pub fn route_paste(text: String, ui: &UiState, editor: &mut Editor) {
    if ui.overlay.is_none() && ui.focus == Focus::Editor && editor.area_mut().insert_str(text) {
        editor.bump();
    }
}
