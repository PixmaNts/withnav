//! Settings rendering reads the editing projection; it never loads or writes files.
use super::text::{columns, fit_end, pad};
use crate::tui::{
    config::{ConfigRow, RowKind, SaveStatus, Section},
    state::{State, UiState},
    theme::Theme,
};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame,
};
use serde_json::Value;

fn value(v: &Value) -> String {
    match v {
        Value::Null => "—".into(),
        Value::String(s) => s.clone(),
        Value::Array(items) => items.iter().map(value).collect::<Vec<_>>().join(" · "),
        _ => v.to_string(),
    }
}

fn description(key: &str) -> &'static str {
    if key.starts_with("layers.reflex") {
        "Ghost-text suggestions. Runs only through the language server, with an IDE attached."
    } else if key.starts_with("layers.instinct") {
        "Classifies effects, scope and risk. Fixed 2 s deadline."
    } else if key.starts_with("layers.metacognition") {
        "Fast model for remarks, reflex context and risky-call review."
    } else if key.starts_with("agents.") {
        "Cognition assignment. A config.json model overrides the agent's Markdown assignment."
    } else {
        match key {
            "default_agent" => "The primary agent used for new conversations.",
            "permissions.automatic_review" => "Instinct reviews calls; metacognition approves risk. On doubt, the driver decides. Off: ask for every call.",
            "permissions.scratch_dirs" => "Absolute scratch paths. An empty list disables scratch access.",
            "subagents.max_concurrent" => "Maximum concurrent subagents per session.",
            "subagents.watchdog_interval_secs" => "Subagent watchdog interval in seconds. Zero disables it.",
            "agent.watchdog_interval_secs" => "Root agent watchdog interval in seconds. Zero disables it.",
            "tui.theme" => "Terminal colours. Applied when written to config.json.",
            _ => "Gateway catalogue. Read-only here; edit in models.json.",
        }
    }
}

fn section_info(section: Section) -> (&'static str, &'static str) {
    match section {
        Section::Gateways => (
            "models.json → providers",
            "Gateway catalogue. Read-only here; edit in models.json.",
        ),
        Section::Layers => (
            "config.json → layers, default_agent",
            "Reflex, instinct and metacognition run one model each. The cognition runs agents.",
        ),
        Section::Agents => (
            "config.json → agents",
            "One cognition model per agent. Config assignments win over model: in the agent file.",
        ),
        Section::Permissions => (
            "config.json → permissions",
            "Review before execution. The driver decides on doubt, timeout or reviewer failure.",
        ),
        Section::Subagents => (
            "config.json → subagents, agent",
            "Concurrency limits and watchdog intervals. Zero disables a watchdog.",
        ),
        Section::Appearance => (
            "config.json → tui",
            "Four terminal themes. Written for every TUI client.",
        ),
    }
}

