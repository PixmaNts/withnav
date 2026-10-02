//! Command catalog and key mapping; no I/O or rendering.
use super::protocol::Action;
#[derive(Debug, Clone, Copy)]
pub struct Command {
    pub name: &'static str,
    pub description: &'static str,
}
pub const CATALOG: &[Command] = &[];
pub fn key_to_action(_key: crossterm::event::KeyEvent) -> Option<Action> {
    None
}
