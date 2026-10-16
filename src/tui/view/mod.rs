//! Draws the terminal UI without mutating the conversation projection.
mod config;
mod header;
mod hints;
mod markdown;
pub mod panel;
mod permission;
mod pickers;
pub(crate) mod selection;
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
#[cfg(test)]
pub(crate) use permission::test_prompt;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};
pub(crate) use transcript::copy_block;
pub(crate) use transcript::permission_sections;

use hints::{
    ARCHIVE, CHILD_PAUSED, CHILD_TRANSCRIPT, COPY_MODE, IDLE, PERMISSION, PERMISSION_ONCE,
    QUESTION, QUESTION_CUSTOM, REJECT_MESSAGE, SUBAGENTS, TYPING, WATCHDOG_PAUSED,
};
/// Parsed markdown, one cache per tab conversation: block ids are only unique within a session, so
/// two tabs never share an entry.
#[derive(Debug, Default)]
pub struct RenderCache {
    pub transcript: selection::TranscriptMap,
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

/// The situation the key bar describes. `key_context` tells which one applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyContext {
    Archive,
    WatchdogPaused,
    /// A child transcript whose agent waits for `continue`.
    ChildPaused,
    ChildTranscript,
    /// The transcript has the focus: keys select and copy.
    CopyMode,
    Subagents,
    Question,
    QuestionCustom,
    /// A deny message is being typed for the open permission request.
    RejectMessage,
    /// An open permission request with no reusable approval offered.
    PermissionOnce,
    Permission,
    Idle,
    Typing,
}

impl Hints {
    const fn new(hints: &'static [&'static str], required: usize) -> Self {
        Self { hints, required }
    }
}

/// Which situation the key bar describes, the first that applies in this order.
fn key_context(state: &State, ui: &UiState, editor: &Editor) -> KeyContext {
    use crate::tui::state::{BlockKind, ChildStatus, Focus};
    use KeyContext::*;
    let empty = editor.text().is_empty();
    let open = state.open_permission();
    if state.archive() {
        return Archive;
    }
    if state.watchdog_paused && open.is_none() {
        return WatchdogPaused;
    }
    if state.read_only() {
        return match state.active_child_status() {
            ChildStatus::OutOfSteps | ChildStatus::Doubt => ChildPaused,
            _ => ChildTranscript,
        };
    }
    if ui.focus == Focus::Transcript && open.is_none() {
        return CopyMode;
    }
    if !state.children.is_empty() && open.is_none() && empty {
        return Subagents;
    }
    // An answered request takes no keys: the usual hints come back.
    let answerable =
        open.filter(|p| state.answered_permission.as_deref() != Some(p.request_id.as_str()));
    let Some(open) = answerable else {
        let question = state
            .conversation
            .blocks
            .iter()
            .rev()
            .find_map(|b| match &b.kind {
                BlockKind::Question(p) if p.answer.is_none() => Some(p),
                _ => None,
            });
        return match question {
            Some(p) if !p.dismissed && empty => Question,
            Some(p) if p.custom => QuestionCustom,
            _ if empty => Idle,
            _ => Typing,
        };
    };
    match (ui.deny_draft(&open.request_id), empty) {
        (Some(_), _) => RejectMessage,
        // Without a server-supplied label, no reusable approval is offered.
        (None, true) if !permission::offers_always(open) => PermissionOnce,
        (None, true) => Permission,
        (None, false) => Typing,
    }
}

/// The keys that apply now: a permission prompt's while one waits for an answer (all three
/// choices always shown, shorter when the row is narrow), the deny message's while one is typed,
/// else the everyday ones.
fn hints(state: &State, ui: &UiState, editor: &Editor) -> &'static [Hints] {
    match key_context(state, ui, editor) {
        KeyContext::Archive => ARCHIVE,
        KeyContext::WatchdogPaused => WATCHDOG_PAUSED,
        KeyContext::ChildPaused => CHILD_PAUSED,
        KeyContext::ChildTranscript => CHILD_TRANSCRIPT,
        KeyContext::CopyMode => COPY_MODE,
        KeyContext::Subagents => SUBAGENTS,
        KeyContext::Question => QUESTION,
        KeyContext::QuestionCustom => QUESTION_CUSTOM,
        KeyContext::RejectMessage => REJECT_MESSAGE,
        KeyContext::PermissionOnce => PERMISSION_ONCE,
        KeyContext::Permission => PERMISSION,
        KeyContext::Idle => IDLE,
        KeyContext::Typing => TYPING,
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
    cache.transcript = selection::TranscriptMap::default();
    // A tab closed or moved to another conversation drops its cache.
    cache
        .markdown
        .retain(|generation, _| state.tabs.iter().any(|t| t.generation.0 == *generation));
    let markdown = cache.markdown.entry(state.generation.0).or_default();
    markdown.set_theme(theme);
    let area = frame.area();
    frame.buffer_mut().set_style(area, theme.base());
    cache.panel_drawn = false;
    if ui.screen == crate::tui::config::Screen::Config {
        cache.prompt_shown = None;
        config::draw(frame, area, state, ui, &theme);
        pickers::draw(frame, area, state, ui, &theme);
        config::overlays(frame, area, state, &theme);
        return;
    }
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
    cache.prompt_shown = transcript::draw(
        frame,
        transcript,
        state,
        ui,
        &theme,
        markdown,
        &mut cache.transcript,
    );
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
            Paragraph::new(if state.archive() {
                "Archive is read-only · copy text or close with alt+w"
            } else {
                "This subagent can't be written to · alt+w returns to its parent"
            })
            .style(theme.muted()),
            inner,
        );
    } else {
        frame.render_widget(editor.area(), inner);
    }
    frame.buffer_mut().set_style(hints, theme.panel());
    if let Some(key) = ui.quit_confirm {
        frame.render_widget(
            Paragraph::new(format!(" {key} again to quit · esc to keep working"))
                .style(theme.warning()),
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
    // editor itself never shrinks below one text line, so the Captain can always type.
    let desired = editor.area().lines().len().clamp(1, 20) as u16 + 2;
    desired.min(available_height.saturating_sub(10).max(3))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};
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
