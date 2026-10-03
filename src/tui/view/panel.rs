//! The right column: what the conversation runs with, only from what the server and git report.
//! Agents (the session's agent and the subagents it can start), Context (the breakdown of the
//! last snapshot and the last compaction), Skills (the catalog counts) and Changes (the files git
//! reports). A section with nothing reported yet shows `—`; none is made up.
use crate::{
    agent::definitions::Agents,
    tui::{
        reducer::tokens,
        state::{Activity, State, UiState},
        theme::Theme,
    },
};
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

/// Columns the column takes, its left border included.
pub const WIDTH: u16 = 36;
/// Below this terminal width the column hides itself, unless the driver asked for it.
pub const MIN_TERMINAL_WIDTH: u16 = 140;
/// The subagents every session can start.
const SUBAGENTS: [&str; 2] = ["explore", "review"];

/// Whether the column is drawn on a terminal `width` columns wide.
pub fn shown(ui: &UiState, width: u16) -> bool {
    ui.panel_shown.unwrap_or(width >= MIN_TERMINAL_WIDTH)
}

fn title(text: &str, theme: &Theme) -> Line<'static> {
    Line::styled(
        text.to_uppercase(),
        theme.muted().add_modifier(Modifier::BOLD),
    )
}

/// `label` then `value` pushed to the right edge of `width`.
fn row(label: &str, value: String, width: usize, theme: &Theme) -> Line<'static> {
    styled_row(label, value, width, theme.text(), theme.number())
}

fn styled_row(
    label: &str,
    value: String,
    width: usize,
    label_style: Style,
    value_style: Style,
) -> Line<'static> {
    let gap = width.saturating_sub(label.chars().count() + value.chars().count() + 1);
    Line::from(vec![
        Span::styled(format!(" {label}"), label_style),
        Span::raw(" ".repeat(gap)),
        Span::styled(value, value_style),
    ])
}

fn none(theme: &Theme) -> Line<'static> {
    Line::styled(" —", theme.muted())
}

