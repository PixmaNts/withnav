use ratatui::text::{Line, Span, Text};
use std::collections::HashMap;

#[derive(Debug, Default)]
pub struct MarkdownCache {
    entries: HashMap<(String, u64, u16), Text<'static>>,
}

impl MarkdownCache {
    pub fn get(&mut self, id: &str, revision: u64, width: u16, source: &str) -> &Text<'static> {
        let key = (id.to_owned(), revision, width);
        if !self.entries.contains_key(&key) {
            self.entries
                .retain(|(block, rev, w), _| block != id || (*rev == revision && *w == width));
            let mut text: String = source
                .chars()
                .map(|c| {
                    if c.is_control() && c != '\n' && c != '\t' {
                        ' '
                    } else {
                        c
                    }
                })
                .collect();
            if source
                .lines()
                .filter(|line| line.trim_start().starts_with("```"))
                .count()
                % 2
                == 1
            {
                text.push_str("\n```");
            }
            let parsed = tui_markdown::from_str_with_options(
                &text,
                &tui_markdown::Options::default().table_width(width),
            );
            let owned = Text::from(
                parsed
                    .lines
                    .into_iter()
                    .map(|line| {
                        Line::from(
                            line.spans
                                .into_iter()
                                .map(|span| Span::styled(span.content.into_owned(), span.style))
                                .collect::<Vec<_>>(),
                        )
                        .style(line.style)
                    })
                    .collect::<Vec<_>>(),
            );
            self.entries.insert(key.clone(), owned);
        }
        &self.entries[&key]
    }
}

pub fn height(text: &Text<'_>, width: u16) -> usize {
    let width = usize::from(width.max(1));
    text.lines
        .iter()
        .map(|line| line.width().max(1).div_ceil(width))
        .sum::<usize>()
        .max(1)
}