fn line(frame: &mut Frame<'_>, area: Rect, y: u16, text: impl Into<Line<'static>>) {
    if y < area.bottom() {
        frame.render_widget(
            Paragraph::new(text.into()),
            Rect::new(area.x, y, area.width, 1),
        );
    }
}

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, state: &State, _ui: &UiState, theme: &Theme) {
    if area.width < 40 || area.height < 9 {
        frame.render_widget(
            Paragraph::new(" Resize terminal for Settings · esc back").style(theme.warning()),
            area,
        );
        return;
    }
    let [header, rule, body, status, keys] = Layout::default()
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(area);
    draw_header(frame, header, state, theme);
    frame.render_widget(
        Paragraph::new("─".repeat(usize::from(rule.width))).style(theme.border()),
        rule,
    );
    let right_width = if area.width >= 110 { 36 } else { 0 };
    let [left, center, right] = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(22),
            Constraint::Min(18),
            Constraint::Length(right_width),
        ])
        .areas(body);
    sections(frame, left, state, theme);
    rows(frame, center, state, theme);
    if right.width > 0 {
        details(frame, right, state, theme);
    }
    frame.buffer_mut().set_style(status, theme.panel());
    let message = if state.config.filtering || !state.config.filter.is_empty() {
        format!(
            " / {}{}",
            state.config.filter,
            if state.config.filtering { "▏" } else { "" }
        )
    } else {
        match &state.config.status {
            SaveStatus::Error { .. } | SaveStatus::StaleReloaded(_) => {
                format!(" {}", state.config.status.text())
            }
            _ => String::new(),
        }
    };
    frame.render_widget(
        Paragraph::new(message).style(if matches!(state.config.status, SaveStatus::Error { .. }) {
            theme.error()
        } else {
            theme.muted()
        }),
        status,
    );
    frame.buffer_mut().set_style(keys, theme.panel());
    let hints = if state.config.leave_prompt {
        "w write · d discard · esc stay"
    } else if state.config.list_editor.is_some() {
        "enter newline · ctrl+s stage list · esc cancel"
    } else {
        "↑↓ move · ←→ change · enter pick / edit · space toggle · / filter · ctrl+s write · ctrl+z revert · esc back"
    };
    let bar = super::key_bar(
        &[super::Hints {
            hints: &[
                "↑↓ move",
                "←→ change",
                "enter pick/edit",
                "space toggle",
                "/ filter",
                "ctrl+s write",
                "ctrl+z revert",
                "esc back",
            ],
            required: 0,
        }],
        usize::from(keys.width),
        theme,
    );
    if state.config.leave_prompt || state.config.list_editor.is_some() {
        frame.render_widget(
            Paragraph::new(format!(" {hints}")).style(theme.muted()),
            keys,
        );
    } else {
        frame.render_widget(Paragraph::new(Line::from(bar)), keys);
    }
}

fn draw_header(frame: &mut Frame<'_>, area: Rect, state: &State, theme: &Theme) {
    frame.buffer_mut().set_style(area, theme.panel());
    let branch = state
        .git
        .as_ref()
        .and_then(|g| g.branch.as_deref())
        .unwrap_or("—");
    let status = match state.config.status {
        SaveStatus::Unsaved(n) => format!("● {n} unsaved"),
        SaveStatus::Written => "✓ written".into(),
        _ => state.config.status.text(),
    };
    let paths = state
        .config
        .body
        .as_ref()
        .map(|b| {
            format!(
                "{}{}",
                b.paths.config,
                b.paths
                    .models
                    .as_ref()
                    .map(|p| format!(" · {p}"))
                    .unwrap_or_default()
            )
        })
        .unwrap_or_default();
    let right = format!("{status}  {paths} ");
    let left_room =
        usize::from(area.width).saturating_sub(columns(&right).min(usize::from(area.width) / 2));
    let left = format!(" {}  {branch}  /config", state.workspace);
    let left = fit_end(&left, left_room.saturating_sub(9));
    let used = 9 + columns(&left);
    let right = fit_end(&right, usize::from(area.width).saturating_sub(used + 1));
    let gap = usize::from(area.width).saturating_sub(used + columns(&right));
    line(
        frame,
        area,
        area.y,
        Line::from(vec![
            Span::styled(" ◆ navai ", theme.accent()),
            Span::styled(left, theme.muted()),
            Span::raw(" ".repeat(gap)),
            Span::styled(
                right,
                if matches!(state.config.status, SaveStatus::Unsaved(_)) {
                    theme.code_ref()
                } else if state.config.status == SaveStatus::Written {
                    theme.success()
                } else {
                    theme.muted()
                },
            ),
        ]),
    );
}

