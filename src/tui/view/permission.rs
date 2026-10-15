//! The inline permission prompt: what the gated call would do, and the keys that answer it.
use crate::tui::{state::PermissionPrompt, theme::Theme};
use ratatui::{
    style::Modifier,
    text::{Line, Span},
};
use serde_json::Value;
use unicode_segmentation::UnicodeSegmentation;

/// Lines of a file or a command shown before the rest is summed up.
const PREVIEW_LINES: usize = 6;

#[cfg(test)]
pub(crate) fn test_prompt() -> PermissionPrompt {
    PermissionPrompt {
        always_label: None,
        skill_script: None,
        child_session_id: None,
        request_id: "R".into(),
        tool_call_id: "C".into(),
        tool: "bash".into(),
        arguments: serde_json::json!({"command":"echo long command"}).to_string(),
        scope_kind: "folder".into(),
        scope: "/workspace".into(),
        agent: "build".into(),
        reason: "long reason".into(),
        resolution: Some("allow_once".into()),
        message: None,
        always_saved: true,
        stale: false,
    }
}

/// Clipboard sections stay complete even when the review cuts its screen preview.
pub(crate) fn copy_sections(p: &PermissionPrompt, call: Option<&Value>, sent: bool) -> [String; 4] {
    let parsed = serde_json::from_str::<Value>(&p.arguments).ok();
    let args = call.filter(|a| a.is_object()).or(parsed.as_ref());
    let payload = match args {
        Some(args) if p.tool == "bash" => args
            .get("command")
            .and_then(Value::as_str)
            .map(|s| format!("$ {s}"))
            .unwrap_or_else(|| args.to_string()),
        Some(args) if p.tool == "write" => {
            let mut args = args.clone();
            if let Some(content) = args.get_mut("content") {
                if let Some(text) = content.as_str() {
                    let mut end = text.len().min(4096);
                    while !text.is_char_boundary(end) {
                        end -= 1;
                    }
                    *content = Value::String(if end < text.len() {
                        format!("{}\n({} bytes omitted)", &text[..end], text.len() - end)
                    } else {
                        text.to_owned()
                    });
                }
            }
            serde_json::to_string_pretty(&args).expect("JSON value")
        }
        Some(args) => serde_json::to_string_pretty(args).expect("JSON value"),
        None => format!(
            "{}\n(arguments shortened by the server; full arguments unavailable)",
            p.arguments
        ),
    };
    let outcome = if let Some(resolution) = &p.resolution {
        outcome(p, resolution, &Theme::default())
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>()
            .trim_start()
            .to_owned()
    } else if p.stale {
        "no longer waiting".into()
    } else if sent {
        "sent, waiting for the server".into()
    } else {
        "waiting for your answer".into()
    };
    [
        format!(
            "PERMISSION · {} wants to run {} {}",
            if p.agent.is_empty() {
                "the agent"
            } else {
                &p.agent
            },
            p.tool,
            scope_text(p)
        ),
        match &p.always_label {
            Some(label) => format!("{}\n{label}", p.reason),
            None => p.reason.clone(),
        },
        payload,
        outcome,
    ]
    .map(|text| {
        text.chars()
            .map(|c| {
                if c.is_control() && c != '\n' && c != '\t' {
                    ' '
                } else {
                    c
                }
            })
            .collect()
    })
}

/// What the keys do on an open prompt.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Keys<'a> {
    Historical,
    /// `o`, `a` and `r` answer it.
    Ready,
    /// `r` was pressed: this deny message is being typed.
    Denying(&'a str),
    /// An answer was sent: nothing until the server ends the request.
    Sent,
    /// Not the request the keys go to (another tab, or an older one waits first).
    Inactive,
}

/// The prompt's lines, with complete proposed rules wrapped to `width`. `call` is the
/// tool call's own arguments, from the transcript, when they are known.
pub fn lines(
    p: &PermissionPrompt,
    call: Option<&Value>,
    keys: Keys<'_>,
    theme: &Theme,
    width: usize,
) -> Vec<Line<'static>> {
    let mut rendered: Vec<_> = build(p, call, keys, theme)
        .into_iter()
        .map(|line| cut(line, width))
        .collect();
    if let Some(label) = &p.always_label {
        let mut row = String::new();
        let mut rules = Vec::new();
        for grapheme in label.graphemes(true) {
            if width > 0 && !row.is_empty() && Line::raw(format!("{row}{grapheme}")).width() > width
            {
                rules.push(Line::styled(std::mem::take(&mut row), theme.muted()));
            }
            row.push_str(grapheme);
        }
        if !row.is_empty() {
            rules.push(Line::styled(row, theme.muted()));
        }
        let before_outcome = rendered.len().saturating_sub(1);
        rendered.splice(before_outcome..before_outcome, rules);
    }
    rendered
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

