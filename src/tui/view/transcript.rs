use super::{
    markdown::{self, MarkdownCache},
    tool,
};
use crate::tui::{
    state::{Block, BlockKind, State, UiState},
    theme::Theme,
};
use ratatui::{
    layout::Rect,
    style::Modifier,
    text::{Line, Span, Text},
    widgets::{Paragraph, Wrap},
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
    Other,
}

impl Origin {
    fn of(source: &str) -> Self {
        if source == "user" || source.starts_with("user:") {
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

pub fn draw(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &State,
    ui: &UiState,
    theme: &Theme,
    cache: &mut MarkdownCache,
) {
    if area.width < 4 || area.height == 0 {
        return;
    }
    let width = area.width.saturating_sub(3);
    let blocks: Vec<_> = state
        .conversation
        .blocks
        .iter()
        .map(|b| {
            let height = match &b.kind {
                BlockKind::Message { text, .. } if !text.is_empty() => {
                    markdown::height(cache.get(&b.id, 0, width.saturating_sub(2), text), width) + 2
                }
                BlockKind::AssistantStep {
                    text,
                    thinking,
                    revision,
                } => {
                    if text.is_empty() && thinking.is_empty() {
                        return (b, 0);
                    }
                    let body = if text.is_empty() {
                        0
                    } else {
                        markdown::height(
                            cache.get(&b.id, *revision, width.saturating_sub(2), text),
                            width,
                        )
                    };
                    2 + body
                        + usize::from(!thinking.is_empty())
                        + if ui.expanded.contains(&b.id) {
                            thinking
                                .lines()
                                .map(|line| {
                                    line.chars()
                                        .count()
                                        .max(1)
                                        .div_ceil(usize::from(width.max(1)))
                                })
                                .sum::<usize>()
                        } else {
                            0
                        }
                }
                BlockKind::ToolCall { output, .. } => {
                    2 + if ui.expanded.contains(&b.id) {
                        output
                            .lines()
                            .take(20)
                            .map(|line| {
                                line.chars()
                                    .count()
                                    .saturating_add(2)
                                    .max(1)
                                    .div_ceil(usize::from(width.max(1)))
                            })
                            .sum::<usize>()
                            + usize::from(output.lines().count() > 20)
                    } else {
                        0
                    }
                }
                BlockKind::Message { .. } => 2,
                BlockKind::YieldQuestion(s) | BlockKind::Error(s) | BlockKind::Notice(s) => {
                    2 + s
                        .lines()
                        .map(|line| {
                            line.chars()
                                .count()
                                .max(1)
                                .div_ceil(usize::from(width.max(1)))
                        })
                        .sum::<usize>()
                }
            };
            (b, height)
        })
        .collect();
    let total: usize = blocks.iter().map(|(_, h)| h).sum();
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
            for (block, height) in &blocks {
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
    for (block, height) in blocks {
        let from = cursor;
        cursor += height;
        if cursor <= start || from >= start + viewport {
            continue;
        }
        if height == 0 {
            continue;
        }
        let text = block_text(block, ui, theme, cache, width);
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
                Origin::Other => theme.subagent(),
            },
            BlockKind::Notice(_) | BlockKind::ToolCall { .. } => theme.border(),
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
        let rect = Rect::new(area.x + 3, row, width, visible as u16);
        frame.render_widget(
            Paragraph::new(text)
                .wrap(Wrap { trim: false })
                .scroll((skipped as u16, 0)),
            rect,
        );
    }
}
