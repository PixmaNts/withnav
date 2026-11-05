//! The four colour themes and the semantic styles the views draw with; no layout or state
//! transitions. Role names and values follow the design tokens in `docs/design/README.md`.
use ratatui::style::{Color, Modifier, Style};

/// Share of the accent mixed over the background for selected and active rows.
pub const SELECTION_TINT: f32 = 0.15;

/// A theme the user can pick; its `id` is what the settings file stores.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ThemeId {
    AyuDark,
    AyuLight,
    NordDark,
    NordLight,
    #[default]
    TideDark,
    TideLight,
    MossDark,
    MossLight,
}

impl ThemeId {
    pub const ALL: [ThemeId; 8] = [
        ThemeId::AyuDark,
        ThemeId::AyuLight,
        ThemeId::NordDark,
        ThemeId::NordLight,
        ThemeId::TideDark,
        ThemeId::TideLight,
        ThemeId::MossDark,
        ThemeId::MossLight,
    ];

    /// The stable id written in `config.json`.
    pub fn id(self) -> &'static str {
        match self {
            ThemeId::AyuDark => "ayu-dark",
            ThemeId::AyuLight => "ayu-light",
            ThemeId::NordDark => "nord-dark",
            ThemeId::TideDark => "tide-dark",
            ThemeId::TideLight => "tide-light",
            ThemeId::MossDark => "moss-dark",
            ThemeId::MossLight => "moss-light",
            ThemeId::NordLight => "nord-light",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            ThemeId::AyuDark => "Ayu Dark",
            ThemeId::AyuLight => "Ayu Light",
            ThemeId::NordDark => "Nord Dark",
            ThemeId::TideDark => "Tide Dark",
            ThemeId::TideLight => "Tide Light",
            ThemeId::MossDark => "Moss Dark",
            ThemeId::MossLight => "Moss Light",
            ThemeId::NordLight => "Nord Light",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|theme| theme.id() == id)
    }

    pub fn is_light(self) -> bool {
        matches!(
            self,
            ThemeId::AyuLight | ThemeId::NordLight | ThemeId::TideLight | ThemeId::MossLight
        )
    }

    pub fn palette(self) -> Theme {
        match self {
            ThemeId::AyuDark => Theme::ayu_dark(),
            ThemeId::AyuLight => Theme::ayu_light(),
            ThemeId::NordDark => Theme::nord_dark(),
            ThemeId::TideDark => Theme::tide_dark(),
            ThemeId::TideLight => Theme::tide_light(),
            ThemeId::MossDark => Theme::moss_dark(),
            ThemeId::MossLight => Theme::moss_light(),
            ThemeId::NordLight => Theme::nord_light(),
        }
    }
}

/// The colour roles of one theme. `accent` paints bars, borders and glyphs; `accent_text` is the
/// accent for text, darker on light themes so it stays readable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub id: ThemeId,
    pub bg: Color,
    pub panel: Color,
    pub surface: Color,
    pub border: Color,
    pub text: Color,
    pub muted: Color,
    pub accent: Color,
    pub accent_text: Color,
    pub on_accent: Color,
    pub green: Color,
    pub red: Color,
    pub blue: Color,
    pub purple: Color,
    pub orange: Color,
    pub nav: Color,
    pub vigie: Color,
    pub lux: Color,
    pub mousse: Color,
    pub cartographe: Color,
    pub sondeur: Color,
}

