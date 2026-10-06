use super::text::{columns, fit_end as fit, pad};
use crate::tui::{
    commands,
    protocol::{ModelEntry, Overlay},
    state::{Activity, State, UiState, VariantPick, NO_VARIANT},
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
        Overlay::Agents => " Choose an agent · assigned model ",
        Overlay::Skills => " Choose a skill ",
        Overlay::Models if crate::tui::reducer::occupied(state) => " Switch this conversation to… ",
        Overlay::Models => " Choose a model ",
        Overlay::Sessions => " Conversations ",
        Overlay::Commands => " Commands ",
        Overlay::Help => " Keyboard shortcuts ",
        Overlay::Themes => " Theme ",
        Overlay::Children => " Subagents · read-only transcripts ",
        Overlay::Variants => match &ui.variant_pick {
            Some(VariantPick::NewConversation(_)) => " Start on variant… ",
            _ => " Variant ",
        },
    };
    let width = area.width.saturating_sub(4).clamp(4, 76);
    // The popup's inner width, less its margins: what a row can take.
    let row_width = usize::from(width.saturating_sub(6));
    let rows: Vec<Line<'static>> = match overlay {
        Overlay::Agents => state
            .agents
            .iter()
            .filter(|a| {
                matches!(a.mode.as_str(), "primary" | "all")
                    && a.id.to_lowercase().contains(&ui.filter.to_lowercase())
            })
            .map(|a| {
                Line::from(format!(
                    "{}{} · {}",
                    if a.id == state.default_agent {
                        "● "
                    } else {
                        "  "
                    },
                    a.id,
                    a.effective
                        .as_ref()
                        .map(|s| s.model.as_str())
                        .unwrap_or("unassigned")
                ))
            })
            .collect(),
        Overlay::Skills => commands::skill_rows(state, &ui.filter)
            .into_iter()
            .map(|skill| {
                let name = fit(&skill.name, row_width);
                let remaining = row_width.saturating_sub(columns(&name) + 3);
                Line::from(vec![
                    Span::styled(name, theme.accent()),
                    Span::styled(
                        format!(" · {}", fit(&skill.description, remaining)),
                        theme.muted(),
                    ),
                ])
            })
            .collect(),
        Overlay::Children => state
            .child_rows(&ui.filter)
            .iter()
            .map(|child| {
                Line::from(vec![
                    Span::styled(child.name.clone(), theme.subagent()),
                    Span::styled(
                        format!(
                            " · {} · {}",
                            child.status.label(),
                            child.goal.split_whitespace().collect::<Vec<_>>().join(" ")
                        ),
                        theme.muted(),
                    ),
                ])
            })
            .collect(),
        Overlay::Models => crate::tui::catalog::rows(&state.catalog, ui)
            .into_iter()
            .map(|row| {
                // Two cells per level, never more than the row itself.
                let indent = (usize::from(row.level()) * 2).min(row_width);
                let room = row_width - indent;
                let star = match &row {
                    crate::tui::catalog::Row::Favorite(entry)
                    | crate::tui::catalog::Row::Model(entry) => {
                        ui.favorite_models.contains(&entry.name)
                    }
                    _ => false,
                };
                let mut line = match row {
                    crate::tui::catalog::Row::Group {
                        label, collapsed, ..
                    } => Line::from(Span::styled(
                        fit(
                            &format!("{} {label}", if collapsed { "▸" } else { "▾" }),
                            room,
                        ),
                        theme.accent(),
                    )),
                    crate::tui::catalog::Row::Section { label } => Line::from(Span::styled(
                        fit(&format!("★ {label}"), room),
                        theme.accent(),
                    )),
                    crate::tui::catalog::Row::Model(entry)
                    | crate::tui::catalog::Row::Favorite(entry) => model_row(entry, room, theme),
                };
                // The star takes the last two cells of the indent, where a group has its fold
                // mark, so favorites mark without moving any column.
                if star && indent >= 2 {
                    line.spans.insert(0, Span::styled("★ ", theme.accent()));
                    line.spans.insert(0, Span::raw(" ".repeat(indent - 2)));
                } else {
                    line.spans.insert(0, Span::raw(" ".repeat(indent)));
                }
                line
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
        Overlay::Commands => commands::palette_rows(state, &ui.filter, ui.palette_typed)
            .into_iter()
            .map(|row| {
                let usage = format!("{} {}", row.name, row.args_hint);
                Line::from(vec![
                    Span::styled(format!("{:<22}", usage.trim_end()), theme.accent()),
                    Span::styled(row.description, theme.muted()),
                ])
            })
            .collect(),
        Overlay::Help => commands::KEYS
            .iter()
            .filter(|(key, _)| match *key {
                "alt+s" => state.tabs.iter().any(|t| !t.children.is_empty()),
                "alt+c" => state.read_only() || state.watchdog_paused,
                _ => true,
            })
            .map(|(key, description)| {
                Line::from(vec![
                    Span::styled(format!("{key:<24}"), theme.accent()),
                    Span::styled(*description, theme.muted()),
                ])
            })
            .collect(),
        Overlay::Variants => {
            let pick = ui.variant_pick.clone().unwrap_or(VariantPick::Current);
            let (names, _) = commands::variant_choices(state, &pick);
            let (default, current) = match &pick {
                VariantPick::NewConversation(model_id) => (
                    state
                        .catalog
                        .iter()
                        .find(|m| &m.name == model_id)
                        .and_then(|m| m.default_variant.clone()),
                    None,
                ),
                VariantPick::Current => (
                    None,
                    Some(state.variant.clone().unwrap_or_else(|| NO_VARIANT.into())),
                ),
            };
            names
                .into_iter()
                .filter(|v| v.to_lowercase().contains(&ui.filter.to_lowercase()))
                .map(|name| {
                    let mark = if current.as_deref() == Some(name.as_str()) {
                        "● "
                    } else {
                        "  "
                    };
                    let note = if default.as_deref() == Some(name.as_str()) {
                        "default"
                    } else if name == NO_VARIANT {
                        "no variant"
                    } else {
                        ""
                    };
                    Line::from(vec![
                        Span::styled(mark, theme.accent()),
                        Span::styled(format!("{name:<20}"), theme.text()),
                        Span::styled(note, theme.muted()),
                    ])
                })
                .collect()
        }
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
    let note = note(overlay, state, ui, rows.is_empty());
    // Filter line, rows, note, then a blank line and the keys line.
    let height = (rows.len() + 5 + usize::from(note.is_some()))
        .min(usize::from(area.height.saturating_sub(2)))
        .max(3) as u16;
    let box_area = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    // The screen behind is dimmed, so the popup reads as the one thing to answer.
    dim(frame, area, theme);
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
    let keys = Rect::new(inner.x, inner.bottom().saturating_sub(1), inner.width, 1);
    let inner = Rect {
        height: inner.height.saturating_sub(2),
        ..inner
    };
    frame.render_widget(
        Paragraph::new(Span::styled(keys_line(overlay), theme.muted())),
        keys,
    );
    if overlay != Overlay::Help {
        let placeholder = match overlay {
            Overlay::Commands => "type a command…",
            Overlay::Models => "filter models…",
            _ => "filter…",
        };
        let typed = if ui.filter.is_empty() {
            Span::styled(placeholder, theme.muted())
        } else {
            Span::styled(ui.filter.clone(), theme.text())
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![Span::styled("❯ ", theme.accent()), typed])),
            Rect { height: 1, ..inner },
        );
    }
    let mut offset = u16::from(overlay != Overlay::Help);
    if let Some(note) = note {
        frame.render_widget(
            Paragraph::new(Span::styled(note, theme.muted())),
            Rect::new(inner.x, inner.y + offset, inner.width, 1),
        );
        offset += 1;
    }
    let selected = ui
        .selection
        .get(&format!("{overlay:?}"))
        .copied()
        .unwrap_or(0);
    // A list longer than the popup scrolls so the selected row stays in view.
    let visible = usize::from(inner.height.saturating_sub(offset));
    let first = (selected + 1).saturating_sub(visible);
    for (index, row) in rows.into_iter().enumerate().skip(first).take(visible) {
        let rect = Rect::new(
            inner.x,
            inner.y + offset + (index - first) as u16,
            inner.width,
            1,
        );
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

/// The keys a popup takes, at its foot.
fn keys_line(overlay: Overlay) -> &'static str {
    match overlay {
        Overlay::Agents => "↑↓ choose · enter assigned model · F2 override · esc close",
        Overlay::Skills => "↑↓ move  enter fill draft  esc close",
        Overlay::Help => "esc close",
        Overlay::Themes => "↑↓ preview  enter keep  esc put back",
        Overlay::Commands => "↑↓ move  enter run  esc close",
        Overlay::Models => "↑↓ move ←→ fold alt+a all ^f fav enter choose esc",
        _ => "↑↓ move  enter choose  esc close",
    }
}

