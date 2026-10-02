use crate::tui::{
    state::{Activity, ConnectionStatus, State, UiState},
    theme::Theme,
};
use ratatui::{
    layout::Rect,
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

pub fn number(n: Option<u64>) -> String {
    match n {
        None => "—".into(),
        Some(n) if n < 1000 => n.to_string(),
        Some(n) if n.is_multiple_of(1000) => format!("{}k", n / 1000),
        Some(n) => format!("{:.1}k", n as f64 / 1000.0),
    }
}

pub fn draw(frame: &mut Frame<'_>, area: Rect, state: &State, ui: &UiState, theme: &Theme) {
    if area.height == 0 {
        return;
    }
    let alias = state.selected_model_alias.as_deref().unwrap_or("—");
    let model = state.selected_model_name.as_deref().unwrap_or("—");
    let model = if area.width < 90 {
        model.rsplit('/').next().unwrap_or(model)
    } else {
        model
    };
    let connection = match state.connection {
        ConnectionStatus::Connecting => "connecting",
        ConnectionStatus::Reconnecting => "reconnecting",
        ConnectionStatus::Disconnected => "offline",
        ConnectionStatus::Connected => "",
    };
    let activity = match state.activity {
        Activity::Running => {
            if ui.tick.is_multiple_of(2) {
                "◐ running"
            } else {
                "◓ running"
            }
        }
        Activity::AwaitingUser => "awaiting answer",
        Activity::Stopped => "stopped",
        Activity::Idle => "ready",
    };
    let warning = if state.delivery_uncertain {
        "  delivery uncertain; check conversation"
    } else {
        ""
    };
    let (context, window) = (
        number(state.conversation.context_tokens),
        number(state.context_window),
    );
    let metrics = format!("   ctx {context} / {window}");
    let session_tokens = number(state.conversation.session_tokens);
    let session = format!("   session {session_tokens} tok");
    let fixed = format!("   {connection}   {activity}{warning}")
        .chars()
        .count();
    let available = usize::from(area.width).saturating_sub(fixed);
    let mut parts = vec![Span::styled(format!(" {alias}"), theme.title())];
    let model_label = format!(" · {model}");
    let remaining = available.saturating_sub(alias.chars().count() + 1);
    if remaining >= 5 {
        let model = if model_label.chars().count() > remaining {
            format!(
                " · {}…",
                model
                    .chars()
                    .take(remaining.saturating_sub(4))
                    .collect::<String>()
            )
        } else {
            model_label
        };
        parts.push(Span::styled(model, theme.muted()));
    }
    let mut used: usize = parts.iter().map(|part| part.content.chars().count()).sum();
    if area.width >= 52 && used + metrics.chars().count() <= available {
        used += metrics.chars().count();
        parts.extend([
            Span::styled("   ctx ", theme.muted()),
            Span::styled(context, theme.number()),
            Span::styled(" / ", theme.muted()),
            Span::styled(window, theme.text()),
        ]);
    }
    if area.width >= 90 && used + session.chars().count() <= available {
        parts.extend([
            Span::styled("   session ", theme.muted()),
            Span::styled(session_tokens, theme.number()),
            Span::styled(" tok", theme.muted()),
        ]);
    }
    if !connection.is_empty() {
        parts.push(Span::styled(format!("   {connection}"), theme.warning()));
    }
    parts.push(Span::styled(
        format!("   {activity}"),
        if state.activity == Activity::Running {
            theme.accent()
        } else if state.activity == Activity::Idle {
            theme.success()
        } else if state.activity == Activity::AwaitingUser {
            theme.warning()
        } else {
            theme.muted()
        },
    ));
    if state.delivery_uncertain {
        parts.push(Span::styled(
            "   delivery uncertain; check conversation",
            theme.warning(),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(parts)), area);
}
