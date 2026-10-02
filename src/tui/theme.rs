//! Semantic terminal styles; no layout or state transitions.
use ratatui::style::{Color, Modifier, Style};

#[derive(Debug, Clone, Copy, Default)]
pub struct Theme;
impl Theme {
    pub fn accent(self) -> Style {
        Style::default().fg(Color::Cyan)
    }
    pub fn muted(self) -> Style {
        Style::default().fg(Color::Gray)
    }
    pub fn strong(self) -> Style {
        Style::default().add_modifier(Modifier::BOLD)
    }
    pub fn error(self) -> Style {
        Style::default().fg(Color::Red)
    }
    pub fn warning(self) -> Style {
        Style::default().fg(Color::Yellow)
    }
    pub fn success(self) -> Style {
        Style::default().fg(Color::Green)
    }
    pub fn selected(self) -> Style {
        Style::default().add_modifier(Modifier::REVERSED)
    }
    pub fn border(self) -> Style {
        Style::default().fg(Color::DarkGray)
    }
}
