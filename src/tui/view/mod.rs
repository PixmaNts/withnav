//! Draws the terminal UI without mutating the conversation projection.
mod header;
mod markdown;
pub mod panel;
mod permission;
mod pickers;
pub mod sidebar;
pub(crate) mod status;
pub mod text;
pub mod tool;
mod transcript;
use crate::tui::{
    input::Editor,
    state::{State, UiState},
    theme::Theme,
};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

/// Parsed markdown, one cache per tab conversation: block ids are only unique within a session, so
/// two tabs never share an entry.
#[derive(Debug, Default)]
pub struct RenderCache {
    markdown: std::collections::HashMap<u64, markdown::MarkdownCache>,
    /// The open permission request whose keys line was on screen at the last draw.
    pub prompt_shown: Option<String>,
    /// Whether the right column was drawn.
    pub panel_drawn: bool,
}

/// One way to show the keys that apply now: the hints in order of use, the first `required` of
/// which must all be on the bar.
struct Hints {
    hints: &'static [&'static str],
    required: usize,
}

/// The columns `hints` take on the bar, separators and the leading space included.
fn bar_width(hints: &[&str]) -> usize {
    1 + hints.iter().map(|h| text::columns(h)).sum::<usize>() + 3 * hints.len().saturating_sub(1)
}

/// The key bar: `key label` pairs, keys in text colour and labels muted. The first of `choices`
/// whose required hints fit is used; from it, as many hints as fit, the last ones giving way.
fn key_bar(choices: &[Hints], width: usize, theme: &Theme) -> Vec<Span<'static>> {
    let chosen = choices
        .iter()
        .find(|c| bar_width(&c.hints[..c.required]) <= width)
        .or(choices.last())
        .map_or(&[][..], |c| c.hints);
    let mut spans = vec![Span::raw(" ")];
    let mut used = 1;
    for (index, hint) in chosen.iter().enumerate() {
        let sep = if index == 0 { "" } else { " · " };
        let wanted = text::columns(sep) + text::columns(hint);
        if used + wanted > width {
            break;
        }
        used += wanted;
        spans.push(Span::styled(sep, theme.border()));
        match hint.split_once(' ') {
            Some((key, label)) => {
                spans.push(Span::styled(key.to_string(), theme.text()));
                spans.push(Span::styled(format!(" {label}"), theme.muted()));
            }
            None => spans.push(Span::styled(hint.to_string(), theme.text())),
        }
    }
    spans
}

/// The keys that apply now: a permission prompt's while one waits for an answer (all three
/// choices always shown, shorter when the row is narrow), the deny message's while one is typed,
/// else the everyday ones.
fn hints(state: &State, ui: &UiState, editor: &Editor) -> &'static [Hints] {
    if state.watchdog_paused {
        return &[Hints {
            hints: &["alt+c continue", "esc stop"],
            required: 1,
        }];
    }
    if state.read_only() {
        if matches!(
            state.active_child_status(),
            crate::tui::state::ChildStatus::OutOfSteps | crate::tui::state::ChildStatus::Doubt
        ) {
            return &[Hints {
                hints: &[
                    "alt+c continue",
                    "alt+w parent",
                    "alt+s subagents",
                    "esc stop",
                ],
                required: 1,
            }];
        }
        return &[Hints {
            hints: &[
                "alt+w parent",
                "alt+s subagents",
                "page↑/↓ scroll",
                "esc stop",
                "f1 help",
            ],
            required: 0,
        }];
    }
    if !state.children.is_empty() && state.open_permission().is_none() && editor.text().is_empty() {
        return &[Hints {
            hints: &[
                "alt+s subagents",
                "tab build/plan",
                "ctrl+p palette",
                "alt+1…9 tabs",
                "esc stop",
                "f1 help",
            ],
            required: 1,
        }];
    }
    // An answered request takes no keys: the usual hints come back.
    let answerable = state
        .open_permission()
        .filter(|p| state.answered_permission.as_deref() != Some(p.request_id.as_str()));
    let draft = answerable.and_then(|p| ui.deny_draft(&p.request_id));
    match (answerable, draft) {
        (Some(_), Some(_)) => &[
            Hints {
                hints: &["enter deny with this message", "esc deny without one"],
                required: 2,
            },
            Hints {
                hints: &["enter deny", "esc no message"],
                required: 2,
            },
            Hints {
                hints: &["enter deny", "esc omit"],
                required: 2,
            },
        ],
        // The plan agent's exceptions are allowed once or denied, never always.
        (Some(open), None) if editor.text().is_empty() && !permission::offers_always(open) => &[
            Hints {
                hints: &["y allow once", "n deny", "esc stop", "ctrl+p palette"],
                required: 2,
            },
            Hints {
                hints: &["y once", "n deny", "esc stop"],
                required: 2,
            },
            Hints {
                hints: &["y", "n"],
                required: 2,
            },
        ],
        (Some(_), None) if editor.text().is_empty() => &[
            Hints {
                hints: &[
                    "y allow once",
                    "a always",
                    "n deny",
                    "esc stop",
                    "ctrl+p palette",
                ],
                required: 3,
            },
            Hints {
                hints: &["y once", "a always", "n deny", "esc stop"],
                required: 3,
            },
            Hints {
                hints: &["y", "a", "n"],
                required: 3,
            },
        ],
        _ if editor.text().is_empty() => &[Hints {
            hints: &[
                "tab build/plan",
                "ctrl+p palette",
                "alt+1…9 tabs",
                "/model",
                "/compact",
                "esc stop",
                "alt+r panel",
                "f1 help",
            ],
            required: 0,
        }],
        _ => &[Hints {
            hints: &[
                "enter send",
                "alt+enter newline",
                "ctrl+p palette",
                "esc stop",
                "f1 help",
            ],
            required: 0,
        }],
    }
}

