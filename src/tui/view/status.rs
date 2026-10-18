use crate::tui::{
    state::{Activity, ConnectionStatus, State, UiState},
    theme::Theme,
};
use ratatui::{
    Frame,
    layout::Rect,
    text::{Line, Span},
    widgets::Paragraph,
};

pub fn number(n: Option<u64>) -> String {
    match n {
        None => "—".into(),
        Some(n) if n < 1000 => n.to_string(),
        Some(n) if n >= 999_950 => crate::tui::reducer::tokens(n),
        Some(n) if n.is_multiple_of(1000) => format!("{}k", n / 1000),
        Some(n) => format!("{:.1}k", n as f64 / 1000.0),
    }
}

/// A token count for the gauge: `842`, `42k`, `1M`, `1.5M`.
pub fn gauge_number(n: u64) -> String {
    crate::tui::reducer::tokens(n)
}

/// The context gauge: used, window and percent. The server's estimate of the next request when
/// known (`~42k / 1M · 4%`); until then the provider's last prompt count over the catalog window.
pub fn context_gauge(state: &State) -> (String, String, Option<String>) {
    match &state.conversation.context {
        Some(c) => (
            format!("~{}", gauge_number(c.estimate_total)),
            gauge_number(c.window),
            Some(format!(" · {}%", c.percent)),
        ),
        None => (
            number(state.conversation.context_tokens),
            number(state.context_window),
            None,
        ),
    }
}

/// `path` with the home folder as `~`, cut from its start to `width` columns.
pub fn shorten(path: &str, width: usize) -> String {
    let path = path.replace(home().as_deref().unwrap_or("\0"), "~");
    super::text::fit_start(&path, width)
}

/// The home folder shown as `~`; a home that is not valid Unicode is not shortened.
fn home() -> Option<String> {
    crate::paths::home_dir().and_then(|home| home.into_os_string().into_string().ok())
}

/// The MCP servers in a few columns: `mcp 2/3` (ready over enabled), in red when one failed,
/// orange while one starts, green when all are ready. `None` before the first status or when no
/// server is enabled.
fn mcp_summary(state: &State, theme: &Theme) -> Option<[Span<'static>; 2]> {
    let servers = &state.mcp.as_ref()?.servers;
    let enabled = servers.iter().filter(|s| s.state != "disabled").count();
    if enabled == 0 {
        return None;
    }
    let ready = servers.iter().filter(|s| s.state == "ready").count();
    let style = if servers.iter().any(|s| s.state == "failed") {
        theme.error()
    } else if servers.iter().any(|s| s.state == "starting") {
        theme.warning()
    } else {
        theme.success()
    };
    Some([
        Span::styled("mcp ", theme.muted()),
        Span::styled(format!("{ready}/{enabled}   "), style),
    ])
}

/// The status line: what the conversation is doing (connection, activity, a notice), then what
/// the session used and the MCP servers' summary. The model, the agent and the context gauge are
/// in the header.
pub fn draw(frame: &mut Frame<'_>, area: Rect, state: &State, ui: &UiState, theme: &Theme) {
    if area.height == 0 {
        return;
    }
    let connection = match state.connection {
        ConnectionStatus::Connecting => "connecting",
        ConnectionStatus::Reconnecting => "reconnecting",
        ConnectionStatus::Disconnected => "offline",
        ConnectionStatus::Connected => "",
    };
    let activity = match state.activity {
        _ if state.archive() => "archive · read-only history",
        Activity::Running => {
            if ui.tick.is_multiple_of(2) {
                "◐ running"
            } else {
                "◓ running"
            }
        }
        Activity::AwaitingUser if state.open_permission().is_some() => "permission needed",
        Activity::AwaitingUser if state.watchdog_paused => "watchdog paused · alt+c continue",
        Activity::AwaitingUser => "awaiting answer",
        Activity::OutOfSteps if state.read_only() => "out of steps · alt+c continue",
        Activity::OutOfSteps => "out of steps · send a message to continue",
        Activity::Stopped => "stopped",
        Activity::Idle => "ready",
    };
    let mut parts = vec![Span::styled(
        format!(" {activity}"),
        match state.activity {
            Activity::Running => theme.accent(),
            Activity::Idle => theme.success(),
            Activity::AwaitingUser | Activity::OutOfSteps => theme.warning(),
            Activity::Stopped => theme.muted(),
        },
    )];
    if !connection.is_empty() {
        parts.push(Span::styled(format!("   {connection}"), theme.warning()));
    }
    if state.delivery_uncertain {
        parts.push(Span::styled(
            "   delivery uncertain; check conversation",
            theme.warning(),
        ));
    }
    if let Some(notice) = state.notice.as_deref() {
        parts.push(Span::styled(format!("   {notice}"), theme.warning()));
    }
    // Right: the MCP summary and what the session used, when they fit; on a narrow line the
    // summary gives way first (the right panel lists the servers).
    let session = [
        Span::styled("session ", theme.muted()),
        Span::styled(number(state.conversation.session_tokens), theme.number()),
        Span::styled(" tok ", theme.muted()),
    ];
    let used: usize = parts.iter().map(Span::width).sum();
    let total = usize::from(area.width);
    let mut right: Vec<Span> = mcp_summary(state, theme).into_iter().flatten().collect();
    right.extend(session);
    let fits = |right: &[Span]| used + right.iter().map(Span::width).sum::<usize>() + 2 <= total;
    if !fits(&right) {
        right.drain(..right.len() - 3);
    }
    if fits(&right) {
        let wanted: usize = right.iter().map(Span::width).sum();
        parts.push(Span::raw(" ".repeat(total - used - wanted)));
        parts.extend(right);
    }
    frame.render_widget(Paragraph::new(Line::from(parts)), area);
}

#[cfg(test)]
mod tests {
    use super::number;

    #[test]
    fn a_million_is_never_shown_as_a_thousand_k_in_the_status_line() {
        assert_eq!(number(Some(1_000_000)), "1M");
        assert_eq!(number(Some(999_950)), "1M");
        assert_eq!(number(Some(999_900)), "999.9k");
        assert_eq!(number(Some(200_000)), "200k");
    }
}
