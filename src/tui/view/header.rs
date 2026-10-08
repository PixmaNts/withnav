//! The header row: `◆ withnav`, the workspace, the git branch and its changes on the left; the
//! agent and the model in use (model_id · model · variant · context gauge) on the right. What does
//! not fit is dropped, least useful first.
use super::{
    status::{context_gauge, shorten},
    text::{columns, fit_end},
};
use crate::tui::{state::State, theme::Theme};
use ratatui::{
    layout::Rect,
    style::Modifier,
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

fn width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(Span::width).sum()
}

pub fn draw(frame: &mut Frame<'_>, area: Rect, state: &State, theme: &Theme) {
    if area.height == 0 {
        return;
    }
    frame.buffer_mut().set_style(area, theme.panel());
    if state.bypass {
        let marker_width = 11.min(area.width);
        draw_content(
            frame,
            Rect::new(
                area.x + marker_width,
                area.y,
                area.width - marker_width,
                area.height,
            ),
            state,
            theme,
        );
        frame.render_widget(
            Paragraph::new(Span::styled(
                " ⚠ bypass ",
                theme.warning().add_modifier(Modifier::BOLD),
            )),
            Rect::new(area.x, area.y, marker_width, 1),
        );
        return;
    }
    draw_content(frame, area, state, theme);
}

fn draw_content(frame: &mut Frame<'_>, area: Rect, state: &State, theme: &Theme) {
    if state.archive() {
        let reason = state
            .session
            .as_ref()
            .and_then(|m| m.resume_disabled_reason.as_deref())
            .unwrap_or("read-only history");
        frame.render_widget(
            Paragraph::new(Line::styled(
                fit_end(&format!(" ◆ archive · {reason}"), usize::from(area.width)),
                theme.muted(),
            )),
            area,
        );
        return;
    }
    let agent = state.agent_name();
    let agent_style = super::sidebar::agent_style(agent, theme);
    if let Some(parent) = &state.parent_session_id {
        let parent_name = state
            .tab_of_session(parent)
            .map(|i| super::sidebar::tab_title(state, &state.tabs[i]))
            .unwrap_or_else(|| crate::tui::reducer::short_id(parent));
        let width = usize::from(area.width);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    format!(
                        " ◆ subagent of {}",
                        fit_end(&parent_name, width.saturating_sub(35).min(24))
                    ),
                    theme.title(),
                ),
                Span::styled(
                    format!("  ▌{}", fit_end(&super::sidebar::agent_label(agent), 14)),
                    theme.subagent(),
                ),
                Span::styled(
                    format!(
                        " · {}",
                        fit_end(
                            state
                                .selected_model_name
                                .as_deref()
                                .or(state.selected_model_id.as_deref())
                                .unwrap_or("—"),
                            width.saturating_sub(42)
                        )
                    ),
                    theme.muted(),
                ),
            ])),
            area,
        );
        return;
    }
    let model_id = state.selected_model_id.as_deref().unwrap_or("—");
    let model = if state.model_source == "override" || state.variant_source == "override" {
        "override"
    } else {
        "assigned"
    };
    let (used, window, percent) = context_gauge(state);

    // Left: the logo always; the branch, then the path, as room allows.
    let logo = vec![
        Span::styled(" ◆", ratatui::style::Style::default().fg(theme.nav)),
        Span::styled(" withnav", theme.strong()),
    ];
    let branch = state.git.as_ref().map(|git| {
        let name = git.branch.as_deref().unwrap_or("detached");
        let mut spans = vec![
            Span::raw("  "),
            Span::styled(fit_end(name, 32), theme.code_ref()),
        ];
        if git.dirty > 0 {
            spans.push(Span::styled(format!(" ●{}", git.dirty), theme.warning()));
        }
        spans
    });
    let path = state
        .session
        .as_ref()
        .map(|s| s.workspace.as_str())
        .unwrap_or(state.workspace.as_str());

    // Right: the agent and the model_id always (each cut to a share of the row); then the gauge,
    // the variant and the model.
    let total = usize::from(area.width);
    let lead = vec![
        Span::styled("▌", agent_style),
        Span::styled(
            fit_end(
                &format!("{} /{agent}", super::sidebar::agent_label(agent)),
                (total / 5).max(8),
            ),
            agent_style.add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  {}", fit_end(model_id, (total / 5).max(4))),
            theme.title(),
        ),
    ];
    let gauge = vec![
        Span::styled(" · ", theme.muted()),
        Span::styled(used, theme.number()),
        Span::styled(" / ", theme.muted()),
        Span::styled(window, theme.text()),
        Span::styled(percent.unwrap_or_default(), theme.muted()),
    ];
    let variant = state.variant.as_deref().map(|v| {
        vec![
            Span::styled(" · ", theme.muted()),
            Span::styled(fit_end(v, 16), theme.text()),
        ]
    });

    // A switch the server accepted for after this turn, beside the model still in use.
    let pending = state.pending_model().map(|model_id| {
        let target = format!("→ {}", fit_end(model_id, (total / 6).max(4)));
        vec![
            Span::raw("  "),
            Span::styled(target, theme.warning()),
            Span::styled(" (after this turn)", theme.muted()),
        ]
    });

    let mut room = total.saturating_sub(width(&logo) + width(&lead) + 3);
    let mut take = |spans: Option<Vec<Span<'static>>>| match spans {
        Some(spans) if width(&spans) <= room => {
            room -= width(&spans);
            Some(spans)
        }
        _ => None,
    };
    let pending = take(pending);
    let gauge = take(Some(gauge));
    let branch = take(branch);
    let variant = take(variant);
    // The model takes up to 30 columns, then the path what is left (cut from its start).
    let model = (room >= 8).then(|| {
        let shown = fit_end(model, room.min(33) - 3);
        room = room.saturating_sub(columns(&shown) + 3);
        vec![
            Span::styled(" · ", theme.muted()),
            Span::styled(shown, theme.muted()),
        ]
    });
    let path = (room >= 8).then(|| {
        let shown = shorten(path, room - 2);
        room = room.saturating_sub(columns(&shown) + 2);
        vec![Span::raw("  "), Span::styled(shown, theme.muted())]
    });

    let mut left = logo;
    left.extend(path.unwrap_or_default());
    left.extend(branch.unwrap_or_default());
    let mut right = lead;
    right.extend(model.unwrap_or_default());
    right.extend(variant.unwrap_or_default());
    right.extend(gauge.unwrap_or_default());
    right.extend(pending.unwrap_or_default());
    right.push(Span::raw(" "));
    let gap = total.saturating_sub(width(&left) + width(&right));
    left.push(Span::raw(" ".repeat(gap)));
    left.extend(right);
    frame.render_widget(Paragraph::new(Line::from(left)), area);
}
