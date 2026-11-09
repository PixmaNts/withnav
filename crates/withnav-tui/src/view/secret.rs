//! The secret prompt, drawn over the conversation: masked value, availability, scope and keys.
use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    style::Modifier,
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use withnav_client_api::secrets::{SecretScope, SecretTtl};

use crate::{
    state::{State, UiState},
    theme::Theme,
};

/// A shown value as one line of terminal output: escape sequences and control characters are
/// removed or replaced with a visible dot, so a pasted secret cannot drive the terminal. Display
/// only — the bytes that are submitted are the value's own.
fn shown_line(text: &str) -> String {
    // `strip_controls` drops ESC sequences and other controls but keeps LF and TAB for multiline
    // text; this field is one line, so they become a dot too.
    crate::reducer::strip_controls(text)
        .chars()
        .map(|c| if c.is_control() { '·' } else { c })
        .collect()
}

fn choice<'a, T: PartialEq + Copy>(
    options: &[(T, &'a str)],
    current: T,
    theme: &Theme,
) -> Vec<Span<'a>> {
    let mut spans = Vec::new();
    for (value, label) in options {
        let on = *value == current;
        spans.push(Span::styled(
            format!(
                " {}{label}{} ",
                if on { "[" } else { " " },
                if on { "]" } else { " " }
            ),
            if on {
                theme.text().add_modifier(Modifier::BOLD)
            } else {
                theme.muted()
            },
        ));
    }
    spans
}

pub(super) fn draw(frame: &mut Frame<'_>, area: Rect, state: &State, ui: &UiState, theme: &Theme) {
    let Some(entry) = &ui.secret_entry else {
        return;
    };
    let prompt = state
        .conversation
        .blocks
        .iter()
        .find_map(|b| match &b.kind {
            crate::state::BlockKind::Secret(p) if p.call_id == entry.call_id => Some(p),
            _ => None,
        });
    let Some(prompt) = prompt else {
        return;
    };
    super::pickers::dim(frame, area, theme);
    let width = area.width.saturating_sub(4).min(72);
    let height = area.height.saturating_sub(2).min(13);
    let rect = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, rect);
    let clean = |text: &str| crate::reducer::strip_controls(text);
    let field_width = usize::from(width.saturating_sub(8));
    let shown = if entry.shown {
        shown_line(&entry.value)
    } else {
        entry.display()
    };
    // The end of a long value stays visible, like a one-line input.
    let visible: String = {
        let chars: Vec<char> = shown.chars().collect();
        chars[chars.len().saturating_sub(field_width)..]
            .iter()
            .collect()
    };
    let status = if prompt.sending {
        "sending…"
    } else if entry.later {
        "give a value"
    } else {
        "waiting for your answer"
    };
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                format!("⚿ {}", clean(&prompt.request.label)),
                theme.text().add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" · {status}"),
                if entry.later {
                    theme.muted()
                } else {
                    theme.warning()
                },
            ),
        ]),
        Line::styled(
            format!(
                "For {}. Requested by {}.",
                clean(&prompt.request.reason),
                prompt
                    .agent
                    .as_deref()
                    .map(clean)
                    .unwrap_or_else(|| "Nav".into())
            ),
            theme.muted(),
        ),
        Line::raw(""),
        Line::from(vec![
            Span::styled(format!("${} ", entry.name), theme.muted()),
            Span::styled(
                if visible.is_empty() {
                    "▏".to_owned()
                } else {
                    format!("{visible}▏")
                },
                theme.text(),
            ),
        ]),
        Line::raw(""),
        Line::from(
            [
                vec![Span::styled("Available ", theme.muted())],
                choice(
                    &[
                        (SecretTtl::Hour, "1 h"),
                        (SecretTtl::Day, "24 h"),
                        (SecretTtl::Session, "Session"),
                    ],
                    entry.ttl,
                    theme,
                ),
            ]
            .concat(),
        ),
        Line::from(
            [
                vec![Span::styled("For       ", theme.muted())],
                choice(
                    &[
                        (SecretScope::Session, "This session"),
                        (SecretScope::Workspace, "This workspace"),
                    ],
                    entry.scope,
                    theme,
                ),
            ]
            .concat(),
        ),
        Line::raw(""),
        Line::styled(
            format!(
                "Kept out of the chat. The agent receives ${}, not the value.",
                entry.name
            ),
            theme.muted(),
        ),
        Line::styled(
            format!(
                "enter save · esc {} · tab availability · shift+tab scope · ctrl+r {} · ctrl+e write instead",
                if entry.later { "close" } else { "decline" },
                if entry.shown { "hide" } else { "show" },
            ),
            theme.muted(),
        ),
    ];
    // The refusal replaces the blank row under the value: the modal has a fixed height and its
    // last lines already reach the final row, so an added line would clip the key help. The
    // message is fixed text that never quotes the value, so it stays one row.
    if let Some(error) = entry.error.as_deref().or(prompt.error.as_deref()) {
        lines[4] = Line::styled(clean(error), theme.error());
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(if entry.later {
            theme.accent_line()
        } else {
            theme.warning()
        })
        .title(" Secret requested ")
        .title_alignment(Alignment::Center);
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .style(theme.surface())
            .block(block),
        rect,
    );
}