/// Darkens what is behind a popup: the text goes muted, the background stays.
fn dim(frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
    let faded = crate::tui::theme::blend(theme.muted, theme.bg, 0.6);
    let buffer = frame.buffer_mut();
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            buffer[(x, y)].set_fg(faded);
        }
    }
}

pub fn confirm_child(
    frame: &mut Frame<'_>,
    area: Rect,
    agent: &str,
    control: crate::tui::protocol::ChildControl,
    theme: &Theme,
) {
    dim(frame, area, theme);
    let width = area.width.saturating_sub(4).min(60);
    let height = area.height.saturating_sub(2).min(5);
    let rect = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, rect);
    let verb = match control {
        crate::tui::protocol::ChildControl::Stop => "Stop",
        crate::tui::protocol::ChildControl::Continue => "Continue",
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.accent_line())
        .title(format!(" {verb} subagent "))
        .title_alignment(Alignment::Center);
    frame.render_widget(
        Paragraph::new(format!(
            " {verb} {}?\n\n enter / y confirm · esc / n cancel",
            fit(agent, usize::from(width.saturating_sub(14)))
        ))
        .style(theme.surface())
        .block(block),
        rect,
    );
}

/// A line under the filter: what picking does to the live conversation (switched after the
/// running turn, refused while the agent waits for the driver, ctrl+n for a new tab), or why the
/// variant list is empty.
fn note<'a>(overlay: Overlay, state: &'a State, ui: &UiState, empty: bool) -> Option<&'a str> {
    let live = crate::tui::reducer::occupied(state);
    let blocked = crate::tui::reducer::switch_blocked(state);
    match overlay {
        Overlay::Skills => state.skills.disabled_reason.as_deref().or(if empty {
            Some("no listed skills match")
        } else {
            Some("enter fills /skill <name>; add a request, then send")
        }),
        Overlay::Models if live => Some(match blocked {
            Some(why) => why,
            None if state.activity == Activity::Running => {
                "the model only, after this turn; /variant after the switch · ctrl+n new tab"
            }
            None => "switches the model only; /variant after the switch · ctrl+n new tab",
        }),
        Overlay::Variants => {
            if empty && ui.filter.is_empty() {
                return Some(match ui.variant_pick {
                    Some(VariantPick::NewConversation(_)) => {
                        "this model has no variants · enter starts without one"
                    }
                    _ => "this model has no variants",
                });
            }
            if state.activity != Activity::Running {
                return None;
            }
            match ui.variant_pick {
                Some(VariantPick::Current) | None => Some("applies next turn"),
                Some(VariantPick::NewConversation(_)) => None,
            }
        }
        _ => None,
    }
}

