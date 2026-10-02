//! Draws the terminal UI without mutating the conversation projection.
mod markdown;
mod pickers;
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
    widgets::{Block, Borders, Paragraph},
    Frame,
};

#[derive(Debug, Default)]
pub struct RenderCache {
    markdown: markdown::MarkdownCache,
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

pub fn draw(
    frame: &mut Frame<'_>,
    state: &State,
    ui: &UiState,
    editor: &Editor,
    cache: &mut RenderCache,
) {
    let area = frame.area();
    if area.width < 24 || area.height < 9 {
        let bottom = area.bottom().saturating_sub(3);
        frame.render_widget(
            Paragraph::new(" resize terminal to see conversation").style(Theme.warning()),
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
    let left = format!(" rust-agent  {}", shorten(path, (area.width / 3) as usize));
    let right = format!("{title} ");
    let gap = usize::from(area.width).saturating_sub(left.chars().count() + right.chars().count());
    frame.render_widget(
        Paragraph::new(format!("{left}{}{right}", " ".repeat(gap))).style(Theme.strong()),
        header,
    );
    frame.render_widget(
        Paragraph::new("─".repeat(usize::from(area.width))).style(Theme.border()),
        rule,
    );
    transcript::draw(frame, transcript, state, ui, &mut cache.markdown);
    frame.render_widget(
        Paragraph::new("─".repeat(usize::from(area.width))).style(Theme.border()),
        separator,
    );
    status::draw(frame, status, state, ui);
    let border = Block::default()
        .title(" Message ")
        .borders(Borders::ALL)
        .border_style(if ui.focus == crate::tui::state::Focus::Editor {
            Theme.accent()
        } else {
            Theme.border()
        });
    let inner = border.inner(input);
    frame.render_widget(border, input);
    frame.render_widget(editor.area(), inner);
    let hint = if ui.quit_confirm {
        " ctrl+q again to quit · esc to keep working"
    } else {
        " enter send  alt+enter newline  esc stop  ctrl+p commands  f1 help"
    };
    frame.render_widget(Paragraph::new(hint).style(Theme.muted()), hints);
    pickers::draw(frame, area, state, ui);
}