const fn hex(rgb: u32) -> Color {
    Color::Rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

/// `fg` laid over `bg` at `alpha` (0 = only `bg`, 1 = only `fg`). Terminals have no alpha
/// channel, so a tint is computed here; a non-RGB colour returns `fg` unchanged.
pub fn blend(fg: Color, bg: Color, alpha: f32) -> Color {
    let (Color::Rgb(fr, fg_, fb), Color::Rgb(br, bg_, bb)) = (fg, bg) else {
        return fg;
    };
    let alpha = alpha.clamp(0.0, 1.0);
    let mix = |f: u8, b: u8| (f32::from(b) + (f32::from(f) - f32::from(b)) * alpha).round() as u8;
    Color::Rgb(mix(fr, br), mix(fg_, bg_), mix(fb, bb))
}

impl Default for Theme {
    fn default() -> Self {
        Self::tide_dark()
    }
}

impl Theme {
    pub const fn ayu_dark() -> Self {
        Self {
            id: ThemeId::AyuDark,
            bg: hex(0x0b0e14),
            panel: hex(0x0f131a),
            surface: hex(0x151a23),
            border: hex(0x232a36),
            text: hex(0xbfbdb6),
            muted: hex(0x7b8494),
            accent: hex(0xe6b450),
            accent_text: hex(0xe6b450),
            on_accent: hex(0x0b0e14),
            green: hex(0xaad94c),
            red: hex(0xf07178),
            blue: hex(0x59c2ff),
            purple: hex(0xd2a6ff),
            orange: hex(0xff8f40),
            nav: hex(0x5ecaed),
            vigie: hex(0x72d8b3),
            lux: hex(0xf8ca65),
            mousse: hex(0xf89177),
            cartographe: hex(0xdca3e5),
            sondeur: hex(0xa3b3f8),
        }
    }

    pub const fn ayu_light() -> Self {
        Self {
            id: ThemeId::AyuLight,
            bg: hex(0xfcfcfc),
            panel: hex(0xf3f4f5),
            surface: hex(0xffffff),
            border: hex(0xdfe2e6),
            text: hex(0x4a5058),
            muted: hex(0x6e7781),
            accent: hex(0xffaa33),
            accent_text: hex(0xa35f00),
            on_accent: hex(0x1f2328),
            green: hex(0x5c8a00),
            red: hex(0xd94f4f),
            blue: hex(0x1a7bbf),
            purple: hex(0x8052b8),
            orange: hex(0xc25d00),
            nav: hex(0x036b9a),
            vigie: hex(0x107460),
            lux: hex(0x8f5d04),
            mousse: hex(0xb14328),
            cartographe: hex(0x85468f),
            sondeur: hex(0x4d5aac),
        }
    }

    pub const fn nord_dark() -> Self {
        Self {
            id: ThemeId::NordDark,
            bg: hex(0x2e3440),
            panel: hex(0x292e39),
            surface: hex(0x3b4252),
            border: hex(0x434c5e),
            text: hex(0xd8dee9),
            muted: hex(0x8f9bb3),
            accent: hex(0x88c0d0),
            accent_text: hex(0x88c0d0),
            on_accent: hex(0x2e3440),
            green: hex(0xa3be8c),
            red: hex(0xbf616a),
            blue: hex(0x81a1c1),
            purple: hex(0xb48ead),
            orange: hex(0xd08770),
            nav: hex(0x5ecaed),
            vigie: hex(0x72d8b3),
            lux: hex(0xf8ca65),
            mousse: hex(0xf89177),
            cartographe: hex(0xdca3e5),
            sondeur: hex(0xa3b3f8),
        }
    }

    pub const fn nord_light() -> Self {
        Self {
            id: ThemeId::NordLight,
            bg: hex(0xeceff4),
            panel: hex(0xe5e9f0),
            surface: hex(0xffffff),
            border: hex(0xd0d7e3),
            text: hex(0x2e3440),
            muted: hex(0x586174),
            accent: hex(0x5e81ac),
            accent_text: hex(0x3f6591),
            on_accent: hex(0xeceff4),
            green: hex(0x4f7a3c),
            red: hex(0xb0424d),
            blue: hex(0x3f6591),
            purple: hex(0x7d5a78),
            orange: hex(0xa85a3f),
            nav: hex(0x036b9a),
            vigie: hex(0x107460),
            lux: hex(0x8f5d04),
            mousse: hex(0xb14328),
            cartographe: hex(0x85468f),
            sondeur: hex(0x4d5aac),
        }
    }

    pub const fn tide_dark() -> Self {
        Self {
            id: ThemeId::TideDark,
            bg: hex(0x061115),
            panel: hex(0x0a171b),
            surface: hex(0x101e23),
            border: hex(0x213135),
            text: hex(0xd5e1e1),
            muted: hex(0x87999e),
            accent: hex(0x5ecbcb),
            accent_text: hex(0x5ecbcb),
            on_accent: hex(0x061115),
            green: hex(0x81d584),
            red: hex(0xee6570),
            blue: hex(0x6cb2ec),
            purple: hex(0xb59be6),
            orange: hex(0xf69370),
            nav: hex(0x5ecaed),
            vigie: hex(0x72d8b3),
            lux: hex(0xf8ca65),
            mousse: hex(0xf89177),
            cartographe: hex(0xdca3e5),
            sondeur: hex(0xa3b3f8),
        }
    }

    pub const fn tide_light() -> Self {
        Self {
            id: ThemeId::TideLight,
            bg: hex(0xf2f8f8),
            panel: hex(0xe9f2f3),
            surface: hex(0xfbfefe),
            border: hex(0xcddbdc),
            text: hex(0x1f3137),
            muted: hex(0x53676c),
            accent: hex(0x1d9999),
            accent_text: hex(0x00696f),
            on_accent: hex(0x081e24),
            green: hex(0x2e7c35),
            red: hex(0xc53443),
            blue: hex(0x0f68a2),
            purple: hex(0x6f4fa1),
            orange: hex(0xc1552c),
            nav: hex(0x036b9a),
            vigie: hex(0x107460),
            lux: hex(0x8f5d04),
            mousse: hex(0xb14328),
            cartographe: hex(0x85468f),
            sondeur: hex(0x4d5aac),
        }
    }

    pub const fn moss_dark() -> Self {
        Self {
            id: ThemeId::MossDark,
            bg: hex(0x0b100c),
            panel: hex(0x101511),
            surface: hex(0x161d17),
            border: hex(0x262e27),
            text: hex(0xe6e1cf),
            muted: hex(0x909487),
            accent: hex(0x75d78d),
            accent_text: hex(0x75d78d),
            on_accent: hex(0x0b100c),
            green: hex(0xc8df58),
            red: hex(0xea6a64),
            blue: hex(0x6eb9dc),
            purple: hex(0xbe99da),
            orange: hex(0xebb25f),
            nav: hex(0x5ecaed),
            vigie: hex(0x72d8b3),
            lux: hex(0xf8ca65),
            mousse: hex(0xf89177),
            cartographe: hex(0xdca3e5),
            sondeur: hex(0xa3b3f8),
        }
    }

    pub const fn moss_light() -> Self {
        Self {
            id: ThemeId::MossLight,
            bg: hex(0xf7f5ee),
            panel: hex(0xefede3),
            surface: hex(0xfdfcf7),
            border: hex(0xd7d5c7),
            text: hex(0x293027),
            muted: hex(0x5f665a),
            accent: hex(0x429c5a),
            accent_text: hex(0x197037),
            on_accent: hex(0x141d16),
            green: hex(0x577600),
            red: hex(0xc13c3b),
            blue: hex(0x136b94),
            purple: hex(0x784d96),
            orange: hex(0xae6700),
            nav: hex(0x036b9a),
            vigie: hex(0x107460),
            lux: hex(0x8f5d04),
            mousse: hex(0xb14328),
            cartographe: hex(0x85468f),
            sondeur: hex(0x4d5aac),
        }
    }

    /// The main area: transcript and editor.
    pub fn base(&self) -> Style {
        Style::default().fg(self.text).bg(self.bg)
    }
    /// Header, status line and key hints.
    pub fn panel(&self) -> Style {
        Style::default().fg(self.text).bg(self.panel)
    }
    /// Popups.
    pub fn surface(&self) -> Style {
        Style::default().fg(self.text).bg(self.surface)
    }
    /// The background of a selected or active row.
    pub fn selection_bg(&self) -> Color {
        blend(self.accent, self.bg, SELECTION_TINT)
    }
    /// The same tint, laid over a panel (the tab sidebar).
    pub fn selection_bg_on_panel(&self) -> Color {
        blend(self.accent, self.panel, SELECTION_TINT)
    }
    /// The same tint, laid over a popup.
    pub fn selection_bg_on_surface(&self) -> Color {
        blend(self.accent, self.surface, SELECTION_TINT)
    }

    /// Accent-coloured text.
    pub fn accent(&self) -> Style {
        Style::default().fg(self.accent_text)
    }
    /// Accent-coloured lines and glyphs: focused borders, gutters, popup frames.
    pub fn accent_line(&self) -> Style {
        Style::default().fg(self.accent)
    }
    pub fn text(&self) -> Style {
        Style::default().fg(self.text)
    }
    pub fn muted(&self) -> Style {
        Style::default().fg(self.muted)
    }
    pub fn strong(&self) -> Style {
        Style::default().fg(self.text).add_modifier(Modifier::BOLD)
    }
    pub fn title(&self) -> Style {
        self.accent().add_modifier(Modifier::BOLD)
    }
    pub fn error(&self) -> Style {
        Style::default().fg(self.red)
    }
    pub fn warning(&self) -> Style {
        Style::default().fg(self.orange)
    }
    pub fn success(&self) -> Style {
        Style::default().fg(self.green)
    }
    /// Paths, symbols and links.
    pub fn code_ref(&self) -> Style {
        Style::default().fg(self.blue)
    }
    /// Counts, durations and sizes.
    pub fn number(&self) -> Style {
        Style::default().fg(self.orange)
    }
    pub fn subagent(&self) -> Style {
        Style::default().fg(self.purple)
    }
    pub fn selected(&self) -> Style {
        Style::default().bg(self.selection_bg())
    }
    pub fn border(&self) -> Style {
        Style::default().fg(self.border)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_theme_has_its_design_values() {
        assert_eq!(Theme::default().id, ThemeId::TideDark);
        assert_eq!(Theme::tide_dark().nav, hex(0x5ecaed));
        assert_eq!(Theme::tide_light().vigie, hex(0x107460));
        for id in ThemeId::ALL {
            let theme = id.palette();
            assert_eq!(
                theme.lux,
                hex(if id.is_light() { 0x8f5d04 } else { 0xf8ca65 })
            );
        }
        assert_eq!(Theme::ayu_dark().bg, Color::Rgb(0x0b, 0x0e, 0x14));
        assert_eq!(Theme::ayu_dark().accent, Color::Rgb(0xe6, 0xb4, 0x50));
        assert_eq!(Theme::ayu_light().accent_text, Color::Rgb(0xa3, 0x5f, 0x00));
        assert_eq!(Theme::ayu_light().bg, Color::Rgb(0xfc, 0xfc, 0xfc));
        assert_eq!(Theme::nord_dark().blue, Color::Rgb(0x81, 0xa1, 0xc1));
        assert_eq!(Theme::nord_dark().panel, Color::Rgb(0x29, 0x2e, 0x39));
        assert_eq!(Theme::nord_light().purple, Color::Rgb(0x7d, 0x5a, 0x78));
        assert_eq!(Theme::nord_light().on_accent, Color::Rgb(0xec, 0xef, 0xf4));
    }

    #[test]
    fn ids_round_trip_and_name_their_palette() {
        for theme in ThemeId::ALL {
            assert_eq!(ThemeId::from_id(theme.id()), Some(theme));
            assert_eq!(theme.palette().id, theme);
        }
        assert_eq!(ThemeId::from_id("solarized"), None);
        assert_eq!(ThemeId::default(), ThemeId::TideDark);
        assert!(ThemeId::NordLight.is_light() && !ThemeId::NordDark.is_light());
    }

    #[test]
    fn blend_mixes_each_channel() {
        let white = Color::Rgb(255, 255, 255);
        let black = Color::Rgb(0, 0, 0);
        assert_eq!(blend(white, black, 0.0), black);
        assert_eq!(blend(white, black, 1.0), white);
        assert_eq!(blend(white, black, 0.5), Color::Rgb(128, 128, 128));
        assert_eq!(
            blend(Color::Rgb(200, 100, 0), Color::Rgb(0, 100, 200), 0.25),
            Color::Rgb(50, 100, 150)
        );
        assert_eq!(blend(Color::Red, black, 0.5), Color::Red);
        // Ayu Dark selection: #e6b450 at 15 % over #0b0e14.
        assert_eq!(
            Theme::ayu_dark().selection_bg(),
            Color::Rgb(0x2c, 0x27, 0x1d)
        );
    }
}
