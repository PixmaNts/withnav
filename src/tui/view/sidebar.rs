//! The tab sidebar: one row pair per open conversation, the active one tinted, with the tab keys at
//! the bottom.
use crate::tui::{
    reducer::short_id,
    state::{Activity, BlockKind, State, Tab, UiState},
    theme::Theme,
};
use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

/// Columns the sidebar takes, its right border included.
pub const WIDTH: u16 = 22;
/// Below this terminal width the sidebar hides itself.
pub const MIN_TERMINAL_WIDTH: u16 = 80;

/// Whether the sidebar is drawn on a terminal `width` columns wide.
pub fn shown(ui: &UiState, width: u16) -> bool {
    !ui.sidebar_hidden && width >= MIN_TERMINAL_WIDTH
}

/// What a tab is doing, in the words the sidebar uses.
pub fn tab_state(tab: &Tab) -> &'static str {
    match tab.activity {
        Activity::Running => "running",
        Activity::AwaitingUser => "waiting",
        _ if tab.delivery_uncertain => "error",
        _ if matches!(
            tab.conversation.blocks.last().map(|b| &b.kind),
            Some(BlockKind::Error(_))
        ) =>
        {
            "error"
        }
        _ => "idle",
    }
}

/// The name of a tab: the conversation title, else the start of its id.
pub fn tab_title(state: &State, tab: &Tab) -> String {
    match &tab.session {
        None if tab.pending_submit_text.is_some() => "starting…".into(),
        None => "new conversation".into(),
        Some(m) => state
            .sessions
            .iter()
            .find(|e| e.id == m.session_id)
            .and_then(|e| e.title.clone())
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| short_id(&m.session_id)),
    }
}

/// `text` cut to `width` columns, with `…` when cut.
fn fit(text: &str, width: usize) -> String {
    let columns = |t: &str| Span::raw(t.to_string()).width();
    if columns(text) <= width {
        return text.to_string();
    }
    let mut out = String::new();
    for c in text.chars() {
        let mut next = out.clone();
        next.push(c);
        if columns(&next) + 1 > width {
            break;
        }
        out = next;
    }
    out.push('…');
    out
}

pub fn draw(frame: &mut Frame<'_>, area: Rect, state: &State, theme: &Theme) {
    if area.width < 4 || area.height == 0 {
        return;
    }
    frame.buffer_mut().set_style(area, theme.panel());
    let inner = Rect {
        width: area.width - 1,
        ..area
    };
    for y in area.top()..area.bottom() {
        frame.buffer_mut()[(area.right() - 1, y)]
            .set_symbol("│")
            .set_style(theme.border());
    }
    let keys = ["alt+1…9 switch", "alt+w close", "alt+b hide bar"];
    let keys_height = if inner.height >= 8 {
        keys.len() as u16 + 1
    } else {
        0
    };
    let rows_bottom = inner.bottom().saturating_sub(keys_height);
    let mut y = inner.y;
    frame.render_widget(
        Paragraph::new(Span::styled(" Conversations", theme.muted())),
        Rect::new(inner.x, y, inner.width, 1),
    );
    y += 1;
    let label_width = usize::from(inner.width).saturating_sub(5);
    for (index, tab) in state.tabs.iter().enumerate() {
        let children = tab.children.len() as u16;
        if y + 2 + children > rows_bottom {
            break;
        }
        let active = index == state.active;
        let rows = Rect::new(inner.x, y, inner.width, 2 + children);
        if active {
            frame
                .buffer_mut()
                .set_style(rows, Style::default().bg(theme.selection_bg_on_panel()));
        }
        let number_style = if active { theme.title() } else { theme.muted() };
        let title_style = if active { theme.strong() } else { theme.text() };
        let mut first = vec![
            Span::raw(" "),
            Span::styled(format!("{}", index + 1), number_style),
            Span::raw(" "),
            Span::styled(fit(&tab_title(state, tab), label_width), title_style),
        ];
        if tab.unread {
            let used: usize = first.iter().map(Span::width).sum();
            let gap = usize::from(inner.width).saturating_sub(used + 2);
            first.push(Span::raw(" ".repeat(gap)));
            first.push(Span::styled("●", theme.accent_line()));
        }
        frame.render_widget(
            Paragraph::new(Line::from(first)),
            Rect { height: 1, ..rows },
        );
        let status = tab_state(tab);
        let status_style = match status {
            "running" => theme.accent(),
            "waiting" => theme.warning(),
            "error" => theme.error(),
            _ => theme.muted(),
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("   build · ", theme.muted()),
                Span::styled(status, status_style),
            ])),
            Rect::new(inner.x, y + 1, inner.width, 1),
        );
        for (n, child) in tab.children.iter().enumerate() {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    fit(&format!("   └ {}", child.name), usize::from(inner.width)),
                    theme.subagent(),
                )),
                Rect::new(inner.x, y + 2 + n as u16, inner.width, 1),
            );
        }
        if active {
            for row in rows.top()..rows.bottom() {
                frame.buffer_mut()[(rows.x, row)]
                    .set_symbol("▌")
                    .set_fg(theme.accent);
            }
        }
        y += 2 + children;
    }
    if keys_height > 0 {
        for (n, hint) in keys.iter().enumerate() {
            let (key, label) = hint.split_once(' ').unwrap_or((hint, ""));
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(format!(" {key}"), theme.text()),
                    Span::styled(format!(" {label}"), theme.muted()),
                ])),
                Rect::new(inner.x, rows_bottom + 1 + n as u16, inner.width, 1),
            );
        }
    }
}