pub fn draw(
    frame: &mut Frame<'_>,
    state: &State,
    ui: &UiState,
    editor: &Editor,
    cache: &mut RenderCache,
) {
    let theme = ui.theme.palette();
    // A tab closed or moved to another conversation drops its cache.
    cache
        .markdown
        .retain(|generation, _| state.tabs.iter().any(|t| t.generation.0 == *generation));
    let markdown = cache.markdown.entry(state.generation.0).or_default();
    markdown.set_theme(theme);
    let area = frame.area();
    frame.buffer_mut().set_style(area, theme.base());
    cache.panel_drawn = false;
    if area.width < 24 || area.height < 9 {
        cache.prompt_shown = None;
        let bottom = area.bottom().saturating_sub(3);
        frame.render_widget(
            Paragraph::new(" resize terminal to see conversation").style(theme.warning()),
            Rect::new(area.x, area.y, area.width, 1),
        );
        frame.render_widget(
            editor.area(),
            Rect::new(
                area.x,
                bottom,
                area.width,
                area.bottom().saturating_sub(bottom),
            ),
        );
        return;
    }
    let full = area;
    let panel_shown = panel::shown(ui, area.width);
    let area = if sidebar::shown(ui, area.width) {
        let [bar, rest] = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(sidebar::WIDTH), Constraint::Min(1)])
            .areas(area);
        sidebar::draw(frame, bar, state, &theme);
        rest
    } else {
        area
    };
    let editor_height = editor_height(editor, area.height);
    let [header, rule, transcript, separator, status, input, hints] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(editor_height),
            Constraint::Length(1),
        ])
        .areas(area);
    header::draw(frame, header, state, &theme);
    frame.render_widget(
        Paragraph::new("─".repeat(usize::from(area.width))).style(theme.border()),
        rule,
    );
    // The right column sits beside the transcript; the transcript keeps at least 40 columns.
    let transcript = if panel_shown && transcript.width >= panel::WIDTH + 40 {
        let [left, right] = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(1), Constraint::Length(panel::WIDTH)])
            .areas(transcript);
        panel::draw(frame, right, state, &theme);
        cache.panel_drawn = true;
        left
    } else {
        transcript
    };
    cache.prompt_shown = transcript::draw(frame, transcript, state, ui, &theme, markdown);
    frame.render_widget(
        Paragraph::new("─".repeat(usize::from(area.width))).style(theme.border()),
        separator,
    );
    frame.buffer_mut().set_style(status, theme.panel());
    status::draw(frame, status, state, ui, &theme);
    let focused = ui.focus == crate::tui::state::Focus::Editor;
    let border = Block::default()
        .title(Span::styled(
            " Message ",
            if focused {
                theme.accent()
            } else {
                theme.muted()
            },
        ))
        .borders(Borders::ALL)
        .border_style(if focused {
            theme.accent_line()
        } else {
            theme.border()
        });
    let inner = border.inner(input);
    frame.render_widget(border, input);
    if state.read_only() {
        frame.render_widget(
            Paragraph::new("This subagent can't be written to · alt+w returns to its parent")
                .style(theme.muted()),
            inner,
        );
    } else {
        frame.render_widget(editor.area(), inner);
    }
    frame.buffer_mut().set_style(hints, theme.panel());
    if ui.quit_confirm {
        frame.render_widget(
            Paragraph::new(" ctrl+q again to quit · esc to keep working").style(theme.warning()),
            hints,
        );
    } else {
        let bar = key_bar(
            self::hints(state, ui, editor),
            usize::from(hints.width),
            &theme,
        );
        frame.render_widget(Paragraph::new(Line::from(bar)), hints);
    }
    pickers::draw(frame, full, state, ui, &theme);
    if let Some((_, _, control)) = &ui.child_confirm {
        pickers::confirm_child(frame, full, state.agent_name(), *control, &theme);
    }
}

fn editor_height(editor: &Editor, available_height: u16) -> u16 {
    // Five chrome rows and five transcript rows stay visible whenever the terminal permits; the
    // editor itself never shrinks below one text line, so the driver can always type.
    let desired = editor.area().lines().len().clamp(1, 20) as u16 + 2;
    desired.min(available_height.saturating_sub(10).max(3))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};
    use ratatui_textarea::CursorMove;

    #[test]
    fn draft_grows_to_twenty_lines_and_preserves_transcript_space() {
        let mut editor = Editor::new();
        for (lines, expected) in [(1, 3), (7, 9), (20, 22), (25, 22)] {
            editor.set_text(&vec!["draft"; lines].join("\n"));
            assert_eq!(editor_height(&editor, 40), expected);
        }
        assert_eq!(editor_height(&editor, 24), 14);
        assert_eq!(editor_height(&editor, 8), 3);
        editor.set_text("draft\n");
        assert_eq!(editor_height(&editor, 40), 4);
    }

    #[test]
    fn textarea_scrolls_to_keep_cursor_visible_beyond_twenty_lines() {
        let mut editor = Editor::new();
        editor.set_text(
            &(0..25)
                .map(|i| format!("line {i:02}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        editor.area_mut().move_cursor(CursorMove::Bottom);
        let mut terminal = Terminal::new(TestBackend::new(30, 20)).unwrap();
        terminal
            .draw(|frame| frame.render_widget(editor.area(), frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let text = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("line 24"));
        assert!(!text.contains("line 00"));
        editor.area_mut().move_cursor(CursorMove::Top);
        terminal
            .draw(|frame| frame.render_widget(editor.area(), frame.area()))
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("line 00"));
        assert!(!text.contains("line 24"));
    }
}
