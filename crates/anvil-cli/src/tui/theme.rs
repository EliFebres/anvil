//! One theme that reads on dark and light terminals.
//!
//! Only state banners, user messages, tool failures and the modal border
//! carry a color. Everything else uses the terminal's default foreground
//! with bold or dim, so `NO_COLOR` loses nothing but the accents.

use ratatui::style::{Color, Modifier, Style};

/// The styles the UI draws with.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    /// State banners.
    pub state: Style,
    /// The user's own messages.
    pub user: Style,
    /// Failed tool calls and error notices.
    pub failure: Style,
    /// The border of a prompt.
    pub modal_border: Style,
    /// Secondary text: notices, borders, tool summaries.
    pub dim: Style,
    /// Inline and fenced code.
    pub code: Style,
    /// Markdown headings and the plan title.
    pub heading: Style,
    /// The input prompt prefix.
    pub prompt: Style,
    /// The status line.
    pub status: Style,
}

impl Theme {
    /// The theme for this process: monochrome when `NO_COLOR` is set to a
    /// non-empty value, the accented theme otherwise.
    ///
    /// # Returns
    ///
    /// The theme.
    pub fn detect() -> Self {
        match std::env::var_os("NO_COLOR") {
            Some(value) if !value.is_empty() => Self::mono(),
            _ => Self::color(),
        }
    }

    /// The accented theme, using only the eight basic ANSI colors and no
    /// backgrounds so it reads on either terminal background.
    ///
    /// # Returns
    ///
    /// The theme.
    pub fn color() -> Self {
        Self {
            state: Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            user: Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
            failure: Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
            modal_border: Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ..Self::mono()
        }
    }

    /// The monochrome theme: bold, dim and underline only.
    ///
    /// # Returns
    ///
    /// The theme.
    pub fn mono() -> Self {
        Self {
            state: Style::new().add_modifier(Modifier::BOLD),
            user: Style::new().add_modifier(Modifier::BOLD),
            failure: Style::new().add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            modal_border: Style::new().add_modifier(Modifier::BOLD),
            dim: Style::new().add_modifier(Modifier::DIM),
            code: Style::new().add_modifier(Modifier::BOLD),
            heading: Style::new().add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            prompt: Style::new().add_modifier(Modifier::BOLD),
            status: Style::new().add_modifier(Modifier::REVERSED),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mono_uses_no_colors() {
        let theme = Theme::mono();
        for style in [
            theme.state,
            theme.user,
            theme.failure,
            theme.modal_border,
            theme.dim,
            theme.code,
            theme.heading,
            theme.prompt,
            theme.status,
        ] {
            assert_eq!(style.fg, None);
            assert_eq!(style.bg, None);
        }
    }

    #[test]
    fn color_accents_only_the_accent_styles() {
        let theme = Theme::color();
        assert!(theme.state.fg.is_some());
        assert!(theme.user.fg.is_some());
        assert!(theme.failure.fg.is_some());
        assert!(theme.modal_border.fg.is_some());
        for style in [
            theme.dim,
            theme.code,
            theme.heading,
            theme.prompt,
            theme.status,
        ] {
            assert_eq!(style.fg, None);
            assert_eq!(style.bg, None);
        }
    }
}
