use crate::tui::{
    commands,
    protocol::Overlay,
    state::{State, UiState},
    theme::Theme,
};
use ratatui::{
    layout::{Alignment, Rect},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

pub fn draw(frame: &mut Frame<'_>, area: Rect, state: &State, ui: &UiState) {
    let Some(overlay) = ui.overlay else { return };
    let theme = Theme;
    let title = match overlay {
        Overlay::Models if state.session.is_some() => " Start a new conversation with… ",
        Overlay::Models => " Choose a model ",
        Overlay::Sessions => " Conversations ",
        Overlay::Commands => " Commands ",
        Overlay::Help => " Keyboard shortcuts ",
    };
    let rows: Vec<Line<'static>> = match overlay {
        Overlay::Models => state
            .catalog
            .iter()
            .filter(|entry| {
                entry
                    .alias
                    .to_lowercase()
                    .contains(&ui.filter.to_lowercase())
                    || entry
                        .model
                        .to_lowercase()
                        .contains(&ui.filter.to_lowercase())
            })
            .map(|entry| {
                Line::from(vec![
                    Span::styled(format!("{:<17}", entry.alias), theme.accent()),
                    Span::raw(format!(
                        "{}  ·  {} ctx",
                        entry.model,
                        super::status::number(entry.context_window)
                    )),
                ])
            })
            .collect(),
        Overlay::Sessions => state
            .sessions
            .iter()
            .filter(|entry| {
                entry.id.to_lowercase().contains(&ui.filter.to_lowercase())
                    || entry
                        .title
                        .as_deref()
                        .unwrap_or("")
                        .to_lowercase()
                        .contains(&ui.filter.to_lowercase())
            })
            .map(|entry| {
                Line::raw(format!(
                    "{}  ·  {}  ·  {}",
                    entry.title.as_deref().unwrap_or("Untitled"),
                    entry.updated_at.get(..10).unwrap_or(&entry.updated_at),
                    entry.id
                ))
            })
            .collect(),
        Overlay::Commands => commands::CATALOG
            .iter()
            .filter(|entry| {
                entry.name.contains(&ui.filter.to_lowercase())
                    || entry.description.contains(&ui.filter.to_lowercase())
            })
            .map(|entry| {
                Line::from(vec![
                    Span::styled(format!("{:<13}", entry.name), theme.accent()),
                    Span::raw(entry.description),
                ])
            })
            .collect(),
        Overlay::Help => commands::KEYS
            .iter()
            .map(|(key, description)| {
                Line::from(vec![
                    Span::styled(format!("{key:<24}"), theme.accent()),
                    Span::raw(*description),
                ])
            })
            .collect(),
    };
    let height = (rows.len() + 4)
        .min(usize::from(area.height.saturating_sub(2)))
        .max(3) as u16;
    let width = area.width.saturating_sub(4).clamp(4, 76);
    let box_area = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, box_area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.accent())
        .title(title)
        .title_alignment(Alignment::Center);
    frame.render_widget(block, box_area);
    let inner = box_area.inner(ratatui::layout::Margin {
        horizontal: 2,
        vertical: 1,
    });
    if overlay != Overlay::Help {
        frame.render_widget(
            Paragraph::new(format!("filter: {}", ui.filter)).style(theme.muted()),
            Rect { height: 1, ..inner },
        );
    }
    let offset = u16::from(overlay != Overlay::Help);
    let selected = ui
        .selection
        .get(&format!("{overlay:?}"))
        .copied()
        .unwrap_or(0);
    for (index, row) in rows.into_iter().enumerate() {
        if index as u16 >= inner.height.saturating_sub(offset) {
            break;
        }
        let rect = Rect::new(inner.x, inner.y + offset + index as u16, inner.width, 1);
        let style = if selected == index && overlay != Overlay::Help {
            theme.selected()
        } else {
            ratatui::style::Style::default()
        };
        frame.render_widget(Paragraph::new(row).style(style), rect);
    }
}
