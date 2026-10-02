use crate::tui::{
    commands,
    protocol::Overlay,
    state::{State, UiState},
    theme::{Theme, ThemeId},
};
use ratatui::{
    layout::{Alignment, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

pub fn draw(frame: &mut Frame<'_>, area: Rect, state: &State, ui: &UiState, theme: &Theme) {
    let Some(overlay) = ui.overlay else { return };
    let title = match overlay {
        Overlay::Models if state.session.is_some() => " Start a new conversation with… ",
        Overlay::Models => " Choose a model ",
        Overlay::Sessions => " Conversations ",
        Overlay::Commands => " Commands ",
        Overlay::Help => " Keyboard shortcuts ",
        Overlay::Themes => " Theme ",
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
                    Span::raw(format!("{}  ·  ", entry.model)),
                    Span::styled(super::status::number(entry.context_window), theme.number()),
                    Span::styled(" ctx", theme.muted()),
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
                Line::from(vec![
                    Span::raw(entry.title.clone().unwrap_or_else(|| "Untitled".into())),
                    Span::styled(
                        format!(
                            "  ·  {}  ·  {}",
                            entry.updated_at.get(..10).unwrap_or(&entry.updated_at),
                            entry.id
                        ),
                        theme.muted(),
                    ),
                ])
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
                    Span::styled(entry.description, theme.muted()),
                ])
            })
            .collect(),
        Overlay::Help => commands::KEYS
            .iter()
            .map(|(key, description)| {
                Line::from(vec![
                    Span::styled(format!("{key:<24}"), theme.accent()),
                    Span::styled(*description, theme.muted()),
                ])
            })
            .collect(),
        Overlay::Themes => ThemeId::ALL
            .iter()
            .filter(|entry| {
                entry.id().contains(&ui.filter.to_lowercase())
                    || entry
                        .name()
                        .to_lowercase()
                        .contains(&ui.filter.to_lowercase())
            })
            .map(|entry| theme_row(*entry, ui.theme_on_open.unwrap_or(ui.theme), theme))
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
    frame.buffer_mut().set_style(box_area, theme.surface());
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.accent_line())
        .title(Span::styled(title, theme.title()))
        .title_alignment(Alignment::Center);
    frame.render_widget(block, box_area);
    let inner = box_area.inner(ratatui::layout::Margin {
        horizontal: 2,
        vertical: 1,
    });
    if overlay != Overlay::Help {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("filter: ", theme.muted()),
                Span::styled(ui.filter.clone(), theme.text()),
            ])),
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
        if selected == index && overlay != Overlay::Help {
            // The tint spans the popup's inner width, margins included, like a list highlight.
            let row_area = Rect::new(box_area.x + 1, rect.y, box_area.width.saturating_sub(2), 1);
            frame.buffer_mut().set_style(
                row_area,
                Style::default().bg(theme.selection_bg_on_surface()),
            );
            frame.buffer_mut()[(row_area.x, rect.y)]
                .set_symbol("▌")
                .set_fg(theme.accent);
        }
        frame.render_widget(Paragraph::new(row), rect);
    }
}

/// One theme in the picker: a mark for the one in effect, its name and id, and swatches of its
/// accent and signal colours drawn in that theme's own colours.
fn theme_row(entry: ThemeId, current: ThemeId, theme: &Theme) -> Line<'static> {
    let palette = entry.palette();
    let mut spans = vec![
        Span::styled(if entry == current { "● " } else { "  " }, theme.accent()),
        Span::raw(format!("{:<14}", entry.name())),
        Span::styled(format!("{:<12}", entry.id()), theme.muted()),
        Span::styled(" Aa ", Style::default().fg(palette.text).bg(palette.bg)),
        Span::raw(" "),
    ];
    for color in [
        palette.accent,
        palette.green,
        palette.red,
        palette.blue,
        palette.purple,
        palette.orange,
    ] {
        spans.push(Span::styled("■", Style::default().fg(color)));
    }
    Line::from(spans)
}
