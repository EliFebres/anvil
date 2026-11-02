//! The one palette the UI draws with.
//!
//! Every color in the UI comes from [`Theme`]; nothing else names a
//! color. The palette is truecolor, mapped to the nearest of the 256
//! indexed colors when the terminal has no truecolor, and reduced to bold
//! and dim under `NO_COLOR`.

use ratatui::style::{Color, Modifier, Style};

/// How many colors the terminal can show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Depth {
    /// 24-bit color.
    TrueColor,
    /// The 256 indexed colors.
    Ansi256,
    /// No color at all: bold and dim only.
    Mono,
}

/// A foreground role in the palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shade {
    /// Primary text.
    Text,
    /// Secondary text: labels, tool names, answers.
    Secondary,
    /// Muted text: notices, summaries, hints.
    Muted,
    /// Faint text: ids, placeholders, key hints.
    Faint,
    /// The accent: state names, running tools, the logo.
    Accent,
    /// The secondary accent: the user's label, inline code, modes.
    Accent2,
    /// Success: finished tools, kept steps, allow.
    Success,
    /// Danger: failures, removed lines, deny.
    Danger,
}

/// The palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    depth: Depth,
    /// The screen background.
    pub bg: Color,
    /// The raised surface: status bar, question bar, modals.
    pub surface: Color,
    /// Borders and rules.
    pub border: Color,
    /// Primary text.
    pub text: Color,
    /// Secondary text.
    pub secondary: Color,
    /// Muted text.
    pub muted: Color,
    /// Faint text.
    pub faint: Color,
    /// The accent.
    pub accent: Color,
    /// The secondary accent.
    pub accent2: Color,
    /// Success.
    pub success: Color,
    /// Danger.
    pub danger: Color,
    /// The background of a removed diff line.
    pub removed_bg: Color,
    /// The background of an added diff line.
    pub added_bg: Color,
}

/// The palette as RGB triples, in field order from `bg` to `added_bg`.
const PALETTE: [(u8, u8, u8); 13] = [
    (0x0f, 0x11, 0x15),
    (0x16, 0x1a, 0x22),
    (0x2a, 0x2f, 0x3a),
    (0xd6, 0xd9, 0xe0),
    (0xb9, 0xbf, 0xca),
    (0x7a, 0x81, 0x90),
    (0x55, 0x60, 0x6f),
    (0xf2, 0x8c, 0x28),
    (0x6e, 0xa8, 0xd9),
    (0x3f, 0xb9, 0x50),
    (0xff, 0x9a, 0x9a),
    (0x3a, 0x1a, 0x1c),
    (0x1a, 0x3a, 0x24),
];

impl Theme {
    /// The theme for this process: monochrome under a non-empty
    /// `NO_COLOR`, otherwise as deep as the terminal allows.
    ///
    /// # Returns
    ///
    /// The theme.
    pub fn detect() -> Self {
        if std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty()) {
            return Self::mono();
        }
        if crossterm::style::available_color_count() == u16::MAX {
            Self::truecolor()
        } else {
            Self::ansi256()
        }
    }

    /// The palette in 24-bit color.
    ///
    /// # Returns
    ///
    /// The theme.
    pub fn truecolor() -> Self {
        Self::from_colors(Depth::TrueColor, |(r, g, b)| Color::Rgb(r, g, b))
    }

    /// The palette on the nearest of the 256 indexed colors.
    ///
    /// # Returns
    ///
    /// The theme.
    pub fn ansi256() -> Self {
        Self::from_colors(Depth::Ansi256, |(r, g, b)| {
            Color::Indexed(nearest_256(r, g, b))
        })
    }

    /// No colors: bold for the accents, dim for the quiet shades.
    ///
    /// # Returns
    ///
    /// The theme.
    pub fn mono() -> Self {
        Self::from_colors(Depth::Mono, |_| Color::Reset)
    }

    /// The style for a foreground role.
    ///
    /// # Arguments
    ///
    /// * `shade` - The role.
    ///
    /// # Returns
    ///
    /// A foreground color, or bold or dim in monochrome.
    pub fn fg(&self, shade: Shade) -> Style {
        if self.depth == Depth::Mono {
            return match shade {
                Shade::Text | Shade::Secondary => Style::new(),
                Shade::Muted | Shade::Faint => Style::new().add_modifier(Modifier::DIM),
                Shade::Accent | Shade::Accent2 | Shade::Success | Shade::Danger => {
                    Style::new().add_modifier(Modifier::BOLD)
                }
            };
        }
        Style::new().fg(self.color(shade))
    }

    /// The bold style for a foreground role.
    ///
    /// # Arguments
    ///
    /// * `shade` - The role.
    ///
    /// # Returns
    ///
    /// The style.
    pub fn bold(&self, shade: Shade) -> Style {
        self.fg(shade).add_modifier(Modifier::BOLD)
    }

    /// A style moved onto the raised surface.
    ///
    /// # Arguments
    ///
    /// * `style` - The style.
    ///
    /// # Returns
    ///
    /// The style with the surface background; unchanged in monochrome.
    pub fn on_surface(&self, style: Style) -> Style {
        if self.depth == Depth::Mono {
            return style;
        }
        style.bg(self.surface)
    }

    /// The style of borders and rules.
    ///
    /// # Returns
    ///
    /// The style; dim in monochrome.
    pub fn border(&self) -> Style {
        if self.depth == Depth::Mono {
            return Style::new().add_modifier(Modifier::DIM);
        }
        Style::new().fg(self.border)
    }

    /// The same theme with every role pushed into the quiet shades, so a
    /// modal drawn over it is the only thing at full contrast.
    ///
    /// # Returns
    ///
    /// The dimmed theme.
    pub fn dimmed(&self) -> Self {
        Self {
            text: self.muted,
            secondary: self.muted,
            muted: self.faint,
            faint: self.faint,
            accent: self.muted,
            accent2: self.muted,
            success: self.muted,
            danger: self.muted,
            removed_bg: self.bg,
            added_bg: self.bg,
            ..*self
        }
    }

    /// The color of a role.
    fn color(&self, shade: Shade) -> Color {
        match shade {
            Shade::Text => self.text,
            Shade::Secondary => self.secondary,
            Shade::Muted => self.muted,
            Shade::Faint => self.faint,
            Shade::Accent => self.accent,
            Shade::Accent2 => self.accent2,
            Shade::Success => self.success,
            Shade::Danger => self.danger,
        }
    }

    /// Builds the theme from the palette through one color mapping.
    fn from_colors(depth: Depth, map: impl Fn((u8, u8, u8)) -> Color) -> Self {
        let [
            bg,
            surface,
            border,
            text,
            secondary,
            muted,
            faint,
            accent,
            accent2,
            success,
            danger,
            removed_bg,
            added_bg,
        ] = PALETTE.map(map);
        Self {
            depth,
            bg,
            surface,
            border,
            text,
            secondary,
            muted,
            faint,
            accent,
            accent2,
            success,
            danger,
            removed_bg,
            added_bg,
        }
    }
}

