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
        } else if matches!(source, "user" | "driver" | "captain")
            || source.starts_with("captain:")
            || crate::bus::command::is_captain_source(source)
        {
            Self::User
        } else if matches!(source, "navigator" | "metacognition" | "nav")
            || source.starts_with("navigator:")
        {
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
                Origin::User => ("▸ Captain".to_string(), theme.text()),
                Origin::Navigator => ("Beacon · Remark — ◎ Nav".to_string(), theme.accent()),
                Origin::Assistant => (
                    super::sidebar::agent_label(state.agent_name()),
                    super::sidebar::agent_style(state.agent_name(), theme),
                ),
                Origin::Subagent => (
                    format!(
                        "{} · untrusted data",
                        super::sidebar::agent_label(
                            source
                                .strip_prefix("subagent:")
                                .unwrap_or("subagent output")
                        )
                    ),
                    super::sidebar::agent_style(
                        source.strip_prefix("subagent:").unwrap_or("nav"),
                        theme,
                    ),
                ),
                Origin::Other => (
                    super::sidebar::agent_label(&clean(source)),
                    super::sidebar::agent_style(source, theme),
                ),
            };
            lines.push(Line::styled(name, style.add_modifier(Modifier::BOLD)));
            if Origin::of(source) == Origin::Assistant {
                let parsed = cache.get(&block.id, content_key(text), width.saturating_sub(2), text);
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
            lines.push(Line::styled(
                super::sidebar::agent_label(state.agent_name()),
                super::sidebar::agent_style(state.agent_name(), theme),
            ));
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
                lines.extend(tool::approval_lines(&block.kind, theme));
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
            let keys = if state.archive() {
                Keys::Historical
            } else if !open {
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
            let key = layout_key(b, width, ui);
            let cacheable = !matches!(b.kind, BlockKind::Permission(_));
            if let Some((old, rows, text)) = cache.wrapped.get(&b.id).filter(|(old,_,_)| old == &key) {
                let _ = old;
                let offset = map.text.len();
                map.text.push_str(text);
                if offset < map.text.len() { map.senders.push((offset..map.text.len(), sender(b, state))); }
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
            let mut rows = super::selection::rows(text, width, &mut local, prefix, body_start);
            if let BlockKind::Permission(p) = &b.kind {
                let sections = permission_sections(p, state);
                let mut ranges = Vec::new();
                for section in &sections {
                    let start = local.text.len();
                    local.text.push_str(section);
                    local.text.push('\n');
                    ranges.push((start, local.text.len()));
                }
                let tail = rows.len().saturating_sub(if ui.deny_draft(&p.request_id).is_some() { 2 } else { 1 });
                for (i,row) in rows.iter_mut().enumerate() {
                    let section = if i == 0 { 0 } else if i >= tail { 3 } else if i == 1 && !p.reason.is_empty() { 1 } else { 2 };
                    let (a,b) = ranges[section];
                    row.ranges = (0..row.line.width().min(usize::from(width))).map(|x| (x as u16,1,a,b)).collect();
                }
            }
            let rows = std::sync::Arc::new(rows);
            if cacheable { cache.wrapped.insert(b.id.clone(), (key, rows.clone(), local.text.clone())); }
            let offset = map.text.len();
            map.text.push_str(&local.text);
            if offset < map.text.len() { map.senders.push((offset..map.text.len(), sender(b, state))); }
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
            for &(x, width, a, b) in &text_row.ranges {
                map.cells
                    .extend((0..width).map(|dx| (area.x + 3 + x + dx, y, a + offset, b + offset)));
            }
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
        BlockKind::ToolCall { output, .. } => {
            let mut lines = tool::approval_text(&block.kind);
            lines.push(clean(output));
            return Some(lines.join("\n"));
        }
        BlockKind::YieldQuestion(text) | BlockKind::Error(text) | BlockKind::Notice(text) => text,
        BlockKind::Permission(_) => return None,
    };
    Some(clean(text))
}

fn content_key(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

pub(crate) fn permission_sections(
    p: &crate::tui::state::PermissionPrompt,
    state: &State,
) -> [String; 4] {
    let id = format!("tool:{}", p.tool_call_id);
    let call = state
        .conversation
        .blocks
        .iter()
        .find_map(|b| match &b.kind {
            BlockKind::ToolCall { args, .. } if b.id == id && p.child_session_id.is_none() => {
                Some(args)
            }
            _ => None,
        });
    super::permission::copy_sections(
        p,
        call,
        state.answered_permission.as_deref() == Some(&p.request_id),
    )
}

fn sender(block: &Block, state: &State) -> String {
    match &block.kind {
        BlockKind::Message { source, .. } => match Origin::of(source) {
            Origin::User => "▸ Captain".into(),
            Origin::Assistant => super::sidebar::agent_label(state.agent_name()),
            Origin::Navigator => "Beacon · Remark — ◎ Nav".into(),
            Origin::Subagent => format!(
                "Subagent {} · untrusted data",
                super::sidebar::agent_label(&clean(
                    source.strip_prefix("subagent:").unwrap_or("output")
                ))
            ),
            Origin::Other => super::sidebar::agent_label(&clean(source)),
        },
        BlockKind::AssistantStep { .. } => super::sidebar::agent_label(state.agent_name()),
        BlockKind::ToolCall { name, .. } => format!("Tool {}", clean(name)),
        BlockKind::Notice(_) => "Notice".into(),
        BlockKind::Error(_) => "Error".into(),
        BlockKind::YieldQuestion(_) => super::sidebar::agent_label(state.agent_name()),
        BlockKind::Command { name, .. } => format!("/{}", clean(name)),
        BlockKind::Permission(_) => "Permission".into(),
    }
}

fn layout_key(block: &Block, width: u16, ui: &UiState) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    width.hash(&mut h);
    ui.expanded.contains(&block.id).hash(&mut h);
    std::mem::discriminant(&block.kind).hash(&mut h);
    match &block.kind {
        BlockKind::AssistantStep { revision, .. } => revision.hash(&mut h),
        BlockKind::ToolCall {
            name,
            args,
            status,
            output,
            duration_ms,
            exit_code,
            metadata,
            ..
        } => {
            name.hash(&mut h);
            metadata.get("permission_gate").hash(&mut h);
            metadata.get("guard").hash(&mut h);
            args.hash(&mut h);
            status.map(|s| s as u8).hash(&mut h);
            if status.is_none() && !args.is_null() {
                (ui.tick % 2).hash(&mut h);
            }
            duration_ms.hash(&mut h);
            exit_code.hash(&mut h);
            if ui.expanded.contains(&block.id) {
                output.hash(&mut h);
            }
        }
        BlockKind::Message { source, text } => {
            source.hash(&mut h);
            text.hash(&mut h);
        }
        BlockKind::Command { name, ok, text } => {
            name.hash(&mut h);
            ok.hash(&mut h);
            text.hash(&mut h);
        }
        BlockKind::Notice(text) | BlockKind::Error(text) | BlockKind::YieldQuestion(text) => {
            text.hash(&mut h)
        }
        BlockKind::Permission(_) => {}
    }
    h.finish().to_string()
}

#[cfg(test)]
mod selection_tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};
    #[test]
    fn permission_selection_copies_full_sections_and_sender_group() {
        let mut state = State::default();
        let mut p = super::super::permission::test_prompt();
        p.reason = "reason ".repeat(200);
        let command = "echo command ".repeat(200);
        p.arguments = serde_json::json!({"command":command}).to_string();
        state.conversation.blocks.push(Block {
            id: "permission".into(),
            kind: BlockKind::Permission(p.clone()),
        });
        state.conversation.blocks.push(Block {
            id: "reply".into(),
            kind: BlockKind::Message {
                source: "user".into(),
                text: "thanks".into(),
            },
        });
        let mut cache = MarkdownCache::default();
        let mut map = super::super::selection::TranscriptMap::default();
        let ui = UiState {
            follow_bottom: true,
            ..Default::default()
        };
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        terminal
            .draw(|f| {
                draw(
                    f,
                    Rect::new(0, 0, 60, 20),
                    &state,
                    &ui,
                    &ui.theme.palette(),
                    &mut cache,
                    &mut map,
                );
            })
            .unwrap();
        let (permission, _) = &map.senders[0];
        let first = map
            .cells
            .iter()
            .find(|(_, _, a, _)| *a == permission.start)
            .unwrap();
        let last = map
            .cells
            .iter()
            .find(|(_, _, _, b)| *b == permission.end)
            .unwrap();
        let a = map.hit(first.0, first.1).unwrap().0;
        let b = map.hit(last.0, last.1).unwrap().1;
        let selected = map.selected_text(a, b).unwrap();
        assert!(selected.contains(&p.reason) && selected.contains(&command));
        assert!(selected.ends_with("allowed once\n"));
        let all = map.selected_text(0, map.text.len()).unwrap();
        assert!(all.starts_with("Permission:\nPERMISSION"));
        assert!(all.contains("\n\n▸ Captain:\nthanks"));
        assert!(!all.contains('│'));
    }
    #[test]
    fn copy_sender_names_preserve_subagent_and_notice_origins() {
        let state = State::default();
        let block = |source: &str| Block {
            id: "x".into(),
            kind: BlockKind::Message {
                source: source.into(),
                text: "body".into(),
            },
        };
        assert_eq!(
            sender(&block("subagent:vigie"), &state),
            "Subagent ◉ vigie · untrusted data"
        );
        for source in ["driver", "captain", "captain:tui"] {
            assert_eq!(sender(&block(source), &state), "▸ Captain");
        }
        assert_eq!(
            sender(&block("navigator"), &state),
            "Beacon · Remark — ◎ Nav"
        );
        assert_eq!(sender(&block("Instinct"), &state), "◎ Instinct");
        assert_eq!(
            sender(
                &Block {
                    id: "n".into(),
                    kind: BlockKind::Notice("notice".into())
                },
                &state
            ),
            "Notice"
        );
    }

    #[test]
    fn persistent_layout_updates_replaced_content_and_running_glyph() {
        let mut state = State::default();
        state.conversation.blocks.push(Block {
            id: "message".into(),
            kind: BlockKind::Message {
                source: "assistant".into(),
                text: "first".into(),
            },
        });
        state.conversation.blocks.push(Block {
            id: "command".into(),
            kind: BlockKind::Command {
                name: "test".into(),
                ok: true,
                text: "before".into(),
            },
        });
        state.conversation.blocks.push(Block {
            id: "tool".into(),
            kind: BlockKind::ToolCall {
                id: "call".into(),
                name: "bash".into(),
                args: serde_json::json!({"command":"ls"}),
                status: None,
                output: String::new(),
                metadata: serde_json::Value::Null,
                duration_ms: None,
                exit_code: None,
            },
        });
        let mut ui = UiState {
            follow_bottom: true,
            ..Default::default()
        };
        let mut cache = MarkdownCache::default();
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        let mut paint = |state: &State, ui: &UiState| {
            let mut map = super::super::selection::TranscriptMap::default();
            terminal
                .draw(|f| {
                    draw(
                        f,
                        Rect::new(0, 0, 60, 20),
                        state,
                        ui,
                        &ui.theme.palette(),
                        &mut cache,
                        &mut map,
                    );
                })
                .unwrap();
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect::<String>()
        };
        let first = paint(&state, &ui);
        ui.tick = 1;
        assert_ne!(first, paint(&state, &ui));
        state.conversation.blocks[0].kind = BlockKind::Message {
            source: "assistant".into(),
            text: "other".into(),
        };
        state.conversation.blocks[1].kind = BlockKind::Command {
            name: "test".into(),
            ok: false,
            text: "after".into(),
        };
        let changed = paint(&state, &ui);
        assert!(
            changed.contains("other") && changed.contains("after") && changed.contains("error")
        );
        if let BlockKind::ToolCall { status, .. } = &mut state.conversation.blocks[2].kind {
            *status = Some(crate::tools::ToolStatus::Ok);
        }
        let settled = paint(&state, &ui);
        ui.tick = 2;
        assert_eq!(settled, paint(&state, &ui));
    }

    #[test]
    fn streaming_reuses_history_layout_and_ranges_are_graphemes() {
        let mut state = State::default();
        state.conversation.blocks.push(Block {
            id: "history".into(),
            kind: BlockKind::Message {
                source: "user".into(),
                text: "界".repeat(1000),
            },
        });
        state.conversation.blocks.push(Block {
            id: "live".into(),
            kind: BlockKind::AssistantStep {
                text: "one".into(),
                thinking: String::new(),
                revision: 1,
            },
        });
        let ui = UiState {
            follow_bottom: true,
            ..Default::default()
        };
        let mut cache = MarkdownCache::default();
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();
        let mut map = super::super::selection::TranscriptMap::default();
        terminal
            .draw(|f| {
                draw(
                    f,
                    Rect::new(0, 0, 60, 20),
                    &state,
                    &ui,
                    &ui.theme.palette(),
                    &mut cache,
                    &mut map,
                );
            })
            .unwrap();
        let history = cache.wrapped["history"].1.clone();
        let ranges: usize = history.iter().map(|r| r.ranges.len()).sum();
        assert_eq!(ranges, 1000);
        state.conversation.blocks[1].kind = BlockKind::AssistantStep {
            text: "two".into(),
            thinking: String::new(),
            revision: 2,
        };
        map = Default::default();
        terminal
            .draw(|f| {
                draw(
                    f,
                    Rect::new(0, 0, 60, 20),
                    &state,
                    &ui,
                    &ui.theme.palette(),
                    &mut cache,
                    &mut map,
                );
            })
            .unwrap();
        assert!(std::sync::Arc::ptr_eq(
            &history,
            &cache.wrapped["history"].1
        ));
        assert!(map.cells.len() <= 60 * 20);
    }

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