fn sections(frame: &mut Frame<'_>, area: Rect, state: &State, theme: &Theme) {
    frame.buffer_mut().set_style(area, theme.panel());
    frame.render_widget(
        Block::default()
            .borders(Borders::RIGHT)
            .border_style(theme.border()),
        area,
    );
    let inner = Rect::new(area.x, area.y, area.width.saturating_sub(1), area.height);
    line(
        frame,
        inner,
        inner.y + 1,
        Span::styled(" SETTINGS", theme.muted()),
    );
    let summaries = state.config.summaries();
    for section in Section::ALL {
        let y = inner.y + 3 + section.index() as u16 * 3;
        if y + 2 >= inner.bottom() {
            break;
        }
        let count = state
            .config
            .unsaved_in(section, &state.catalog, &state.agents);
        let selected = state.config.section == section;
        let rect = Rect::new(inner.x, y, inner.width, 3);
        if selected {
            frame
                .buffer_mut()
                .set_style(rect, Style::default().bg(theme.selection_bg_on_panel()));
        }
        let title = format!(
            "{}{} {}",
            if selected { "▌" } else { " " },
            section.index() + 1,
            section.label()
        );
        line(
            frame,
            inner,
            y,
            Line::from(vec![
                Span::styled(
                    title,
                    if selected {
                        theme.accent()
                    } else {
                        theme.text()
                    },
                ),
                Span::styled(
                    if count > 0 {
                        format!(" ●{count}")
                    } else {
                        String::new()
                    },
                    theme.code_ref(),
                ),
            ]),
        );
        let summary = Rect::new(inner.x + 1, y + 1, inner.width.saturating_sub(2), 2);
        frame.render_widget(
            Paragraph::new(summaries[section.index()].clone())
                .style(theme.muted())
                .wrap(Wrap { trim: true }),
            summary,
        );
    }
    if inner.height > 30 {
        frame.render_widget(
            Paragraph::new(" 1…6 section\n tab next section\n esc back to session")
                .style(theme.muted()),
            Rect::new(inner.x, inner.bottom() - 4, inner.width, 3),
        );
    }
}

fn group_title(row: &ConfigRow, state: &State) -> String {
    match row.section {
        Section::Layers => match row.group.as_str() {
            "reflex" => "reflex  fim · LSP only".into(),
            "instinct" => "instinct  systemone · 2 s deadline".into(),
            "metacognition" => "metacognition  chat · fast model".into(),
            _ => format!("cognition  chat · {} agents", state.agents.len()),
        },
        Section::Agents => {
            let agent = state.agents.iter().find(|a| a.id == row.group);
            format!(
                "{}  {}",
                row.group,
                agent.map(|a| a.mode.as_str()).unwrap_or("—")
            )
        }
        Section::Gateways => {
            let test = if state.config.testing_gateway.as_deref() == Some(&row.group) {
                "testing…".into()
            } else {
                state
                    .config
                    .gateway_tests
                    .get(&row.group)
                    .map(|r| match r {
                        Ok(t) if t.ok => format!("✓ {} ms", t.latency_ms),
                        Ok(t) => t.error.clone().unwrap_or_else(|| "failed".into()),
                        Err(e) => e.clone(),
                    })
                    .unwrap_or_else(|| "edit in models.json".into())
            };
            format!("{}  {test}", row.group)
        }
        _ => row.group.clone(),
    }
}

fn row_key(row: &ConfigRow) -> &str {
    match row.section {
        Section::Layers | Section::Agents | Section::Gateways => {
            row.key.rsplit('.').next().unwrap_or(&row.key)
        }
        _ => &row.key,
    }
}

fn hint(kind: &RowKind) -> &'static str {
    match kind {
        RowKind::Model(_) => "enter pick",
        RowKind::Choice => "←→ cycle",
        RowKind::Integer { .. } => "←→ ±1",
        RowKind::Bool => "space toggle",
        RowKind::StringList => "enter edit",
        RowKind::ReadOnly => "read-only",
    }
}

/// When a change to the row takes effect, in the words the WebUI and VS Code use for the
/// server's `applies` label; `None` for read-only rows, which carry no label.
fn applies_note(applies: &str) -> Option<&'static str> {
    match applies {
        "restart" => Some("applies after restarting Navai"),
        "new_sessions" => Some("applies to new sessions and subagents"),
        "live" => Some("applies here now; other windows on next start"),
        _ => None,
    }
}

