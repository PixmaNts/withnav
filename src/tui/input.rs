//! The message editor and key routing: turns terminal keys and pastes into
//! editor changes or `Action`s. No networking, and no change to `State`.
use super::protocol::Action;
use super::state::{State, UiState};
use crossterm::event::KeyEvent;
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
/// Enter on a non-empty editor gives `Action::Submit` and leaves the text in
/// place; the reducer clears it once the server accepted it.
pub fn route_key(
    _key: KeyEvent,
    _state: &State,
    _ui: &mut UiState,
    _editor: &mut Editor,
) -> Option<Action> {
    None
}

/// Inserts pasted text into the editor (newlines kept), unless an overlay
/// has the focus.
pub fn route_paste(_text: String, _ui: &UiState, _editor: &mut Editor) {}