fn build(
    p: &PermissionPrompt,
    call: Option<&Value>,
    keys: Keys<'_>,
    theme: &Theme,
) -> Vec<Line<'static>> {
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
            format!(
                " · {} wants to run ",
                if p.agent.is_empty() {
                    "the agent"
                } else {
                    &p.agent
                }
            ),
            theme.muted(),
        ),
        Span::styled(p.tool.clone(), theme.strong()),
        Span::styled(format!(" {}", scope_text(p)), theme.code_ref()),
    ])];
    // Why the server asks, one muted line (the width cut in `lines` bounds it).
    let reason = p.reason.split_whitespace().collect::<Vec<_>>().join(" ");
    if !reason.is_empty() {
        lines.push(Line::styled(format!(" {reason}"), theme.muted()));
    }
    for line in preview(p, call) {
        let style = if line.starts_with('(') {
            theme.muted()
        } else {
            theme.text()
        };
        lines.push(Line::styled(format!(" {line}"), style));
    }
    match (&p.resolution, keys) {
        (None, _) if p.stale => lines.push(Line::styled(" no longer waiting", theme.muted())),
        (None, Keys::Denying(message)) => {
            lines.push(Line::from(vec![
                Span::styled(" reject with a message: ", theme.warning()),
                Span::styled(message.to_string(), theme.text()),
                Span::styled("▏", theme.accent_line()),
            ]));
            lines.push(Line::styled(
                " enter send · esc reject without a message",
                theme.muted(),
            ));
        }
        (None, Keys::Ready) if !offers_always(p) => lines.push(Line::from(vec![
            Span::styled(" o", theme.strong()),
            Span::styled(" once · ", theme.muted()),
            Span::styled("r", theme.strong()),
            Span::styled(" reject", theme.muted()),
        ])),
        (None, Keys::Ready) => lines.push(Line::from(vec![
            Span::styled(" o", theme.strong()),
            Span::styled(" once   ", theme.muted()),
            Span::styled("a", theme.strong()),
            Span::styled(" Always   ", theme.muted()),
            Span::styled("r", theme.strong()),
            Span::styled(" reject", theme.muted()),
        ])),
        (None, Keys::Sent) => {
            lines.push(Line::styled(" sent, waiting for the server", theme.muted()))
        }
        (None, Keys::Inactive) => {
            lines.push(Line::styled(" waiting for your answer", theme.warning()))
        }
        (None, Keys::Historical) => lines.push(Line::styled(
            " historical request · archive is read-only",
            theme.muted(),
        )),
        (Some(resolution), _) => lines.push(outcome(p, resolution, theme)),
    }
    lines
}

/// Always is only offered with a server-supplied rule and scope label.
pub(crate) fn offers_always(p: &PermissionPrompt) -> bool {
    p.always_label.is_some()
}

#[cfg(test)]
#[test]
fn always_answer_requires_the_server_label() {
    let mut prompt = test_prompt();
    prompt.resolution = None;
    let theme = super::super::theme::Theme::default();
    assert!(!offers_always(&prompt));
    prompt.always_label = Some("Always (project): bash git push *".into());
    assert!(offers_always(&prompt));
    assert!(lines(&prompt, None, Keys::Ready, &theme, 200)
        .iter()
        .any(|line| line
            .to_string()
            .contains("Always (project): bash git push *")));
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
        "allow_always" if !p.always_saved => (
            format!(
                "Always not saved: {}; ran once",
                p.message.as_deref().unwrap_or("unknown error")
            ),
            theme.warning(),
        ),
        "allow_always" => ("allowed always".to_string(), theme.success()),
        "allow_once" => ("allowed once".to_string(), theme.success()),
        "allow_skill_machine" => ("allowed skill on this machine".to_string(), theme.success()),
        "allow_config_session" => (
            "allowed WithNav config for this session".to_string(),
            theme.success(),
        ),
        "allow_folder" => (format!("allowed {}", always_text(p)), theme.success()),
        "deny" | "reject" => match p.message.as_deref().filter(|m| !m.is_empty()) {
            Some(message) => (format!("denied: {message}"), theme.error()),
            None => ("denied".to_string(), theme.error()),
        },
        "cancelled" => ("cancelled".to_string(), theme.muted()),
        "expired" => ("expired: the session restarted".to_string(), theme.muted()),
        other => (other.to_string(), theme.muted()),
    };
    Line::styled(format!(" {text}"), style)
}

/// The longest preview line kept, in characters, before it is cut to the screen's width.
const PREVIEW_LINE_CHARS: usize = 400;

