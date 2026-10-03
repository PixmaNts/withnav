//! Draws the terminal UI without mutating the conversation projection.
mod header;
mod markdown;
pub mod panel;
mod permission;
mod pickers;
pub mod sidebar;
mod status;
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

/// The key bar: `key label` pairs, keys in text colour and labels muted, as many as fit in
/// `width` columns; they are in order of use, so the last ones give way first.
fn key_bar(hints: &[&str], width: usize, theme: &Theme) -> Vec<Span<'static>> {
    let mut spans = vec![Span::raw(" ")];
    let mut used = 1;
    for (index, hint) in hints.iter().enumerate() {
        let sep = if index == 0 { "" } else { " · " };
        let wanted = sep.chars().count() + hint.chars().count();
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

/// The keys that apply now: a permission prompt's while one waits for an answer, the deny
/// message's while one is typed, else the everyday ones.
fn hints(state: &State, ui: &UiState, editor: &Editor) -> &'static [&'static str] {
    // An answered request takes no keys: the usual hints come back.
    let answerable = state
        .open_permission()
        .filter(|p| state.answered_permission.as_deref() != Some(p.request_id.as_str()));
    let draft = answerable.and_then(|p| ui.deny_draft(&p.request_id));
    match (answerable, draft) {
        (Some(_), Some(_)) => &["enter deny with this message", "esc deny without one"],
        (Some(_), None) if editor.text().is_empty() => &[
            "y allow once",
            "a always",
            "n deny",
            "esc stop",
            "ctrl+p palette",
        ],
        _ if editor.text().is_empty() => &[
            "tab build/plan",
            "ctrl+p palette",
            "alt+1…9 tabs",
            "/model",
            "/compact",
            "esc stop",
            "alt+r panel",
            "f1 help",
        ],
        _ => &[
            "enter send",
            "alt+enter newline",
            "ctrl+p palette",
            "esc stop",
            "f1 help",
        ],
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
    let editor_height = (editor.text().lines().count().max(1) as u16 + 2).min(7);
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
    frame.render_widget(editor.area(), inner);
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
}
