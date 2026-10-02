use crate::tui::theme::{Theme, ThemeId};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span, Text},
};
use std::collections::HashMap;
use tui_markdown::{AlertKind, BuiltinCodeTheme, Options, StyleSheet};

/// Markdown styles taken from the theme roles. No style sets a background, so the panel colour
/// shows through and a selected block keeps its tint.
#[derive(Debug, Clone)]
struct ThemeStyles(Theme);

impl StyleSheet for ThemeStyles {
    fn heading(&self, level: u8) -> Style {
        let base = Style::new()
            .fg(self.0.accent_text)
            .add_modifier(Modifier::BOLD);
        match level {
            1 => base.add_modifier(Modifier::UNDERLINED),
            2 => base,
            _ => base.add_modifier(Modifier::ITALIC),
        }
    }
    fn code(&self) -> Style {
        self.0.code_ref()
    }
    fn link(&self) -> Style {
        self.0.code_ref().add_modifier(Modifier::UNDERLINED)
    }
    fn blockquote(&self) -> Style {
        self.0.muted().add_modifier(Modifier::ITALIC)
    }
    fn heading_meta(&self) -> Style {
        self.0.muted()
    }
    fn metadata_block(&self) -> Style {
        self.0.muted()
    }
    fn html(&self) -> Style {
        self.0.muted()
    }
    fn math_inline(&self) -> Style {
        Style::new()
            .fg(self.0.purple)
            .add_modifier(Modifier::ITALIC)
    }
    fn math_display(&self) -> Style {
        Style::new().fg(self.0.purple)
    }
    fn footnote_ref(&self) -> Style {
        self.0.muted().add_modifier(Modifier::ITALIC)
    }
    fn footnote_def(&self) -> Style {
        self.0.muted()
    }
    fn alert(&self, kind: AlertKind) -> Style {
        Style::new().fg(match kind {
            AlertKind::Note => self.0.blue,
            AlertKind::Tip => self.0.green,
            AlertKind::Important => self.0.purple,
            AlertKind::Warning => self.0.orange,
            AlertKind::Caution => self.0.red,
        })
    }
    fn table_header(&self) -> Style {
        self.0.title()
    }
    fn table_border(&self) -> Style {
        self.0.muted()
    }
    fn list_marker(&self) -> Style {
        self.0.accent()
    }
    fn image_alt(&self) -> Style {
        self.0.muted().add_modifier(Modifier::ITALIC)
    }
}

fn options(theme: Theme) -> Options<ThemeStyles> {
    let code = if theme.id.is_light() {
        BuiltinCodeTheme::InspiredGitHub
    } else {
        BuiltinCodeTheme::Base16OceanDark
    };
    Options::new(ThemeStyles(theme)).code_theme(code)
}

#[derive(Debug, Default)]
pub struct MarkdownCache {
    entries: HashMap<(String, u64, u16), Text<'static>>,
    /// Options for the current theme and table width; rebuilt only when either changes, since
    /// they hold a copy of the syntax-highlighting theme.
    options: Option<(ThemeId, u16, Options<ThemeStyles>)>,
    theme: Theme,
}

impl MarkdownCache {
    /// Uses `theme` for the next parses; a change of theme drops every cached block.
    pub fn set_theme(&mut self, theme: Theme) {
        if self.theme != theme {
            self.entries.clear();
            self.theme = theme;
        }
    }

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
            let current = (self.theme.id, width);
            if self.options.as_ref().map(|(id, w, _)| (*id, *w)) != Some(current) {
                self.options = Some((self.theme.id, width, options(self.theme).table_width(width)));
            }
            let options = &self.options.as_ref().expect("set above").2;
            let parsed = tui_markdown::from_str_with_options(&text, options);
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
