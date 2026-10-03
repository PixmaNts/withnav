//! The inline permission prompt: what the gated call would do, and the keys that answer it.
use crate::tui::{state::PermissionPrompt, theme::Theme};
use ratatui::{
    style::Modifier,
    text::{Line, Span},
};
use serde_json::Value;

/// Lines of a file or a command shown before the rest is summed up.
const PREVIEW_LINES: usize = 6;

/// What the keys do on an open prompt.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Keys<'a> {
    /// `y`, `a` and `n` answer it.
    Ready,
    /// `n` was pressed: this deny message is being typed.
    Denying(&'a str),
    /// An answer was sent: nothing until the server ends the request.
    Sent,
    /// Not the request the keys go to (another tab, or an older one waits first).
    Inactive,
}

/// The prompt's lines, each cut to `width` columns so it takes exactly one row.
pub fn lines(
    p: &PermissionPrompt,
    keys: Keys<'_>,
    theme: &Theme,
    width: usize,
) -> Vec<Line<'static>> {
    build(p, keys, theme)
        .into_iter()
        .map(|line| cut(line, width))
        .collect()
}

/// `line` cut to `width` columns, ending with `…` when cut.
fn cut(line: Line<'static>, width: usize) -> Line<'static> {
    if line.width() <= width || width == 0 {
        return line;
    }
    let mut left = width - 1;
    let mut spans = Vec::new();
    for span in line.spans {
        if left == 0 {
            break;
        }
        let w = span.width();
        if w <= left {
            left -= w;
            spans.push(span);
            continue;
        }
        let mut text = String::new();
        for c in span.content.chars() {
            let next = Span::raw(format!("{text}{c}")).width();
            if next > left {
                break;
            }
            text.push(c);
        }
        left = 0;
        spans.push(Span::styled(text, span.style));
    }
    spans.push(Span::raw("…"));
    Line::from(spans).style(line.style)
}

fn build(p: &PermissionPrompt, keys: Keys<'_>, theme: &Theme) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(vec![
        Span::styled(
            "PERMISSION",
            if p.is_open() {
                theme.accent()
            } else {
                theme.muted()
            }
            .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" · {} wants to run ", p.agent_or_default()),
            theme.muted(),
        ),
        Span::styled(p.tool.clone(), theme.strong()),
        Span::styled(format!(" {}", scope_text(p)), theme.code_ref()),
    ])];
    for line in preview(p) {
        lines.push(Line::styled(format!(" {line}"), theme.text()));
    }
    match (&p.resolution, keys) {
        (None, _) if p.stale => lines.push(Line::styled(" no longer waiting", theme.muted())),
        (None, Keys::Denying(message)) => {
            lines.push(Line::from(vec![
                Span::styled(" deny with a message: ", theme.warning()),
                Span::styled(message.to_string(), theme.text()),
                Span::styled("▏", theme.accent_line()),
            ]));
            lines.push(Line::styled(
                " enter send · esc deny without a message",
                theme.muted(),
            ));
        }
        (None, Keys::Ready) => lines.push(Line::from(vec![
            Span::styled(" y", theme.strong()),
            Span::styled(" allow once   ", theme.muted()),
            Span::styled("a", theme.strong()),
            Span::styled(format!(" always {}   ", always_text(p)), theme.muted()),
            Span::styled("n", theme.strong()),
            Span::styled(" deny", theme.muted()),
        ])),
        (None, Keys::Sent) => {
            lines.push(Line::styled(" sent, waiting for the server", theme.muted()))
        }
        (None, Keys::Inactive) => {
            lines.push(Line::styled(" waiting for your answer", theme.warning()))
        }
        (Some(resolution), _) => lines.push(outcome(p, resolution, theme)),
    }
    lines
}

/// `in /path` for a folder, `to https://host` for an origin.
pub fn scope_text(p: &PermissionPrompt) -> String {
    if p.scope_kind == "origin" {
        format!("to {}", p.scope)
    } else {
        format!("in {}", p.scope)
    }
}

/// What `a` grants: `for this folder` or `for this site`.
fn always_text(p: &PermissionPrompt) -> &'static str {
    if p.scope_kind == "origin" {
        "for this site"
    } else {
        "for this folder"
    }
}

fn outcome(p: &PermissionPrompt, resolution: &str, theme: &Theme) -> Line<'static> {
    let (text, style) = match resolution {
        "allow_once" => ("allowed once".to_string(), theme.success()),
        "allow_folder" => (format!("allowed {}", always_text(p)), theme.success()),
        "deny" => match p.message.as_deref().filter(|m| !m.is_empty()) {
            Some(message) => (format!("denied: {message}"), theme.error()),
            None => ("denied".to_string(), theme.error()),
        },
        "cancelled" => ("cancelled".to_string(), theme.muted()),
        "expired" => ("expired: the session restarted".to_string(), theme.muted()),
        other => (other.to_string(), theme.muted()),
    };
    Line::styled(format!(" {text}"), style)
}

/// A short look at what the call would do: the path and the first lines for `write`, the
/// replaced and new text for `edit`, the command for `bash`, the URL for `fetch`; the raw
/// arguments for anything else.
pub fn preview(p: &PermissionPrompt) -> Vec<String> {
    let args: Value = serde_json::from_str(&p.arguments).unwrap_or(Value::Null);
    let text = |key: &str| args.get(key).and_then(Value::as_str).map(str::to_string);
    let mut out = Vec::new();
    match p.tool.as_str() {
        "write" => {
            if let Some(path) = text("path") {
                out.push(path);
            }
            if let Some(content) = text("content") {
                add_lines(&mut out, &content, "+ ");
            }
        }
        "edit" => {
            if let Some(path) = text("path") {
                out.push(path);
            }
            if let Some(old) = text("old_string") {
                add_lines(&mut out, &old, "- ");
            }
            if let Some(new) = text("new_string") {
                add_lines(&mut out, &new, "+ ");
            }
        }
        "bash" => {
            if let Some(command) = text("command") {
                add_lines(&mut out, &command, "$ ");
            }
        }
        "fetch" => {
            if let Some(url) = text("url") {
                out.push(url);
            }
        }
        _ => {}
    }
    if out.is_empty() && !p.arguments.is_empty() {
        add_lines(&mut out, &p.arguments, "");
    }
    out
}

/// Adds the first lines of `text` with `prefix`, and how many more there are.
fn add_lines(out: &mut Vec<String>, text: &str, prefix: &str) {
    let all: Vec<&str> = text.lines().collect();
    for line in all.iter().take(PREVIEW_LINES) {
        out.push(format!("{prefix}{line}"));
    }
    if all.len() > PREVIEW_LINES {
        out.push(format!("  … {} more lines", all.len() - PREVIEW_LINES));
    }
}

trait AgentName {
    fn agent_or_default(&self) -> String;
}

impl AgentName for PermissionPrompt {
    fn agent_or_default(&self) -> String {
        if self.agent.is_empty() {
            "the agent".into()
        } else {
            self.agent.clone()
        }
    }
}