/// A short look at what the call would do: the path (always on its own line) and the first lines
/// for `write`, the replaced and new text for `edit`, the command for `bash`, the URL for
/// `fetch`; the raw arguments for anything else. `call` is the tool call's own arguments; without
/// them the server's copy is read, which it cuts to 4 KiB, and the preview says so.
pub fn preview(p: &PermissionPrompt, call: Option<&Value>) -> Vec<String> {
    let parsed = serde_json::from_str::<Value>(&p.arguments).ok();
    let args = call.filter(|a| a.is_object()).or(parsed.as_ref());
    let cut_by_server = call.is_none_or(|a| !a.is_object()) && parsed.is_none();
    // From the call's arguments, or else a field still readable in the server's cut text.
    let text = |key: &str| match args {
        Some(args) => args.get(key).and_then(Value::as_str).map(str::to_string),
        None => field_in_cut_json(&p.arguments, key),
    };
    let mut out = Vec::new();
    let body = |out: &mut Vec<String>, key: &str, prefix: &str| match text(key) {
        Some(value) => add_lines(out, &value, prefix),
        None if cut_by_server => out.push(format!("({key} too long to show here)")),
        None => {}
    };
    match p.tool.as_str() {
        "write" | "edit" => {
            out.push(match text("path") {
                Some(path) => escape(&path),
                None => "(path not available)".into(),
            });
            if p.tool == "write" {
                body(&mut out, "content", "+ ");
            } else {
                body(&mut out, "old_string", "- ");
                body(&mut out, "new_string", "+ ");
            }
        }
        "bash" => body(&mut out, "command", "$ "),
        "fetch" => match text("url") {
            Some(url) => out.push(escape(&url)),
            None => out.push("(url not available)".into()),
        },
        _ => {}
    }
    if out.is_empty() && !p.arguments.is_empty() {
        add_lines(&mut out, &p.arguments, "");
        if cut_by_server {
            out.push("(arguments shortened by the server)".into());
        }
    }
    out
}

/// The string value of `"key":` in JSON text the server cut in the middle, if that part survived.
fn field_in_cut_json(text: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":");
    let at = text.find(&needle)? + needle.len();
    let mut values =
        serde_json::Deserializer::from_str(text[at..].trim_start()).into_iter::<String>();
    values.next()?.ok()
}

/// Adds the first lines of `text` with `prefix`, and how many more there are. A very long line is
/// shortened first, so cutting it to the screen stays cheap.
fn add_lines(out: &mut Vec<String>, text: &str, prefix: &str) {
    let all: Vec<&str> = text.lines().collect();
    for line in all.iter().take(PREVIEW_LINES) {
        let mut shown = escape(line);
        if let Some((at, _)) = shown.char_indices().nth(PREVIEW_LINE_CHARS) {
            shown.truncate(at);
            shown.push('…');
        }
        out.push(format!("{prefix}{shown}"));
    }
    if all.len() > PREVIEW_LINES {
        out.push(format!("  … {} more lines", all.len() - PREVIEW_LINES));
    }
}

/// `text` with control characters written out (`\u{1b}`), so the call's text cannot drive the
/// terminal or hide what follows; tabs become spaces.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\t' => out.push_str("    "),
            c if c.is_control() => out.extend(c.escape_unicode()),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod copy_tests {
    use super::*;
    #[test]
    fn complete_always_rules_are_wrapped_and_copied() {
        let mut p = test_prompt();
        p.resolution = None;
        let label = "Always (project): bash git push *; bash cargo test *; external_directory /a/very/long/path/*";
        p.always_label = Some(label.into());
        let rendered = lines(&p, None, Keys::Ready, &Theme::default(), 24);
        let joined = rendered
            .iter()
            .map(|line| line.to_string())
            .collect::<String>();
        assert!(joined.contains(label));
        assert!(rendered.iter().all(|line| line.width() <= 24));
        assert!(copy_sections(&p, None, false).join("\n").contains(label));
        insta::assert_snapshot!(rendered.iter().map(|line| line.to_string()).collect::<Vec<_>>().join("\n"), @r"
        PERMISSION · build want…
         long reason
         $ echo long command
        Always (project): bash g
        it push *; bash cargo te
        st *; external_directory
         /a/very/long/path/*
         o once   a Always   r …
        ");
    }
    #[test]
    fn outcomes_and_write_content_are_explicit_and_bounded() {
        let mut p = test_prompt();
        p.child_session_id = Some("child".into());
        for (resolution, expected) in [
            (None, "waiting for your answer"),
            (Some("deny"), "denied: do not run"),
            (Some("expired"), "expired: the session restarted"),
        ] {
            p.resolution = resolution.map(str::to_owned);
            p.message = Some("do not run".into());
            assert_eq!(copy_sections(&p, None, false)[3], expected);
        }
        p.resolution = None;
        assert_eq!(
            copy_sections(&p, None, true)[3],
            "sent, waiting for the server"
        );
        p.tool = "write".into();
        let args = serde_json::json!({"path":"/tmp/full-path","content":"a".repeat(5000)});
        let text = &copy_sections(&p, Some(&args), false)[2];
        assert!(text.contains("/tmp/full-path") && text.contains("904 bytes omitted"));
    }
}

#[cfg(test)]
#[test]
fn an_unsaved_always_says_it_ran_once() {
    let mut prompt = test_prompt();
    prompt.resolution = Some("allow_always".into());
    prompt.always_saved = false;
    prompt.message = Some("database is locked".into());
    let line = outcome(&prompt, "allow_always", &Theme::default()).to_string();
    assert_eq!(line, " Always not saved: database is locked; ran once");
    prompt.always_saved = true;
    prompt.message = None;
    let line = outcome(&prompt, "allow_always", &Theme::default()).to_string();
    assert_eq!(line, " allowed always");
}
