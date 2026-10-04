use super::{markdown::MarkdownCache, permission::Keys, tool};
use crate::tui::{
    state::{Block, BlockKind, State, UiState},
    theme::Theme,
};
use ratatui::{
    layout::Rect,
    style::Modifier,
    text::{Line, Span, Text},
    widgets::Paragraph,
    Frame,
};

fn clean(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_control() && c != '\n' && c != '\t' {
                ' '
            } else {
                c
            }
        })
        .collect()
}

/// Who a message came from, classified from the actor id in `message.received.source`
/// (`user:<client>`, `navigator[:<id>]`).
#[derive(PartialEq)]
enum Origin {
    User,
    Navigator,
    Assistant,
    Subagent,
    Other,
}

impl Origin {
    fn of(source: &str) -> Self {
        if source == "user:subagent" || source == "subagent" || source.starts_with("subagent:") {
            Self::Subagent
        } else if source == "user" || source.starts_with("user:") {
            Self::User
        } else if source == "navigator" || source.starts_with("navigator:") {
            Self::Navigator
        } else if source == "assistant" {
            Self::Assistant
        } else {
            Self::Other
        }
    }
}

fn block_text(
    block: &Block,
    state: &State,
    ui: &UiState,
    theme: &Theme,
    cache: &mut MarkdownCache,
    width: u16,
) -> Text<'static> {
    let expanded = ui.expanded.contains(&block.id);
    let mut lines: Vec<Line<'static>> = Vec::new();
    match &block.kind {
        BlockKind::Message { source, text } => {
            let (name, style) = match Origin::of(source) {
                Origin::User => ("YOU".to_string(), theme.muted()),
                Origin::Navigator => ("NAVIGATOR".to_string(), theme.error()),
                Origin::Assistant => ("ASSISTANT".to_string(), theme.accent()),
                Origin::Subagent => (
                    "SUBAGENT OUTPUT · untrusted data".to_string(),
                    theme.subagent(),
                ),
                Origin::Other => (clean(source), theme.subagent()),
            };
            lines.push(Line::styled(name, style.add_modifier(Modifier::BOLD)));
            if Origin::of(source) == Origin::Assistant {
                let parsed = cache.get(&block.id, 0, width.saturating_sub(2), text);
                lines.extend(parsed.lines.iter().cloned().map(|line| {
                    Line::from(
                        std::iter::once(Span::raw(" "))
                            .chain(line.spans)
                            .collect::<Vec<_>>(),
                    )
                    .style(line.style)
                }));
            } else {
                lines.extend(
                    clean(text)
                        .lines()
                        .map(|line| Line::raw(format!(" {line}"))),
                );
            }
        }
        BlockKind::AssistantStep {
            text,
            thinking,
            revision,
        } => {
            if text.is_empty() && thinking.is_empty() {
                return Text::default();
            }
            lines.push(Line::styled("ASSISTANT", theme.title()));
            if !thinking.is_empty() {
                let reasoning = theme.muted().add_modifier(Modifier::ITALIC);
                lines.push(Line::styled(
                    if expanded {
                        "▾ Reasoning"
                    } else {
                        "▸ Reasoning"
                    },
                    theme.muted(),
                ));
                if expanded {
                    lines.extend(
                        clean(thinking)
                            .lines()
                            .map(|line| Line::styled(format!("  {line}"), reasoning)),
                    );
                }
            }
            if !text.is_empty() {
                let parsed = cache.get(&block.id, *revision, width.saturating_sub(2), text);
                lines.extend(parsed.lines.iter().cloned().map(|line| {
                    Line::from(
                        std::iter::once(Span::raw(" "))
                            .chain(line.spans)
                            .collect::<Vec<_>>(),
                    )
                    .style(line.style)
                }));
            }
        }
        BlockKind::ToolCall { .. } => {
            let mut header = tool::header(&block.kind, ui.tick, theme);
            header.spans.insert(
                0,
                Span::styled(if expanded { "▾ " } else { "▸ " }, theme.muted()),
            );
            lines.push(header);
            if expanded {
                lines.extend(tool::output(&block.kind, theme));
            }
        }
        BlockKind::YieldQuestion(question) => {
            lines.push(Line::styled(
                "QUESTION · awaiting answer",
                theme.warning().add_modifier(Modifier::BOLD),
            ));
            lines.extend(clean(question).lines().map(|s| Line::raw(format!(" {s}"))));
        }
        BlockKind::Error(error) => {
            lines.push(Line::styled(
                "ERROR",
                theme.error().add_modifier(Modifier::BOLD),
            ));
            lines.extend(
                clean(error)
                    .lines()
                    .map(|s| Line::styled(format!(" {s}"), theme.error())),
            );
        }
        BlockKind::Command { name, ok, text } => {
            let (mark, style) = if *ok {
                ("ok", theme.success())
            } else {
                ("error", theme.error())
            };
            lines.push(Line::from(vec![
                Span::styled(
                    format!("/{name}"),
                    theme.muted().add_modifier(Modifier::BOLD),
                ),
                Span::styled(" · ", theme.muted()),
                Span::styled(mark, style),
            ]));
            let body = if *ok { theme.text() } else { theme.error() };
            lines.extend(
                clean(text)
                    .lines()
                    .map(|s| Line::styled(format!(" {s}"), body)),
            );
        }
        BlockKind::Permission(prompt) => {
            let open = state.open_permission().map(|p| p.request_id.as_str())
                == Some(prompt.request_id.as_str());
            let keys = if !open {
                Keys::Inactive
            } else if state.answered_permission.as_deref() == Some(prompt.request_id.as_str()) {
                Keys::Sent
            } else if let Some(message) = ui.deny_draft(&prompt.request_id) {
                Keys::Denying(message)
            } else {
                Keys::Ready
            };
            let tool_id = format!("tool:{}", prompt.tool_call_id);
            let call = state
                .conversation
                .blocks
                .iter()
                .find_map(|b| match &b.kind {
                    BlockKind::ToolCall { args, .. }
                        if b.id == tool_id && prompt.child_session_id.is_none() =>
                    {
                        Some(args)
                    }
                    _ => None,
                });
            lines.extend(super::permission::lines(
                prompt,
                call,
                keys,
                theme,
                usize::from(width.saturating_sub(2)),
            ));
        }
        BlockKind::Notice(notice) => {
            lines.push(Line::styled(
                "NOTICE",
                theme.muted().add_modifier(Modifier::BOLD),
            ));
            lines.extend(
                clean(notice)
                    .lines()
                    .map(|s| Line::styled(format!(" {s}"), theme.muted())),
            );
        }
    }
    if lines.is_empty() {
        lines.push(Line::raw(" "));
    }
    Text::from(lines)
}

