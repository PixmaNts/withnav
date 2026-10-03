use crate::tui::{
    state::{Activity, ConnectionStatus, State, UiState},
    theme::Theme,
};
use ratatui::{
    layout::Rect,
    style::Modifier,
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

/// A token count for the gauge: `842`, `42k`, `1M`, `1.5M`.
pub fn gauge_number(n: u64) -> String {
    crate::tui::reducer::tokens(n)
}

/// What the status line shows for the context: used, window, percent.
/// The server's estimate of the next request when known (`~42k / 1M · 4%`);
/// until then the provider's last prompt count over the catalog window.
fn context_gauge(state: &State) -> (String, String, Option<String>) {
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
    let (context, window, percent) = context_gauge(state);
    let percent_text = percent.unwrap_or_default();
    let session_tokens = number(state.conversation.session_tokens);
    let fixed = format!("   {connection}   {activity}{warning}")
        .chars()
        .count();
    let available = usize::from(area.width).saturating_sub(fixed);
    let agent = state.agent_name();
    let agent_style = super::sidebar::agent_style(agent, theme);
    // `▌build  alias · model · variant · ~42k / 1M · 4%`: the agent and the alias always; then the
    // gauge, the variant, the model (shortened first) and the session count as room allows.
    let lead = vec![
        Span::styled("▌", agent_style),
        Span::styled(agent.to_string(), agent_style.add_modifier(Modifier::BOLD)),
        Span::styled(format!("  {alias}"), theme.title()),
    ];
    let variant = state.variant.as_deref().map(|v| {
        vec![
            Span::styled(" · ", theme.muted()),
            Span::styled(v.to_string(), theme.text()),
        ]
    });
    let gauge = (area.width >= 52).then(|| {
        vec![
            Span::styled(" · ", theme.muted()),
            Span::styled(context, theme.number()),
            Span::styled(" / ", theme.muted()),
            Span::styled(window, theme.text()),
            Span::styled(percent_text, theme.muted()),
        ]
    });
    let session = (area.width >= 90).then(|| {
        vec![
            Span::styled("   session ", theme.muted()),
            Span::styled(session_tokens, theme.number()),
            Span::styled(" tok", theme.muted()),
        ]
    });
    let width = |spans: &[Span<'_>]| {
        spans
            .iter()
            .map(|p| p.content.chars().count())
            .sum::<usize>()
    };
    let mut used = width(&lead);
    let mut fits = |spans: &Option<Vec<Span<'static>>>| match spans {
        Some(spans) if used + width(spans) <= available => {
            used += width(spans);
            true
        }
        _ => false,
    };
    let gauge_fits = fits(&gauge);
    let variant_fits = fits(&variant);
    let session_fits = fits(&session);
    // The model takes what is left, cut with `…`, and is dropped below five columns.
    let room = available.saturating_sub(used);
    let model_label = format!(" · {model}");
    let model = if room < 5 {
        None
    } else if model_label.chars().count() > room {
        Some(format!(
            " · {}…",
            model
                .chars()
                .take(room.saturating_sub(4))
                .collect::<String>()
        ))
    } else {
        Some(model_label)
    };
    let mut parts = lead;
    if let Some(model) = model {
        parts.push(Span::styled(model, theme.muted()));
    }
    for (shown, spans) in [
        (variant_fits, variant),
        (gauge_fits, gauge),
        (session_fits, session),
    ] {
        if shown {
            parts.extend(spans.unwrap_or_default());
        }
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