/// `text` cut to `width` characters with `…`.
fn fit(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(width.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

fn agents(state: &State, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let builtin = Agents::builtin();
    let agent = state.agent_name();
    let style = super::sidebar::agent_style(agent, theme);
    let mut lines = vec![
        title("Agents", theme),
        Line::from(vec![
            Span::styled(" ● ", style),
            Span::styled(agent.to_string(), style.add_modifier(Modifier::BOLD)),
            Span::styled("  in use", theme.muted()),
        ]),
    ];
    // The role of a built-in agent; one from a file has its own, which the server has not sent.
    if let Some(definition) = builtin.get(agent) {
        for part in wrap(&definition.description, width.saturating_sub(3)) {
            lines.push(Line::styled(format!("   {part}"), theme.muted()));
        }
    }
    for child in &state.children {
        let running = child.activity == Activity::Running;
        lines.push(Line::from(vec![
            Span::styled(if running { " ◐ " } else { " ○ " }, theme.subagent()),
            Span::styled(fit(&child.name, width.saturating_sub(14)), theme.subagent()),
            Span::styled(if running { "  running" } else { "  done" }, theme.muted()),
        ]));
    }
    for name in SUBAGENTS {
        if let Some(definition) = builtin.get(name) {
            lines.push(Line::from(vec![
                Span::styled(" ○ ", theme.muted()),
                Span::styled(name.to_string(), theme.subagent()),
                Span::styled(
                    fit(
                        &format!("  {}", definition.description.to_lowercase()),
                        width.saturating_sub(3 + name.len()),
                    ),
                    theme.muted(),
                ),
            ]));
        }
    }
    lines
}

fn context(state: &State, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let mut lines = vec![title("Context", theme)];
    let Some(c) = &state.conversation.context else {
        lines.push(none(theme));
        return lines;
    };
    let b = &c.breakdown;
    let rules = if c.rule_files == 1 {
        "rules · 1 file".to_string()
    } else {
        format!("rules · {} files", c.rule_files)
    };
    for (label, value) in [
        ("system", b.system),
        (rules.as_str(), b.rules),
        ("skills", b.skills_catalog),
        ("tools", b.tools),
        ("messages", b.messages),
    ] {
        lines.push(row(label, format!("~{}", tokens(value)), width, theme));
    }
    lines.push(styled_row(
        "next request",
        format!("~{} · {}%", tokens(c.estimate_total), c.percent),
        width,
        theme.strong(),
        theme.number(),
    ));
    lines.push(row("input budget", tokens(c.input_budget), width, theme));
    if let Some(done) = &state.conversation.last_compaction {
        lines.push(Line::styled(
            fit(
                &format!(
                    " compacted ×{} · ~{} → ~{}",
                    done.generation,
                    tokens(done.estimate_before),
                    tokens(done.estimate_after)
                ),
                width,
            ),
            theme.muted(),
        ));
    }
    lines
}

fn skills(state: &State, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let mut lines = vec![title("Skills", theme)];
    match &state.conversation.context {
        None => lines.push(none(theme)),
        Some(c) => {
            lines.push(row("listed", c.skills.listed.to_string(), width, theme));
            if c.skills.omitted > 0 {
                lines.push(styled_row(
                    "omitted, no room",
                    c.skills.omitted.to_string(),
                    width,
                    theme.warning(),
                    theme.warning(),
                ));
            }
        }
    }
    lines
}

/// `None` when the workspace is not a git repository: the section is not drawn. At most `rows`
/// lines: the files that do not fit are counted with the rest.
fn changes(state: &State, width: usize, rows: usize, theme: &Theme) -> Option<Vec<Line<'static>>> {
    let git = state.git.as_ref()?;
    let mut lines = vec![title("Changes", theme)];
    if git.dirty == 0 {
        lines.push(Line::styled(" no changes", theme.muted()));
    }
    // The title and, when some are left out, the `N more` line take a row each.
    let fit_all = git.files.len() == git.dirty && git.files.len() < rows;
    let shown = if fit_all {
        git.files.len()
    } else {
        rows.saturating_sub(2).min(git.files.len())
    };
    for file in &git.files[..shown] {
        let mark = match file.mark {
            'A' | '?' => theme.success(),
            'D' => theme.error(),
            _ => Style::default().fg(theme.orange),
        };
        lines.push(Line::from(vec![
            Span::styled(format!(" {} ", file.mark), mark),
            Span::styled(fit_start(&file.path, width.saturating_sub(4)), theme.text()),
        ]));
    }
    if git.dirty > shown {
        lines.push(Line::styled(
            format!("   {} more", git.dirty - shown),
            theme.muted(),
        ));
    }
    Some(lines)
}

/// `path` cut from its start, keeping the file name.
fn fit_start(path: &str, width: usize) -> String {
    let chars: Vec<char> = path.chars().collect();
    if chars.len() <= width {
        return path.to_string();
    }
    let tail: String = chars[chars.len() - width.saturating_sub(1)..]
        .iter()
        .collect();
    format!("…{tail}")
}

/// `text` broken into lines of at most `width` characters, at spaces.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        match lines.last_mut() {
            Some(line) if line.chars().count() + 1 + word.chars().count() <= width => {
                line.push(' ');
                line.push_str(word);
            }
            _ => lines.push(fit(word, width)),
        }
    }
    lines
}

pub fn draw(frame: &mut Frame<'_>, area: Rect, state: &State, theme: &Theme) {
    if area.width < 8 || area.height == 0 {
        return;
    }
    frame.buffer_mut().set_style(area, theme.panel());
    for y in area.top()..area.bottom() {
        frame.buffer_mut()[(area.x, y)]
            .set_symbol("│")
            .set_style(theme.border());
    }
    let inner = Rect {
        x: area.x + 2,
        width: area.width - 3,
        y: area.y + 1,
        height: area.height.saturating_sub(1),
    };
    let width = usize::from(inner.width);
    let mut lines = agents(state, width, theme);
    for section in [context(state, width, theme), skills(state, width, theme)] {
        lines.push(Line::raw(""));
        lines.extend(section);
    }
    let left = usize::from(inner.height).saturating_sub(lines.len() + 1);
    if let Some(section) = changes(state, width, left, theme).filter(|_| left >= 3) {
        lines.push(Line::raw(""));
        lines.extend(section);
    }
    frame.render_widget(Paragraph::new(lines), inner);
}