/// The index of the 256-color palette closest to an RGB color: the best
/// of the 6x6x6 cube and the 24-step gray ramp.
///
/// # Arguments
///
/// * `r` - Red.
/// * `g` - Green.
/// * `b` - Blue.
///
/// # Returns
///
/// The index, 16 to 255.
pub fn nearest_256(r: u8, g: u8, b: u8) -> u8 {
    const STEPS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let nearest_step = |v: u8| -> (u8, u8) {
        let mut best = (0u8, u8::MAX);
        for (index, step) in STEPS.iter().enumerate() {
            let distance = step.abs_diff(v);
            if distance < best.1 {
                best = (u8::try_from(index).unwrap_or(0), distance);
            }
        }
        best
    };
    let (ri, _) = nearest_step(r);
    let (gi, _) = nearest_step(g);
    let (bi, _) = nearest_step(b);
    let cube = (
        STEPS[usize::from(ri)],
        STEPS[usize::from(gi)],
        STEPS[usize::from(bi)],
    );
    let cube_index = 16 + 36 * ri + 6 * gi + bi;
    let average = (u16::from(r) + u16::from(g) + u16::from(b)) / 3;
    let gray_level = ((average.saturating_sub(8) + 5) / 10).min(23);
    let gray = u8::try_from(8 + 10 * gray_level).unwrap_or(238);
    let gray_index = u8::try_from(232 + gray_level).unwrap_or(255);
    let distance = |(cr, cg, cb): (u8, u8, u8)| -> u32 {
        let d = |a: u8, b: u8| u32::from(a.abs_diff(b)).pow(2);
        d(cr, r) + d(cg, g) + d(cb, b)
    };
    if distance((gray, gray, gray)) < distance(cube) {
        gray_index
    } else {
        cube_index
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHADES: [Shade; 8] = [
        Shade::Text,
        Shade::Secondary,
        Shade::Muted,
        Shade::Faint,
        Shade::Accent,
        Shade::Accent2,
        Shade::Success,
        Shade::Danger,
    ];

    #[test]
    fn mono_has_no_colors() {
        let theme = Theme::mono();
        for shade in SHADES {
            let style = theme.fg(shade);
            assert_eq!(style.fg, None, "{shade:?}");
            assert_eq!(style.bg, None, "{shade:?}");
        }
        for style in [theme.border(), theme.on_surface(Style::new())] {
            assert_eq!(style.fg, None);
            assert_eq!(style.bg, None);
        }
        assert!(
            theme
                .fg(Shade::Accent)
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert!(theme.fg(Shade::Faint).add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn truecolor_uses_the_palette_and_dimmed_drops_the_accents() {
        let theme = Theme::truecolor();
        assert_eq!(theme.accent, Color::Rgb(0xf2, 0x8c, 0x28));
        assert_eq!(
            theme.fg(Shade::Danger).fg,
            Some(Color::Rgb(0xff, 0x9a, 0x9a))
        );
        assert_eq!(theme.on_surface(Style::new()).bg, Some(theme.surface));
        let dim = theme.dimmed();
        for shade in SHADES {
            let color = dim.fg(shade).fg.expect("colored");
            assert!(
                color == theme.muted || color == theme.faint,
                "{shade:?} is {color:?}"
            );
        }
        assert_eq!(dim.removed_bg, theme.bg);
        assert_eq!(dim.added_bg, theme.bg);
        assert_eq!(dim.surface, theme.surface);
    }

    #[test]
    fn nearest_256_hits_corners_and_grays() {
        assert_eq!(nearest_256(0, 0, 0), 16);
        assert_eq!(nearest_256(255, 255, 255), 231);
        assert_eq!(nearest_256(255, 0, 0), 196);
        assert_eq!(nearest_256(0, 0, 255), 21);
        assert_eq!(nearest_256(128, 128, 128), 244);
        assert_eq!(nearest_256(0x0f, 0x11, 0x15), 233);
        let theme = Theme::ansi256();
        assert_eq!(theme.accent, Color::Indexed(nearest_256(0xf2, 0x8c, 0x28)));
        assert!(matches!(theme.fg(Shade::Text).fg, Some(Color::Indexed(_))));
    }
}