/// Draws the transcript; returns the open permission request whose keys line is on screen.
pub fn draw(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &State,
    ui: &UiState,
    theme: &Theme,
    cache: &mut MarkdownCache,
    map: &mut super::selection::TranscriptMap,
) -> Option<String> {
    if area.width < 4 || area.height == 0 {
        return None;
    }
    let open = state.open_permission().map(|p| p.request_id.as_str());
    let mut shown = None;
    let width = area.width.saturating_sub(3);
    map.area = Rect::new(area.x + 3, area.y, width, area.height);
    let blocks: Vec<_> = state
        .conversation
        .blocks
        .iter()
        .map(|b| {
            if matches!(&b.kind, BlockKind::AssistantStep { text, thinking, .. } if text.is_empty() && thinking.is_empty()) {
                return (b, std::sync::Arc::new(Vec::new()), map.text.len());
            }
            let key = format!("{width}:{:?}:{}:{}", b.kind, ui.expanded.contains(&b.id), if matches!(b.kind, BlockKind::Permission(_) | BlockKind::ToolCall { .. }) { ui.tick } else { 0 });
            if let Some((old, rows, text)) = cache.wrapped.get(&b.id).filter(|(old,_,_)| old == &key) {
                let _ = old;
                let offset = map.text.len();
                map.text.push_str(text);
                return (b, rows.clone(), offset);
            }
            let text = block_text(b, state, ui, theme, cache, width);
            let (prefix, body_start) = match &b.kind {
                BlockKind::ToolCall { .. } => (2, 1),
                BlockKind::Permission(_) => (0, usize::MAX),
                BlockKind::AssistantStep { thinking, .. } => (
                    1,
                    1 + usize::from(!thinking.is_empty())
                        + if ui.expanded.contains(&b.id) {
                            thinking.lines().count()
                        } else {
                            0
                        },
                ),
                _ => (1, 1),
            };
            let mut local = super::selection::TranscriptMap::default();
            let rows = std::sync::Arc::new(super::selection::rows(text, width, &mut local, prefix, body_start));
            cache.wrapped.insert(b.id.clone(), (key, rows.clone(), local.text.clone()));
            let offset = map.text.len();
            map.text.push_str(&local.text);
            (b, rows, offset)
        })
        .collect();
    let total: usize = blocks
        .iter()
        .map(|(_, rows, _)| rows.len() + usize::from(!rows.is_empty()))
        .sum();
    let viewport = usize::from(area.height);
    let mut start = if ui.follow_bottom {
        total.saturating_sub(viewport)
    } else {
        total
            .saturating_sub(viewport)
            .saturating_sub(usize::from(ui.scroll))
    };
    if !ui.follow_bottom {
        if let Some(selected) = &ui.selected_block {
            let mut offset = 0;
            for (block, rows, _) in &blocks {
                let height = rows.len() + usize::from(!rows.is_empty());
                if &block.id == selected {
                    if offset < start {
                        start = offset;
                    }
                    if offset + height > start + viewport {
                        start = (offset + height).saturating_sub(viewport);
                    }
                    break;
                }
                offset += height;
            }
        }
    }
    let mut cursor = 0usize;
    map.scroll = total
        .saturating_sub(viewport)
        .saturating_sub(start)
        .min(u16::MAX as usize) as u16;
    for (block, rows, offset) in blocks {
        let height = rows.len() + usize::from(!rows.is_empty());
        let from = cursor;
        cursor += height;
        if let BlockKind::Permission(p) = &block.kind {
            // The keys line is the prompt's last; a blank row follows it.
            let keys_row = cursor.saturating_sub(2);
            if open == Some(p.request_id.as_str()) && (start..start + viewport).contains(&keys_row)
            {
                shown = Some(p.request_id.clone());
            }
        }
        if cursor <= start || from >= start + viewport {
            continue;
        }
        if height == 0 {
            continue;
        }
        let skipped = start.saturating_sub(from);
        let top = from.saturating_sub(start);
        let visible = height.saturating_sub(skipped).min(viewport - top);
        let row = area.y + top as u16;
        let selected = ui.selected_block.as_deref() == Some(&block.id);
        let gutter = match &block.kind {
            BlockKind::Error(_) => theme.error(),
            BlockKind::YieldQuestion(_) => theme.warning(),
            BlockKind::Message { source, .. } => match Origin::of(source) {
                Origin::User => theme.muted(),
                Origin::Navigator => theme.error(),
                Origin::Assistant => theme.accent_line(),
                Origin::Subagent => theme.subagent(),
                Origin::Other => theme.subagent(),
            },
            BlockKind::Command { ok: false, .. } => theme.error(),
            BlockKind::Permission(p) if p.is_open() => theme.accent_line(),
            BlockKind::Permission(_) => theme.border(),
            BlockKind::Notice(_) | BlockKind::Command { .. } | BlockKind::ToolCall { .. } => {
                theme.border()
            }
            BlockKind::AssistantStep { .. } => theme.accent_line(),
        };
        if selected {
            frame.buffer_mut().set_style(
                Rect::new(area.x, row, area.width, visible as u16),
                theme.selected(),
            );
        }
        // The selected block gets a heavier bar in the accent, whatever its kind.
        let (bar, gutter) = if selected {
            ("┃", theme.accent_line())
        } else {
            ("│", gutter)
        };
        for y in row..row + visible as u16 {
            frame.buffer_mut()[(area.x + 1, y)]
                .set_symbol(bar)
                .set_style(gutter);
        }
        for (dy, text_row) in rows.iter().skip(skipped).take(visible).enumerate() {
            let y = row + dy as u16;
            frame.render_widget(
                Paragraph::new(text_row.line.clone()),
                Rect::new(area.x + 3, y, width, 1),
            );
            map.cells.extend(
                text_row
                    .cells
                    .iter()
                    .copied()
                    .map(|(x, a, b)| (area.x + 3 + x, y, a + offset, b + offset)),
            );
        }
    }
    shown
}