fn rows(frame: &mut Frame<'_>, area: Rect, state: &State, theme: &Theme) {
    let inner = Rect::new(
        area.x + 2,
        area.y,
        area.width.saturating_sub(4),
        area.height,
    );
    let (source, desc) = section_info(state.config.section);
    line(
        frame,
        inner,
        inner.y + 1,
        Line::from(vec![
            Span::styled(state.config.section.label(), theme.accent()),
            Span::styled(format!("  {source}"), theme.muted()),
        ]),
    );
    line(
        frame,
        inner,
        inner.y + 2,
        Span::styled(fit_end(desc, usize::from(inner.width)), theme.muted()),
    );
    let rows = state.config.visible_rows(&state.catalog, &state.agents);
    if rows.is_empty() {
        line(
            frame,
            inner,
            inner.y + 4,
            Span::styled(
                if state.config.body.is_none() {
                    "Loading settings…"
                } else {
                    "No matching settings."
                },
                theme.muted(),
            ),
        );
        return;
    }
    let mut rendered = Vec::new();
    let mut group = "";
    for (i, row) in rows.iter().enumerate() {
        if group != row.group
            && matches!(
                row.section,
                Section::Layers | Section::Agents | Section::Gateways
            )
        {
            rendered.push((None, String::new()));
            rendered.push((None, group_title(row, state)));
            group = &row.group;
        }
        rendered.push((Some(i), String::new()));
        if let Some(error) = state.config.field_errors.get(&row.write_key).or_else(|| {
            state
                .config
                .body
                .as_ref()
                .and_then(|b| b.fields.iter().find(|f| f.key == row.write_key))
                .and_then(|f| f.error.as_ref())
        }) {
            rendered.push((None, format!("! {error}")));
        }
    }
    let available = usize::from(inner.height.saturating_sub(4));
    let selected_line = rendered
        .iter()
        .position(|(i, _)| *i == Some(state.config.row))
        .unwrap_or(0);
    let first = (selected_line + 2).saturating_sub(available);
    let key_width = rows
        .iter()
        .map(|r| columns(row_key(r)))
        .max()
        .unwrap_or(0)
        .min(usize::from(inner.width) / 2);
    for (offset, (index, title)) in rendered.into_iter().skip(first).take(available).enumerate() {
        let y = inner.y + 4 + offset as u16;
        let Some(index) = index else {
            line(
                frame,
                inner,
                y,
                Span::styled(
                    title.clone(),
                    if title.starts_with('!') {
                        theme.error()
                    } else {
                        theme.muted()
                    },
                ),
            );
            continue;
        };
        let row = &rows[index];
        let selected = index == state.config.row;
        let pending = state.config.staged.contains_key(&row.write_key);
        let rect = Rect::new(area.x, y, area.width, 1);
        if selected {
            frame.buffer_mut().set_style(rect, theme.selected());
            frame.buffer_mut()[(area.x, y)]
                .set_symbol("▌")
                .set_fg(theme.accent);
        }
        let v = state.config.row_value(row);
        let style = match row.kind {
            RowKind::ReadOnly => theme.muted(),
            RowKind::Bool if v.as_bool() == Some(true) => theme.success(),
            RowKind::Bool => theme.error(),
            RowKind::Integer { .. } => theme.number(),
            RowKind::Choice => theme.accent(),
            _ => theme.text(),
        };
        let h = if selected { hint(&row.kind) } else { "" };
        let remaining = usize::from(inner.width).saturating_sub(key_width + columns(h) + 7);
        let v = fit_end(&value(&v), remaining);
        let gap =
            usize::from(inner.width).saturating_sub(2 + key_width + 2 + columns(&v) + columns(h));
        line(
            frame,
            inner,
            y,
            Line::from(vec![
                Span::styled(if pending { "● " } else { "  " }, theme.code_ref()),
                Span::styled(
                    pad(&fit_end(row_key(row), key_width), key_width),
                    if selected {
                        theme.text()
                    } else {
                        theme.muted()
                    },
                ),
                Span::raw("  "),
                Span::styled(v, style),
                Span::raw(" ".repeat(gap)),
                Span::styled(h, theme.muted()),
            ]),
        );
    }
}

