//! The right column: what the conversation runs with, only from what the server and git report.
//! Agents (the session's agent and the agents it started), Context (the breakdown of the
//! last snapshot and the last compaction), Goal (the Captain's `/goal`: objective, status and
//! each criterion's last check status and the agent's claims, from the ledger), Ledger (the
//! turns, the files changed and the commands that keep failing, from the server's session
//! ledger), Memory (the register items the session may be given), Skills · MCP · Plugins (the skill catalog counts, the
//! skills the model loaded, a dot per MCP server of the service), Layers (one dot each, from
//! the assignment check and the conversation's activity) and Changes (the files git reports). A
//! section with nothing reported yet shows `—`; none is made up.
use super::text::{columns, fit_end as fit, fit_start};
use crate::{
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
use withnav_client_api::{
    event::TurnEndOutcome,
    goal::GoalStatus,
    ledger::{ClaimStatus, CriterionStatus},
    tools::ToolStatus,
};
use withnav_client_state::strip_controls;

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
        let running = child.status == crate::state::ChildStatus::Running;
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
    // Only while requests carry the goal: a session without one keeps the rows it always had.
    if b.goal > 0 {
        lines.push(row("goal", format!("~{}", tokens(b.goal)), width, theme));
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

/// Most lines the goal's objective takes; a longer one ends with `…`.
const GOAL_LINES: usize = 3;

/// The Captain's session goal, from the server's ledger: the objective, its status and how many
/// success criteria it has.
fn goal(state: &State, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let mut lines = vec![title("Goal", theme)];
    let Some(goal) = state
        .conversation
        .ledger
        .as_ref()
        .and_then(|l| l.goal.as_ref())
    else {
        lines.push(none(theme));
        return lines;
    };
    let room = width.saturating_sub(2);
    let mut objective = wrap(&strip_controls(&goal.objective), room);
    if objective.len() > GOAL_LINES {
        objective.truncate(GOAL_LINES);
        let last = &mut objective[GOAL_LINES - 1];
        *last = fit(&format!("{last}…"), room);
    }
    for part in objective {
        lines.push(Line::styled(format!(" {part}"), theme.text()));
    }
    let (status, style) = match goal.status {
        GoalStatus::Active => ("active", theme.number()),
        GoalStatus::Paused => ("paused", theme.warning()),
    };
    lines.push(styled_row(
        "status",
        status.to_owned(),
        width,
        theme.text(),
        style,
    ));
    lines.push(row(
        "criteria",
        goal.criteria.len().to_string(),
        width,
        theme,
    ));
    // A check of an older revision says nothing about the criteria as they stand now.
    let check = state
        .conversation
        .ledger
        .as_ref()
        .and_then(|l| l.goal_check.as_ref())
        .filter(|c| c.revision == goal.revision);
    for (n, criterion) in goal.criteria.iter().enumerate() {
        let status = check
            .and_then(|c| c.statuses.iter().find(|s| s.index == n as u64 + 1))
            .map_or(CriterionStatus::NotChecked, |s| s.status);
        let (mark, style) = match status {
            CriterionStatus::Supported => ("✓", theme.success()),
            CriterionStatus::Contradicted => ("✗", theme.error()),
            CriterionStatus::NotChecked => ("·", theme.muted()),
        };
        lines.push(mark_line(mark, style, criterion, width, theme));
    }
    if let Some(check) = check.filter(|c| !c.claims.is_empty()) {
        lines.push(row("claims", check.claims.len().to_string(), width, theme));
        for claim in &check.claims {
            let (mark, style) = match claim.status {
                ClaimStatus::Supported => ("✓", theme.success()),
                ClaimStatus::Contradicted => ("✗", theme.error()),
                // Never read as false: no ledger line settles it.
                ClaimStatus::Unverified => ("?", theme.muted()),
            };
            lines.push(mark_line(mark, style, &claim.claim, width, theme));
        }
    }
    lines
}

/// ` ✓ text` with the text on one line, cut to the width that is left.
fn mark_line(mark: &str, style: Style, text: &str, width: usize, theme: &Theme) -> Line<'static> {
    let flat = strip_controls(text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    Line::from(vec![
        Span::styled(format!("   {mark} "), style),
        Span::styled(fit(&flat, width.saturating_sub(5)), theme.text()),
    ])
}

/// Items listed under the memory section; the others are counted on one line.
const MEMORY_ROWS: usize = 8;

/// The register items the session may be given, each a title with its scope.
fn memory(state: &State, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let mut lines = vec![title("Memory", theme)];
    let Some(items) = &state.conversation.memory else {
        lines.push(none(theme));
        return lines;
    };
    if items.is_empty() {
        lines.push(Line::styled(" no item applies", theme.muted()));
        return lines;
    }
    let shown = if items.len() <= MEMORY_ROWS {
        items.len()
    } else {
        MEMORY_ROWS - 1
    };
    for item in &items[..shown] {
        let scope = strip_controls(&item.scope);
        let room = width.saturating_sub(columns(&scope) + 3);
        lines.push(styled_row(
            &fit(&strip_controls(&item.title), room),
            scope,
            width,
            theme.text(),
            theme.muted(),
        ));
    }
    if items.len() > shown {
        lines.push(Line::styled(
            format!("   {} more", items.len() - shown),
            theme.muted(),
        ));
    }
    lines
}

/// Files listed under the ledger's count, most recently changed first.
const LEDGER_FILES: usize = 3;

/// What the session did, from the server's ledger: the turns and how the last one ended, the
/// files it changed (the last few) and the commands that keep failing (the worst one).
fn ledger(state: &State, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let mut lines = vec![title("Ledger", theme)];
    let Some(l) = &state.conversation.ledger else {
        lines.push(none(theme));
        return lines;
    };
    let turns = match &l.turns.last {
        Some(last) => format!("{} · {}", l.turns.count, outcome_word(last.outcome)),
        None => l.turns.count.to_string(),
    };
    lines.push(row("turns", turns, width, theme));
    lines.push(row(
        "files touched",
        l.files_touched.len().to_string(),
        width,
        theme,
    ));
    for file in l.files_touched.iter().take(LEDGER_FILES) {
        lines.push(Line::styled(
            format!(
                "   {}",
                fit_start(&strip_controls(&file.path), width.saturating_sub(3))
            ),
            theme.text(),
        ));
    }
    let failures = l.repeated_failures.len();
    let style = if failures == 0 {
        theme.text()
    } else {
        theme.warning()
    };
    lines.push(styled_row(
        "repeated failures",
        failures.to_string(),
        width,
        style,
        style,
    ));
    if let Some(worst) = l.repeated_failures.first() {
        lines.push(Line::styled(
            fit(
                &format!("   {}× {}", worst.count, strip_controls(&worst.command)),
                width,
            ),
            theme.warning(),
        ));
    }
    lines
}

fn outcome_word(outcome: TurnEndOutcome) -> &'static str {
    match outcome {
        TurnEndOutcome::Done => "done",
        TurnEndOutcome::Yielded => "yielded",
        TurnEndOutcome::Interrupted => "interrupted",
        TurnEndOutcome::Error => "error",
    }
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
        goal(state, width, theme),
        ledger(state, width, theme),
        memory(state, width, theme),
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

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};
    use withnav_client_api::event::{Breakdown, SkillCounts};
    use withnav_client_api::ledger::{
        ClaimCheck, ClaimStatus, CriterionCheck, CriterionStatus, FileTouched, GoalCheck,
        GoalStatus, LedgerTurns, RepeatedFailure, SessionGoal, SessionLedger, SettledTurn,
    };
    use withnav_client_api::protocol::ContextSnapshot;

    /// The column drawn alone, one string per row.
    fn rows(state: &State) -> Vec<String> {
        let (width, height) = (WIDTH, 60);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| draw(frame, frame.area(), state, &Theme::default()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    /// The rows of the section titled `title`, up to the blank row that ends it.
    fn section(rows: &[String], title: &str) -> Vec<String> {
        let start = rows
            .iter()
            .position(|r| r.ends_with(title))
            .unwrap_or_else(|| panic!("{title}: {rows:#?}"));
        rows[start..]
            .iter()
            .take_while(|r| r.trim_start_matches('│').trim() != "")
            .map(|r| r.trim_start_matches('│').trim_end().to_owned())
            .collect()
    }

    fn file(path: &str, changes: u64) -> FileTouched {
        FileTouched {
            path: path.into(),
            changes,
            last_turn: 1,
        }
    }

    #[test]
    fn the_ledger_section_shows_a_dash_before_the_server_answered() {
        let shown = section(&rows(&State::default()), "LEDGER");
        assert_eq!(shown, vec![" LEDGER", "  —"]);
    }

    #[test]
    fn the_ledger_section_reads_the_turns_the_last_files_and_the_worst_failure() {
        let mut state = State::default();
        state.conversation.ledger = Some(SessionLedger {
            turns: LedgerTurns {
                count: 12,
                last: Some(SettledTurn {
                    turn_id: ulid::Ulid(1),
                    outcome: TurnEndOutcome::Interrupted,
                }),
            },
            files_touched: vec![
                file("src/parser.rs", 3),
                file("src/lexer.rs", 1),
                file("crates/a/very/long/path/that/does/not/fit/lib.rs", 1),
                file("README.md", 1),
            ],
            repeated_failures: vec![
                RepeatedFailure {
                    command: "cargo test -p parser\x1b[31m".into(),
                    count: 3,
                    last_error: "FAILED".into(),
                },
                RepeatedFailure {
                    command: "make".into(),
                    count: 2,
                    last_error: "no rule".into(),
                },
            ],
            ..Default::default()
        });
        let shown = section(&rows(&state), "LEDGER");
        assert_eq!(
            shown,
            vec![
                " LEDGER",
                "  turns           12 · interrupted",
                "  files touched                  4",
                "    src/parser.rs",
                "    src/lexer.rs",
                "    …path/that/does/not/fit/lib.rs",
                "  repeated failures              2",
                "    3× cargo test -p parser",
            ]
        );
    }

    #[test]
    fn the_goal_section_shows_a_dash_without_a_goal_and_comes_before_the_ledger() {
        let shown = rows(&State::default());
        assert_eq!(section(&shown, "GOAL"), vec![" GOAL", "  —"]);
        let mut state = State::default();
        state.conversation.ledger = Some(SessionLedger::default());
        let shown = rows(&state);
        assert_eq!(section(&shown, "GOAL"), vec![" GOAL", "  —"]);
        let goal = shown.iter().position(|r| r.ends_with("GOAL")).unwrap();
        let ledger = shown.iter().position(|r| r.ends_with("LEDGER")).unwrap();
        assert!(goal < ledger);
    }

    #[test]
    fn the_goal_section_wraps_the_objective_to_three_lines_then_status_and_criteria() {
        let mut state = State::default();
        state.conversation.ledger = Some(SessionLedger {
            goal: Some(SessionGoal {
                revision: 4,
                objective: "ship the parser rewrite with the new error recovery \x1b[2Jand the \
                            docs for every flag the command line takes today"
                    .into(),
                criteria: vec!["tests pass".into(), "docs".into()],
                status: GoalStatus::Paused,
            }),
            ..Default::default()
        });
        assert_eq!(
            section(&rows(&state), "GOAL"),
            vec![
                " GOAL",
                "  ship the parser rewrite with",
                "  the new error recovery and the",
                "  docs for every flag the comman…",
                "  status                    paused",
                "  criteria                       2",
                "    · tests pass",
                "    · docs",
            ]
        );
        state.conversation.ledger.as_mut().unwrap().goal = Some(SessionGoal {
            revision: 1,
            objective: "short".into(),
            criteria: vec![],
            status: GoalStatus::Active,
        });
        assert_eq!(
            section(&rows(&state), "GOAL"),
            vec![
                " GOAL",
                "  short",
                "  status                    active",
                "  criteria                       0",
            ]
        );
    }

    fn checked_goal(revision: u64, check_revision: u64) -> SessionLedger {
        SessionLedger {
            goal: Some(SessionGoal {
                revision,
                objective: "ship".into(),
                criteria: vec![
                    "clippy is clean".into(),
                    "Windows CI passes".into(),
                    "docs".into(),
                ],
                status: GoalStatus::Active,
            }),
            goal_check: Some(GoalCheck {
                revision: check_revision,
                turn_id: "T".into(),
                statuses: vec![
                    CriterionCheck {
                        index: 1,
                        status: CriterionStatus::Supported,
                        evidence: "cargo clippy exited 0".into(),
                    },
                    CriterionCheck {
                        index: 2,
                        status: CriterionStatus::Contradicted,
                        evidence: "cargo test exited 101".into(),
                    },
                ],
                claims: vec![
                    ClaimCheck {
                        claim: "tests pass".into(),
                        status: ClaimStatus::Contradicted,
                        evidence: String::new(),
                    },
                    ClaimCheck {
                        claim: "committed".into(),
                        status: ClaimStatus::Unverified,
                        evidence: String::new(),
                    },
                ],
                note_sent: false,
                note: None,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn each_criterion_shows_the_status_of_its_last_check_and_the_claims_follow() {
        let mut state = State::default();
        state.conversation.ledger = Some(checked_goal(3, 3));
        assert_eq!(
            section(&rows(&state), "GOAL"),
            vec![
                " GOAL",
                "  ship",
                "  status                    active",
                "  criteria                       3",
                "    ✓ clippy is clean",
                "    ✗ Windows CI passes",
                "    · docs",
                "  claims                         2",
                "    ✗ tests pass",
                "    ? committed",
            ]
        );
    }

    #[test]
    fn a_check_of_another_revision_leaves_every_criterion_unchecked_and_hides_its_claims() {
        let mut state = State::default();
        state.conversation.ledger = Some(checked_goal(4, 3));
        let shown = section(&rows(&state), "GOAL");
        assert_eq!(
            &shown[4..],
            [
                "    · clippy is clean",
                "    · Windows CI passes",
                "    · docs"
            ]
        );
    }

    #[test]
    fn the_memory_section_shows_a_dash_before_the_server_answered_and_follows_the_ledger() {
        let shown = rows(&State::default());
        assert_eq!(section(&shown, "MEMORY"), vec![" MEMORY", "  —"]);
        let ledger = shown.iter().position(|r| r.ends_with("LEDGER")).unwrap();
        let memory = shown.iter().position(|r| r.ends_with("MEMORY")).unwrap();
        let extensions = shown
            .iter()
            .position(|r| r.ends_with("SKILLS · MCP · PLUGINS"))
            .unwrap();
        assert!(ledger < memory && memory < extensions);
        let mut state = State::default();
        state.conversation.memory = Some(vec![]);
        assert_eq!(
            section(&rows(&state), "MEMORY"),
            vec![" MEMORY", "  no item applies"]
        );
    }

    fn entry(title: &str, scope: &str) -> withnav_client_state::MemoryEntry {
        withnav_client_state::MemoryEntry {
            id: format!("id-{title}"),
            title: title.into(),
            scope: scope.into(),
        }
    }

    #[test]
    fn the_memory_section_lists_titles_with_their_scope_and_counts_what_does_not_fit() {
        let mut state = State::default();
        let mut items = vec![
            entry("Conventional commits", "project"),
            entry("Reply in French\x1b[31m", "global"),
            entry(
                "A title that is far too long to fit beside its scope",
                "group",
            ),
        ];
        items.extend((4..=10).map(|n| entry(&format!("item {n}"), "session")));
        state.conversation.memory = Some(items);
        let shown = section(&rows(&state), "MEMORY");
        assert_eq!(shown.len(), 1 + 7 + 1, "{shown:#?}");
        assert_eq!(shown[1], "  Conventional commits     project");
        assert_eq!(shown[2], "  Reply in French           global");
        assert_eq!(shown[3], "  A title that is far too …  group");
        assert_eq!(shown[7], "  item 7                   session");
        assert_eq!(shown[8], "    3 more");
    }

    #[test]
    fn the_context_section_counts_the_goal_when_requests_carry_it() {
        let mut state = State::default();
        state.conversation.context = Some(ContextSnapshot {
            model: "mock/m".into(),
            gateway: "mock".into(),
            upstream_id: "m".into(),
            variant: None,
            window: 200_000,
            window_source: "models.json".into(),
            estimate_total: 1_300,
            breakdown: Breakdown {
                system: 1_000,
                goal: 300,
                ..Default::default()
            },
            output_reserve: 8_192,
            margin: 10_000,
            input_budget: 181_808,
            percent: 1,
            last_usage: None,
            skills: SkillCounts::default(),
            rule_files: 0,
            incarnation: ulid::Ulid(7),
            revision: 1,
        });
        let shown = section(&rows(&state), "CONTEXT");
        assert!(
            shown.contains(&"  goal                        ~300".to_string()),
            "{shown:#?}"
        );
    }

    #[test]
    fn the_context_section_still_reads_the_snapshot() {
        let mut state = State::default();
        state.conversation.context = Some(ContextSnapshot {
            model: "mock/m".into(),
            gateway: "mock".into(),
            upstream_id: "m".into(),
            variant: None,
            window: 200_000,
            window_source: "models.json".into(),
            estimate_total: 41_200,
            breakdown: Breakdown {
                system: 3_100,
                rules: 1_900,
                skills_catalog: 2_400,
                tools: 6_800,
                messages: 27_000,
                goal: 0,
            },
            output_reserve: 8_192,
            margin: 10_000,
            input_budget: 181_808,
            percent: 21,
            last_usage: None,
            skills: SkillCounts::default(),
            rule_files: 2,
            incarnation: ulid::Ulid(7),
            revision: 1,
        });
        let shown = section(&rows(&state), "CONTEXT");
        assert_eq!(
            shown,
            vec![
                " CONTEXT",
                "  system                       ~3k",
                "  rules · 2 files              ~2k",
                "  skills                       ~2k",
                "  tools                        ~7k",
                "  messages                    ~27k",
                "  next request          ~41k · 21%",
                "  input budget                182k",
            ]
        );
    }
}