/// One model in the picker: model_id, model, context window and variant names, cut to `width`.
fn model_row(entry: &ModelEntry, width: usize, theme: &Theme) -> Line<'static> {
    let window = entry
        .context_window
        .map(crate::tui::reducer::tokens)
        .unwrap_or_else(|| "—".into());
    let variants = if entry.variants.is_empty() {
        String::new()
    } else {
        format!("  {}", entry.variants.join(" "))
    };
    // model_id (18) + model + "  " + window + variants; the model gives way first, then variants.
    // A narrow popup cuts the model_id with an ellipsis rather than clipping it at the border.
    let model_id = format!("{} ", pad(&entry.name, width.saturating_sub(1).min(17)));
    let fixed = columns(&model_id) + 2 + columns(&window);
    let variants = fit(&variants, width.saturating_sub(fixed + 12));
    let model_room = width.saturating_sub(fixed + columns(&variants));
    let model = pad(&entry.upstream_id, model_room);
    let mut spans = vec![
        Span::styled(model_id, theme.accent()),
        Span::styled(model, theme.text()),
        Span::raw("  "),
        Span::styled(window, theme.number()),
        Span::styled(variants, theme.subagent()),
    ];
    if entry.enabled == Some(false) {
        for span in &mut spans {
            span.style = theme.muted();
        }
    }
    Line::from(spans)
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

#[cfg(test)]
mod skill_tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    #[test]
    fn skills_picker_renders_on_a_narrow_terminal() {
        let mut state = State::default();
        state.skills.skills.push(crate::bus::event::SkillEntry {
            name: "review".into(),
            description: "Check correctness".into(),
        });
        let ui = UiState {
            overlay: Some(Overlay::Skills),
            ..Default::default()
        };
        let theme = ui.theme.palette();
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        terminal
            .draw(|frame| draw(frame, frame.area(), &state, &ui, &theme))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("Choose a skill"));
        assert!(text.contains("review"));
        assert!(text.contains("Check correctness"));
    }
}
