//! Transcript-only screen cells mapped to unwrapped, clipboard-safe text.
use super::text::columns;
use ratatui::{
    layout::Rect,
    text::{Line, Span, Text},
};
use unicode_segmentation::UnicodeSegmentation;

#[derive(Debug, Default)]
pub struct TranscriptMap {
    pub area: Rect,
    pub text: String,
    pub cells: Vec<(u16, u16, usize, usize)>,
    pub scroll: u16,
}

impl TranscriptMap {
    pub fn hit(&self, x: u16, y: u16) -> Option<(usize, usize)> {
        if !self.area.contains((x, y).into()) {
            return None;
        }
        self.cells
            .iter()
            .filter(|(_, row, _, _)| *row == y)
            .min_by_key(|(col, _, _, _)| col.abs_diff(x))
            .map(|(_, _, a, b)| (*a, *b))
    }
    pub fn word(&self, a: usize, b: usize) -> (usize, usize) {
        if !self.text[a..b]
            .chars()
            .any(|c| c.is_alphanumeric() || c == '_')
        {
            return (a, b);
        }
        let mut start = a;
        let mut end = b;
        for (i, c) in self.text[..a].char_indices().rev() {
            if !c.is_alphanumeric() && c != '_' {
                break;
            }
            start = i;
        }
        for (i, c) in self.text[b..].char_indices() {
            if !c.is_alphanumeric() && c != '_' {
                break;
            }
            end = b + i + c.len_utf8();
        }
        (start, end)
    }
}

#[derive(Clone, Debug)]
pub struct Row {
    pub line: Line<'static>,
    pub cells: Vec<(u16, usize, usize)>,
}

/// Use one grapheme-safe wrapping pass for both painting and hit testing.
pub fn rows(
    text: Text<'static>,
    width: u16,
    map: &mut TranscriptMap,
    prefix: usize,
    body_start: usize,
) -> Vec<Row> {
    let mut rows = Vec::new();
    for (index, line) in text.lines.into_iter().enumerate() {
        let marked = line
            .style
            .add_modifier
            .contains(ratatui::style::Modifier::HIDDEN)
            || line.spans.iter().any(|s| {
                s.style
                    .add_modifier
                    .contains(ratatui::style::Modifier::HIDDEN)
            });
        let line = Line::from(
            line.spans
                .into_iter()
                .map(|s| {
                    Span::styled(
                        s.content.replace('\t', "    "),
                        s.style.remove_modifier(ratatui::style::Modifier::HIDDEN),
                    )
                })
                .collect::<Vec<_>>(),
        )
        .style(line.style.remove_modifier(ratatui::style::Modifier::HIDDEN));
        let plain = line
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();
        let copyable = index >= body_start && !marked;
        let skip = plain
            .char_indices()
            .nth(prefix)
            .map_or(plain.len(), |(i, _)| i);
        let start = map.text.len();
        if copyable {
            map.text.push_str(&plain[skip..]);
            map.text.push('\n');
        }
        let mut row = Row {
            line: Line::default().style(line.style),
            cells: Vec::new(),
        };
        let mut col = 0;
        let mut byte = 0;
        for span in line.spans {
            for g in span.content.graphemes(true) {
                let w = columns(g).min(usize::from(width.max(1))) as u16;
                if w == 0 {
                    byte += g.len();
                    continue;
                }
                let word_start = !g.chars().all(char::is_whitespace)
                    && (byte == 0
                        || plain[..byte]
                            .chars()
                            .next_back()
                            .is_some_and(char::is_whitespace));
                let word_width = if word_start {
                    columns(plain[byte..].split_whitespace().next().unwrap_or(""))
                } else {
                    0
                };
                let wrap_word = word_width <= usize::from(width)
                    && usize::from(col) + word_width > usize::from(width);
                if col + w > width.max(1) || wrap_word {
                    rows.push(row);
                    row = Row {
                        line: Line::default().style(line.style),
                        cells: Vec::new(),
                    };
                    col = 0;
                }
                if let Some(last) = row.line.spans.last_mut().filter(|s| s.style == span.style) {
                    last.content.to_mut().push_str(g);
                } else {
                    row.line.spans.push(Span::styled(g.to_owned(), span.style));
                }
                if copyable && byte >= skip {
                    for dx in 0..w {
                        row.cells.push((
                            col + dx,
                            start + byte - skip,
                            start + byte - skip + g.len(),
                        ));
                    }
                }
                col += w;
                byte += g.len();
            }
        }
        rows.push(row);
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn literal_fences_and_ellipsis_are_content_and_tabs_match_paint() {
        use ratatui::{backend::TestBackend, widgets::Paragraph, Terminal};
        let mut map = TranscriptMap::default();
        let rows = rows(
            Text::from(" a\tb e\u{301} 👩‍💻 界\n ```literal\n … real"),
            40,
            &mut map,
            1,
            0,
        );
        assert!(map.text.contains("```literal\n… real"));
        let mut terminal = Terminal::new(TestBackend::new(40, 5)).unwrap();
        terminal
            .draw(|f| {
                for (y, row) in rows.iter().enumerate() {
                    f.render_widget(
                        Paragraph::new(row.line.clone()),
                        Rect::new(0, y as u16, 40, 1),
                    );
                }
            })
            .unwrap();
        for (y, row) in rows.iter().enumerate() {
            for &(x, a, b) in &row.cells {
                let g = &map.text[a..b];
                if x > 0
                    && row
                        .cells
                        .iter()
                        .any(|&(px, pa, pb)| px == x - 1 && pa == a && pb == b)
                {
                    continue;
                }
                assert_eq!(terminal.backend().buffer()[(x, y as u16)].symbol(), g);
            }
        }
        assert!(rows[0].line.spans.len() < 4);
    }
    #[test]
    fn wraps_without_copying_chrome_or_fences() {
        let mut map = TranscriptMap::default();
        let rows = rows(
            Text::from(vec![
                Line::raw("ASSISTANT"),
                Line::raw(" hello world"),
                Line::styled(
                    " ```rust",
                    ratatui::style::Style::default().add_modifier(ratatui::style::Modifier::HIDDEN),
                ),
                Line::raw("   x();"),
                Line::styled(
                    " ```",
                    ratatui::style::Style::default().add_modifier(ratatui::style::Modifier::HIDDEN),
                ),
            ]),
            6,
            &mut map,
            1,
            1,
        );
        assert_eq!(map.text, "hello world\n  x();\n");
        assert!(rows.len() > 5);
        map.area = Rect::new(30, 2, 6, 10);
        map.cells = vec![(31, 3, 0, 1)];
        assert_eq!(map.hit(2, 3), None);
        assert_eq!(map.hit(31, 3), Some((0, 1)));
        assert_eq!(map.word(1, 2), (0, 5));
    }

    #[test]
    fn wide_and_combining_graphemes_keep_byte_boundaries() {
        let mut map = TranscriptMap::default();
        let rows = rows(Text::from(" 界e\u{301} xyz"), 4, &mut map, 1, 0);
        assert_eq!(map.text, "界e\u{301} xyz\n");
        for row in rows {
            for (_, a, b) in row.cells {
                assert!(map.text.is_char_boundary(a));
                assert!(map.text.is_char_boundary(b));
                assert!(!map.text[a..b].is_empty());
            }
        }
    }
}
