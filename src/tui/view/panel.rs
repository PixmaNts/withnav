//! The right column: what the conversation runs with, only from what the server and git report.
//! Agents (the session's agent and the agents it started), Context (the breakdown of the
//! last snapshot and the last compaction), Skills · MCP · Plugins (the skill catalog counts, the
//! skills the model loaded, a dot per MCP server of the service), Layers (one dot each, from
//! the assignment check and the conversation's activity) and Changes (the files git reports). A
//! section with nothing reported yet shows `—`; none is made up.
use super::text::{columns, fit_end as fit, fit_start};
use crate::tools::ToolStatus;
use crate::tui::{
    reducer::tokens,
    state::{Activity, BlockKind, ConnectionStatus, State, UiState},
    theme::Theme,
};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

/// Columns the column takes, its left border included.
pub const WIDTH: u16 = 36;
/// Below this terminal width the column hides itself, unless the Captain asked for it.
pub const MIN_TERMINAL_WIDTH: u16 = 140;

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
    let gap = width.saturating_sub(columns(label) + columns(&value) + 1);
    Line::from(vec![
        Span::styled(format!(" {label}"), label_style),
        Span::raw(" ".repeat(gap)),
        Span::styled(value, value_style),
    ])
}

fn none(theme: &Theme) -> Line<'static> {
    Line::styled(" —", theme.muted())
}

/// One dot per layer: green ready, orange busy, red off. Reflex is not listed: it needs the
/// editor's language server, which the terminal client does not run.
fn layers(state: &State, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let mut lines = vec![title("Layers", theme)];
    for layer in state.layers.iter().filter(|l| l.layer != "reflex") {
        let (style, label) = if layer.status == "usable" {
            let role = match layer.layer.as_str() {
                "instinct" => "triage",
                "metacognition" => "beacon",
                other => other,
            };
            (theme.success(), role.to_owned())
        } else {
            let why = layer.reason.clone().unwrap_or_else(|| layer.status.clone());
            (theme.error(), why)
        };
        lines.push(dot(&layer.layer, label, width, style, theme));
    }
    let (style, label) = match (state.connection, state.activity) {
        (ConnectionStatus::Connected, Activity::Running) => (theme.warning(), "busy"),
        (ConnectionStatus::Connected, Activity::AwaitingUser) => {
            (theme.warning(), "waiting for you")
        }
        (ConnectionStatus::Connected, Activity::OutOfSteps) => (theme.warning(), "out of steps"),
        (ConnectionStatus::Connected, _) => (theme.success(), "idle"),
        _ => (theme.error(), "offline"),
    };
    lines.push(dot("nav", label.to_owned(), width, style, theme));
    lines
}

/// ` ● name  label`, the label cut to the width that is left.
fn dot(name: &str, label: String, width: usize, style: Style, theme: &Theme) -> Line<'static> {
    let name = format!("{name}  ");
    let room = width.saturating_sub(3 + columns(&name));
    Line::from(vec![
        Span::styled(" ● ", style),
        Span::styled(name, theme.text()),
        Span::styled(fit(&label, room), theme.muted()),
    ])
}

