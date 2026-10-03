//! Draws the terminal UI without mutating the conversation projection.
mod markdown;
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
}

fn shorten(path: &str, width: usize) -> String {
    let path = path.replace(std::env::var("HOME").as_deref().unwrap_or("\0"), "~");
    let chars: Vec<char> = path.chars().collect();
    if chars.len() <= width {
        path
    } else {
        format!(
            "…{}",
            chars[chars.len() - width.saturating_sub(1)..]
                .iter()
                .collect::<String>()
        )
    }
}

/// Splits a hint line (`key label  key label`) into keys in text colour and labels in muted.
fn hint_spans(hint: &str, theme: &Theme) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (index, part) in hint.split("  ").enumerate() {
        if index > 0 {
            spans.push(Span::raw("  "));
        }
        let lead = part.len() - part.trim_start().len();
        spans.push(Span::raw(part[..lead].to_string()));
        let part = &part[lead..];
        match part.split_once(' ') {
            Some((key, label)) => {
                spans.push(Span::styled(key.to_string(), theme.text()));
                spans.push(Span::styled(format!(" {label}"), theme.muted()));
            }
            None => spans.push(Span::styled(part.to_string(), theme.text())),
        }
    }
    spans
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
    if area.width < 24 || area.height < 9 {
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
    let path = state
        .session
        .as_ref()
        .map(|s| s.workspace.as_str())
        .unwrap_or("workspace");
    let title = state
        .session
        .as_ref()
        .map(|s| {
            let name = state
                .sessions
                .iter()
                .find(|entry| entry.id == s.session_id)
                .and_then(|entry| entry.title.as_deref())
                .unwrap_or("Conversation");
            format!(
                "{} · {}",
                shorten(name, usize::from(area.width / 4)),
                shorten(&s.session_id, 12)
            )
        })
        .unwrap_or_else(|| "new conversation".into());
    let path = shorten(path, (area.width / 3) as usize);
    let used = " navai  ".chars().count() + path.chars().count() + title.chars().count() + 1;
    let gap = usize::from(area.width).saturating_sub(used);
    frame.buffer_mut().set_style(header, theme.panel());
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" navai", theme.title()),
            Span::raw("  "),
            Span::styled(path, theme.muted()),
            Span::raw(" ".repeat(gap)),
            Span::styled(format!("{title} "), theme.strong()),
        ])),
        header,
    );
    frame.render_widget(
        Paragraph::new("─".repeat(usize::from(area.width))).style(theme.border()),
        rule,
    );
    transcript::draw(frame, transcript, state, ui, &theme, markdown);
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
        let hint = " enter send  alt+enter newline  esc stop  ctrl+p commands  f1 help";
        frame.render_widget(Paragraph::new(Line::from(hint_spans(hint, &theme))), hints);
    }
    pickers::draw(frame, full, state, ui, &theme);
}
