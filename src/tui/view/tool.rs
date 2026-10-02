use crate::{
    tools::ToolStatus,
    tui::{state::BlockKind, theme::Theme},
};
use ratatui::{
    style::Style,
    text::{Line, Span},
};
use serde_json::Value;

pub fn summary(name: &str, args: &Value) -> String {
    let key = match name.to_lowercase().as_str() {
        "read" | "write" | "edit" | "apply_patch" => "path",
        "bash" | "shell" => "command",
        "grep" | "glob" => "pattern",
        _ => "",
    };
    let value = args.get(key).or_else(|| {
        if key == "path" {
            args.get("file_path")
        } else {
            None
        }
    });
    let raw = value
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| {
            if args.is_null() {
                "".into()
            } else {
                args.to_string()
            }
        });
    raw.chars().filter(|c| !c.is_control()).take(100).collect()
}

pub fn duration(ms: Option<u64>) -> String {
    match ms {
        None => "—".into(),
        Some(n) if n < 1000 => format!("{n} ms"),
        Some(n) => format!("{:.2} s", n as f64 / 1000.0),
    }
}

pub fn header(kind: &BlockKind, tick: u64) -> Line<'static> {
    let BlockKind::ToolCall {
        name,
        args,
        status,
        duration_ms,
        exit_code,
        ..
    } = kind
    else {
        return Line::default();
    };
    let theme = Theme;
    let (glyph, label, style) = match status {
        Some(ToolStatus::Ok) => ("✓", "ok", theme.success()),
        Some(ToolStatus::Error) => ("×", "error", theme.error()),
        Some(ToolStatus::Denied) => ("!", "denied", theme.warning()),
        Some(ToolStatus::Interrupted) => ("■", "interrupted", theme.warning()),
        None if args.is_null() => ("·", "preparing", theme.muted()),
        None => (
            if tick.is_multiple_of(2) { "◐" } else { "◓" },
            "running",
            theme.accent(),
        ),
    };
    let exit = exit_code
        .filter(|n| *n != 0)
        .map(|n| format!("  exit {n}"))
        .unwrap_or_default();
    Line::from(vec![
        Span::styled(format!("{glyph} "), style),
        Span::styled(name.clone(), theme.strong()),
        Span::raw(format!(" {}", summary(name, args))),
        Span::styled(
            format!("  {label}  {}{exit}", duration(*duration_ms)),
            style,
        ),
    ])
}

pub fn output(kind: &BlockKind) -> Vec<Line<'static>> {
    let BlockKind::ToolCall {
        output, args, name, ..
    } = kind
    else {
        return vec![];
    };
    let diff = name.contains("edit")
        || name.contains("patch")
        || output.contains("@@")
        || args.to_string().contains("@@");
    let lines: Vec<_> = output.lines().collect();
    let mut preview: Vec<Line<'static>> = lines
        .iter()
        .take(20)
        .map(|line| {
            let style = if diff && line.starts_with('+') && !line.starts_with("+++") {
                Theme.success()
            } else if diff && line.starts_with('-') && !line.starts_with("---") {
                Theme.error()
            } else {
                Style::default()
            };
            Line::from(Span::styled(format!("  {line}"), style))
        })
        .collect();
    if lines.len() > 20 {
        preview.push(Line::styled(
            format!("  … {} more lines", lines.len() - 20),
            Theme.muted(),
        ));
    }
    preview
}
