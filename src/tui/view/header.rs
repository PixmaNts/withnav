//! The header row: `◆ withnav`, the workspace, the git branch and its changes on the left; the
//! agent and the model in use (model_id · model · variant · context gauge) on the right. What does
//! not fit is dropped, least useful first.
use super::{
    status::{context_gauge, shorten},
    text::{columns, fit_end},
};
use crate::tui::{state::State, theme::Theme};
use ratatui::{
    Frame,
    layout::Rect,
    style::Modifier,
    text::{Line, Span},
    widgets::Paragraph,
};

fn width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(Span::width).sum()
}

/// `spans` cut at `total` columns: a row wider than the terminal would spill out of the header.
fn clip(spans: Vec<Span<'static>>, total: usize) -> Line<'static> {
    let mut room = total;
    let mut kept = Vec::new();
    for span in spans {
        if span.width() <= room {
            room -= span.width();
            kept.push(span);
        } else {
            kept.push(Span::styled(fit_end(&span.content, room), span.style));
            break;
        }
    }
    Line::from(kept)
}

pub fn draw(frame: &mut Frame<'_>, area: Rect, state: &State, theme: &Theme) {
    if area.height == 0 {
        return;
    }
    frame.buffer_mut().set_style(area, theme.panel());
    {
        let yolo = state.session.as_ref().is_some_and(|m| m.yolo);
        let unavailable = state
            .session
            .as_ref()
            .and_then(|m| m.permissions_unavailable.as_ref());
        let marked = yolo || state.bypass || unavailable.is_some();
        let marker = if yolo {
            " ⚠ YOLO   "
        } else if unavailable.is_some() {
            " ⚠ Permission rules unavailable "
        } else if state.bypass {
            " ⚠ bypass "
        } else {
            " 🛡 safe "
        };
        let marker_width = (if unavailable.is_some() && !yolo {
            33
        } else if marked {
            11
        } else if area.width >= 80 {
            10
        } else {
            0
        })
        .min(area.width.saturating_sub(13));
        frame.render_widget(
            Paragraph::new(line(state, theme, usize::from(area.width - marker_width))),
            Rect::new(
                area.x + marker_width,
                area.y,
                area.width - marker_width,
                area.height,
            ),
        );
        frame.render_widget(
            Paragraph::new(Span::styled(
                marker,
                if marked {
                    theme.warning().add_modifier(Modifier::BOLD)
                } else {
                    theme.muted()
                },
            )),
            Rect::new(area.x, area.y, marker_width, 1),
        );
    }
}

/// The header row for `total` columns: never wider than that.
fn line(state: &State, theme: &Theme, total: usize) -> Line<'static> {
    if state.archive() {
        let reason = state
            .session
            .as_ref()
            .and_then(|m| m.resume_disabled_reason.as_deref())
            .unwrap_or("read-only history");
        return Line::styled(
            fit_end(&format!(" ◆ archive · {reason}"), total),
            theme.muted(),
        );
    }
    if let Some(error) = state
        .session
        .as_ref()
        .and_then(|m| m.permissions_unavailable.as_deref())
        .filter(|_| !state.session.as_ref().is_some_and(|m| m.yolo))
    {
        return Line::styled(fit_end(error, total), theme.warning());
    }
    let agent = state.agent_name();
    let agent_style = super::sidebar::agent_style(agent, theme);
    if let Some(parent) = &state.parent_session_id {
        let parent_name = state
            .tab_of_session(parent)
            .map(|i| super::sidebar::tab_title(state, &state.tabs[i]))
            .unwrap_or_else(|| crate::tui::reducer::short_id(parent));
        let width = total;
        let parent_name = fit_end(&parent_name, width.saturating_sub(35).min(24));
        let title = if parent_name.is_empty() {
            " ◆ subagent".to_string()
        } else {
            format!(" ◆ subagent of {parent_name}")
        };
        // The agent stays whole down to the narrowest view; the title gives way first.
        let label = fit_end(
            &super::sidebar::agent_label(agent),
            14.min(width.saturating_sub(3)),
        );
        let mut spans = vec![
            Span::styled(title, theme.title()),
            Span::styled(format!("  ▌{label}"), theme.subagent()),
        ];
        let model = fit_end(
            state
                .selected_model_name
                .as_deref()
                .or(state.selected_model_id.as_deref())
                .unwrap_or("—"),
            width
                .saturating_sub(42)
                .min(width.saturating_sub(self::width(&spans) + 3)),
        );
        if !model.is_empty() {
            spans.push(Span::styled(format!(" · {model}"), theme.muted()));
        }
        if self::width(&spans) > width {
            spans[0] = Span::styled(" ◆", theme.title());
        }
        if self::width(&spans) > width {
            spans.remove(0);
        }
        return clip(spans, total);
    }
    let model_id = state.selected_model_id.as_deref().unwrap_or("—");
    let model = if state.model_source == "override" || state.variant_source == "override" {
        "override"
    } else {
        "assigned"
    };
    let (used, window, percent) = context_gauge(state);

    // The agent first: build and plan act differently, so the mode stays visible at every width.
    // A narrow header drops the crew name before the agent.
    let agent_label = if total < 80 {
        format!("{} {agent}", super::sidebar::identity(agent).0)
    } else {
        format!("{} /{agent}", super::sidebar::agent_label(agent))
    };
    // `▌` and the trailing space around it.
    let agent_label = fit_end(
        &agent_label,
        (total / 5).max(8).min(total.saturating_sub(2)),
    );
    let agent_width = columns(&agent_label) + 2;
    // Then the model_id at 4 columns or more, then the logo: `◆ withnav`, `◆`, or nothing.
    let model_min = 2 + columns(model_id).min(4);
    let glyph = Span::styled(" ◆", ratatui::style::Style::default().fg(theme.nav));
    let logo = if total >= 10 + agent_width + model_min {
        vec![glyph, Span::styled(" withnav", theme.strong())]
    } else if total >= 2 + agent_width {
        vec![glyph]
    } else {
        vec![]
    };
    let model_room = total.saturating_sub(width(&logo) + agent_width + 2);
    let model_id = if model_room >= columns(model_id).min(4) {
        fit_end(model_id, (total / 5).max(4).min(model_room))
    } else {
        String::new()
    };

    // Left: the logo; the branch, then the path, as room allows.
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

    // Right: the agent and the model_id (each cut to a share of the row); then the gauge, the
    // variant and the model.
    let mut lead = vec![
        Span::styled("▌", agent_style),
        Span::styled(agent_label, agent_style.add_modifier(Modifier::BOLD)),
    ];
    if !model_id.is_empty() {
        lead.push(Span::styled(format!("  {model_id}"), theme.title()));
    }
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
    clip(left, total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::state::{ModelSwitch, Tab};
    use ratatui::{Terminal, backend::TestBackend};

    /// A header with everything to show: a long workspace, a dirty branch, a variant, a gauge and
    /// a switch waiting for the turn to end.
    fn busy() -> State {
        let tab = Tab {
            selected_model_id: Some("omniroute/combo/deepseek-v4-flash".into()),
            context_window: Some(200_000),
            variant: Some("high".into()),
            model_switch: Some(ModelSwitch {
                agent: "build".into(),
                model_id: "cc/claude-opus-5-5".into(),
                token: 1,
                operation_id: Some("op".into()),
                deferred: true,
            }),
            git: Some(crate::workspace::git::GitStatus {
                branch: Some("fix/header-overflows-on-narrow-terminals".into()),
                dirty: 12,
                files: vec![],
            }),
            ..Tab::default()
        };
        State {
            tabs: vec![tab],
            workspace: "/home/pixma/dev/a/rather/long/workspace/path".into(),
            ..State::default()
        }
    }

    fn row(state: &State, width: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, 1)).unwrap();
        terminal
            .draw(|frame| draw(frame, frame.area(), state, &Theme::default()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..width).map(|x| buffer[(x, 0)].symbol()).collect()
    }

    #[test]
    fn the_header_never_outgrows_the_row_and_keeps_the_agent() {
        let mut child = busy();
        child.parent_session_id = Some("01JPARENTSESSION".into());
        let mut bypass = busy();
        bypass.bypass = true;
        for state in [busy(), child, bypass] {
            for total in 0..=140 {
                let line = line(&state, &Theme::default(), total);
                assert!(line.width() <= total, "{total}: {line:?}");
                if total >= 12 {
                    assert!(line.to_string().contains('▌'), "{total}: {line:?}");
                }
            }
        }
    }

    #[test]
    fn a_narrow_header_drops_the_least_useful_items_first() {
        let state = busy();
        // 80 columns: the safe-mode marker takes its place on the left; the switch waiting for
        // the turn still outranks the crew name, variant, gauge and path.
        assert_eq!(
            row(&state, 80),
            " 🛡 safe    ◆ withnav  ▌◎ build  omniroute/com…  → cc/claude-… (after this turn) "
        );
        // 40 columns: the crew name, the switch and the gauge go; the agent and model_id stay.
        assert_eq!(row(&state, 40), " ◆ withnav    ▌◎ build  omnirou… · high ");
        // 24 columns, the narrowest the view draws a header at: the logo shrinks to its glyph.
        assert_eq!(row(&state, 24), " ◆       ▌◎ build  omn… ");
        // 13 columns (24 with the bypass marker): the model_id goes, the agent stays.
        let mut bypass = busy();
        bypass.bypass = true;
        assert_eq!(row(&bypass, 24), " ⚠ bypass   ◆  ▌◎ build ");
    }
}