/// Full source of the focused block, never a truncated tool preview or UI label.
pub(crate) fn copy_block(block: &Block) -> Option<String> {
    let text = match &block.kind {
        BlockKind::Message { text, .. }
        | BlockKind::AssistantStep { text, .. }
        | BlockKind::Command { text, .. } => text,
        BlockKind::ToolCall { output, .. } => output,
        BlockKind::YieldQuestion(text) | BlockKind::Error(text) | BlockKind::Notice(text) => text,
        BlockKind::Permission(_) => return None,
    };
    Some(clean(text))
}

#[cfg(test)]
mod selection_tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    #[test]
    fn copy_full_source_and_map_narrow_transcript_only() {
        let source = "Hello **world**\n\n```rust\n  println!(\"界\");\n```";
        let block = Block {
            id: "reply".into(),
            kind: BlockKind::Message {
                source: "assistant".into(),
                text: source.into(),
            },
        };
        assert_eq!(copy_block(&block).as_deref(), Some(source));
        let mut state = State::default();
        state.conversation.blocks.push(block);
        let ui = UiState {
            follow_bottom: true,
            ..Default::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(70, 25)).unwrap();
        let mut cache = MarkdownCache::default();
        let mut map = super::super::selection::TranscriptMap::default();
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    Rect::new(25, 2, 40, 20),
                    &state,
                    &ui,
                    &ui.theme.palette(),
                    &mut cache,
                    &mut map,
                );
            })
            .unwrap();
        assert!(map.text.contains("Hello world"));
        assert!(map.text.contains("  println!(\"界\");"));
        assert!(!map.text.contains("ASSISTANT"));
        assert!(!map.text.contains("```"));
        assert!(map
            .cells
            .iter()
            .all(|(x, y, _, _)| *x >= 28 && *x < 65 && *y >= 2 && *y < 22));
        assert!(map.hit(10, 3).is_none());
    }

    #[test]
    fn tool_copy_is_not_limited_to_preview() {
        // A block copy is the complete output even when the view only shows a preview.
        let output = (0..40)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut block = Block {
            id: "tool".into(),
            kind: BlockKind::ToolCall {
                id: "call".into(),
                name: "bash".into(),
                args: serde_json::Value::Null,
                status: None,
                output: output.clone(),
                metadata: serde_json::Value::Null,
                duration_ms: None,
                exit_code: None,
            },
        };
        assert_eq!(copy_block(&block), Some(output));
        block.kind = BlockKind::Notice("\x1b[31munsafe".into());
        assert!(!copy_block(&block).unwrap().contains('\x1b'));
    }
}
