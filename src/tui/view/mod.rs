//! Draws the terminal UI from state; must not perform I/O or mutate application state.
mod markdown;
mod pickers;
mod status;
mod tool;
mod transcript;
use crate::tui::{
    input::Editor,
    state::{State, UiState},
};
#[derive(Debug, Default)]
pub struct RenderCache;
pub fn draw(
    _frame: &mut ratatui::Frame<'_>,
    _state: &State,
    _ui: &UiState,
    _editor: &Editor,
    _cache: &mut RenderCache,
) {
}