fn agents(state: &State, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let agent = state.agent_name();
    let style = super::sidebar::agent_style(agent, theme);
    let mut lines = vec![
        title("Agents", theme),
        Line::from(vec![
            Span::styled(" ● ", style),
            Span::styled(
                super::sidebar::agent_label(agent),
                style.add_modifier(Modifier::BOLD),
            ),
            Span::styled("  in use", theme.muted()),
        ]),
    ];
    for part in wrap(
        &format!(
            "{} · {} · {}",
            state.selected_model_id.as_deref().unwrap_or("—"),
            state.variant.as_deref().unwrap_or("off"),
            state.model_source
        ),
        width.saturating_sub(2),
    ) {
        lines.push(Line::styled(format!(" {part}"), theme.text()));
    }
    if let Some(entry) = state.agents.iter().find(|a| a.id == agent)
        && let Some(warning) = &entry.warning
    {
        for part in wrap(warning, width.saturating_sub(2)) {
            lines.push(Line::styled(format!(" {part}"), theme.warning()));
        }
    }
    for child in &state.children {
        let running = child.status == crate::tui::state::ChildStatus::Running;
        lines.push(Line::from(vec![
            Span::styled(if running { " ◐ " } else { " ○ " }, theme.subagent()),
            Span::styled(
                fit(
                    &super::sidebar::agent_label(&child.name),
                    width.saturating_sub(14),
                ),
                super::sidebar::agent_style(&child.name, theme),
            ),
            Span::styled(format!("  {}", child.status.label()), theme.muted()),
        ]));
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

/// The skills the model loaded in this conversation: the `skill` calls that succeeded, each
/// name once, in the order it was first loaded.
fn loaded_skills(state: &State) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for block in &state.conversation.blocks {
        if let BlockKind::ToolCall {
            name, args, status, ..
        } = &block.kind
            && name == "skill"
            && *status == Some(ToolStatus::Ok)
            && let Some(skill) = args.get("name").and_then(|n| n.as_str())
            && !names.iter().any(|n| n == skill)
        {
            names.push(skill.to_owned());
        }
    }
    names
}

/// The colour of an MCP server's dot: green ready, orange starting, red failed, muted off.
fn mcp_style(state: &str, theme: &Theme) -> Style {
    match state {
        "ready" => theme.success(),
        "starting" => theme.warning(),
        "failed" => theme.error(),
        _ => theme.muted(),
    }
}

/// Most servers listed; the others are counted on one line.
const MCP_ROWS: usize = 10;

/// What the conversation can call on beyond its built-in tools: the skills (the catalog
/// counts and the ones the model loaded), the service's MCP servers, and the plugins (none
/// yet). The servers' status is the service's, not this conversation's: a session keeps the
/// tools it was created with.
fn extensions(state: &State, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let mut lines = vec![title("Skills · MCP · Plugins", theme)];
    match &state.conversation.context {
        None => lines.push(row("skills", "—".into(), width, theme)),
        Some(c) => {
            lines.push(row(
                "skills listed",
                c.skills.listed.to_string(),
                width,
                theme,
            ));
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
    let loaded = loaded_skills(state);
    if !loaded.is_empty() {
        lines.push(row("model loaded", loaded.len().to_string(), width, theme));
        for part in wrap(&loaded.join(" · "), width.saturating_sub(3)) {
            lines.push(Line::styled(format!("   {part}"), theme.text()));
        }
    }
    match &state.mcp {
        None => lines.push(row("mcp", "—".into(), width, theme)),
        Some(mcp) if mcp.servers.is_empty() => {
            lines.push(Line::styled(" no MCP server", theme.muted()));
        }
        Some(mcp) => {
            let shown = if mcp.servers.len() <= MCP_ROWS {
                mcp.servers.len()
            } else {
                MCP_ROWS - 1
            };
            for server in &mcp.servers[..shown] {
                let label = match server.state.as_str() {
                    "ready" if server.tool_count == 1 => "1 tool".to_owned(),
                    "ready" => format!("{} tools", server.tool_count),
                    "failed" => server.error.clone().unwrap_or_else(|| "failed".into()),
                    other => other.to_owned(),
                };
                lines.push(dot(
                    &format!("mcp:{}", server.server_id),
                    label,
                    width,
                    mcp_style(&server.state, theme),
                    theme,
                ));
            }
            if mcp.servers.len() > shown {
                lines.push(Line::styled(
                    format!("   {} more", mcp.servers.len() - shown),
                    theme.muted(),
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

/// `text` broken into lines of at most `width` characters, at spaces.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        match lines.last_mut() {
            Some(line) if columns(line) + 1 + columns(word) <= width => {
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
    for section in [
        context(state, width, theme),
        extensions(state, width, theme),
        layers(state, width, theme),
    ] {
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