fn details(frame: &mut Frame<'_>, area: Rect, state: &State, theme: &Theme) {
    frame.buffer_mut().set_style(area, theme.panel());
    frame.render_widget(
        Block::default()
            .borders(Borders::LEFT)
            .border_style(theme.border()),
        area,
    );
    let inner = Rect::new(
        area.x + 2,
        area.y + 1,
        area.width.saturating_sub(4),
        area.height.saturating_sub(2),
    );
    let rows = state.config.visible_rows(&state.catalog, &state.agents);
    let mut lines = vec![Line::from(Span::styled("FOCUSED", theme.muted()))];
    if let Some(row) = rows.get(state.config.row) {
        lines.push(Line::from(Span::styled(row.key.clone(), theme.accent())));
        lines.push(Line::default());
        lines.push(Line::from(description(&row.key)));
        lines.push(Line::default());
        let kind = match row.kind {
            RowKind::Model(api) => format!(
                "model · {} catalogue",
                match api {
                    crate::tui::protocol::ApiKind::Fim => "fim",
                    crate::tui::protocol::ApiKind::SystemOne => "systemone",
                    crate::tui::protocol::ApiKind::Chat => "chat",
                    crate::tui::protocol::ApiKind::Unknown => "unknown",
                }
            ),
            RowKind::Choice => "choice".into(),
            RowKind::Integer { min, max } => format!("integer · {min}–{max}"),
            RowKind::Bool => "boolean".into(),
            RowKind::StringList => "absolute path list".into(),
            RowKind::ReadOnly => "read-only".into(),
        };
        lines.push(Line::from(vec![
            Span::styled("type    ", theme.muted()),
            Span::raw(kind),
        ]));
        let allowed = if let RowKind::Model(api) = row.kind {
            format!(
                "<gateway>/<id> · {} models",
                state.catalog.iter().filter(|m| m.api == api).count()
            )
        } else if matches!(row.kind, RowKind::Integer { .. }) {
            String::new()
        } else {
            row.allowed
                .iter()
                .map(value)
                .collect::<Vec<_>>()
                .join(" · ")
        };
        if !allowed.is_empty() {
            lines.push(Line::from(vec![
                Span::styled("values  ", theme.muted()),
                Span::raw(allowed),
            ]));
        }
        lines.push(Line::from(vec![
            Span::styled("saved   ", theme.muted()),
            Span::raw(fit_end(
                &value(&row.saved),
                usize::from(inner.width.saturating_sub(8)),
            )),
        ]));
        if let Some(note) = applies_note(&row.applies) {
            lines.push(Line::from(Span::styled(note, theme.muted())));
        }
    }
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        format!("PENDING · {}", state.config.staged.len()),
        theme.muted(),
    )));
    if state.config.staged.is_empty() {
        lines.push(Line::from(Span::styled(
            "No changes. config.json is up to date.",
            theme.muted(),
        )));
    } else {
        for (key, new) in &state.config.staged {
            lines.push(Line::from(Span::styled(key.clone(), theme.code_ref())));
            if let Some(field) = state
                .config
                .body
                .as_ref()
                .and_then(|b| b.fields.iter().find(|f| &f.key == key))
            {
                lines.push(Line::from(Span::styled(
                    format!("- {}", value(field.effective())),
                    theme.error(),
                )));
            }
            lines.push(Line::from(Span::styled(
                format!(
                    "+ {}",
                    if new.is_null() {
                        "default (unset)".into()
                    } else {
                        value(new)
                    }
                ),
                theme.success(),
            )));
        }
        lines.push(Line::from(Span::styled(
            "[ctrl+s] write  [ctrl+z] revert",
            theme.accent(),
        )));
    }
    lines.push(Line::default());
    lines.push(Line::from(Span::styled("▌ Written for every client: TUI, WebUI and VS Code. A /models pick in a session stays in that session and never writes here.", theme.muted())));
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

pub(super) fn overlays(frame: &mut Frame<'_>, area: Rect, state: &State, theme: &Theme) {
    let content = if state.config.leave_prompt {
        format!(
            "{} unsaved changes. Leave Settings?\n\n[w] write · [d] discard · [esc] stay",
            state.config.staged.len()
        )
    } else if let Some(text) = &state.config.list_editor {
        format!("One absolute path per line:\n\n{text}▏\n\n[ctrl+s] stage list · [esc] cancel")
    } else {
        return;
    };
    let width = area.width.saturating_sub(4).min(66);
    let height = if state.config.leave_prompt { 7 } else { 16 }.min(area.height.saturating_sub(2));
    let rect = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(content)
            .style(theme.surface())
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(theme.accent_line())
                    .title(if state.config.leave_prompt {
                        " Leave Settings "
                    } else {
                        " Scratch directories "
                    })
                    .padding(ratatui::widgets::Padding::new(2, 2, 1, 1)),
            ),
        rect,
    );
}
