//! The status line between the transcript and the input.

use std::time::Instant;

use std::collections::BTreeMap;

use anvil_core::agent::{Role, State};
use anvil_core::provider::Usage;
use anvil_types::Price;
use ratatui::text::{Line, Span};

use super::{PermissionMode, SessionInfo};
use anvil_tui::layout::{elapsed, fit_parts, short};
use anvil_tui::{Shade, Theme};

/// The hint next to the permission mode.
const CYCLE_HINT: &str = "shift+tab to cycle";

/// What the status line shows: state and model on the left, the
/// permission mode in the middle, usage and time on the right.
#[derive(Debug)]
pub struct StatusBar {
    state: String,
    model: String,
    permission_mode: PermissionMode,
    input: u64,
    output: u64,
    cached: u64,
    /// Prompt tokens of the latest completion, for the context percentage.
    last_prompt: u64,
    context_window: u32,
    price: Option<Price>,
    role_prices: BTreeMap<Role, Option<Price>>,
    /// Tokens per role, for pricing each with its own provider's price.
    by_role: BTreeMap<Role, Tokens>,
    started: Instant,
}

/// Tokens a role has used.
#[derive(Debug, Clone, Copy, Default)]
struct Tokens {
    input: u64,
    output: u64,
    cached: u64,
}

impl Tokens {
    /// The cost of these tokens at `price`.
    fn cost(self, price: Price) -> f64 {
        price.cost(self.input, self.output, self.cached)
    }
}

impl StatusBar {
    /// A status line for a session.
    ///
    /// # Arguments
    ///
    /// * `info` - The session.
    /// * `started` - When the session began, for the elapsed time.
    ///
    /// # Returns
    ///
    /// The status line.
    pub fn new(info: &SessionInfo, started: Instant) -> Self {
        Self {
            state: info.initial_state.name().to_uppercase(),
            model: info.model.clone(),
            permission_mode: info.permission_mode,
            input: 0,
            output: 0,
            cached: 0,
            last_prompt: 0,
            context_window: info.context_window,
            price: info.price,
            role_prices: info.role_prices.clone(),
            by_role: BTreeMap::new(),
            started,
        }
    }

    /// Shows a new state.
    ///
    /// # Arguments
    ///
    /// * `state` - The state.
    pub fn set_state(&mut self, state: State) {
        self.state = state.name().to_uppercase();
    }

    /// Shows a new model.
    ///
    /// # Arguments
    ///
    /// * `model` - The model id.
    pub fn set_model(&mut self, model: &str) {
        self.model = model.to_owned();
    }

    /// Shows a new permission mode.
    ///
    /// # Arguments
    ///
    /// * `mode` - The mode.
    pub fn set_mode(&mut self, mode: PermissionMode) {
        self.permission_mode = mode;
    }

    /// The permission mode shown.
    ///
    /// # Returns
    ///
    /// The mode.
    pub fn mode(&self) -> PermissionMode {
        self.permission_mode
    }

    /// Adds one completion's usage, to the role whose provider served it.
    ///
    /// # Arguments
    ///
    /// * `role` - The role.
    /// * `usage` - The tokens.
    pub fn record_for(&mut self, role: Role, usage: Usage) {
        let tokens = self.by_role.entry(role).or_default();
        tokens.input = tokens.input.saturating_add(usage.input);
        tokens.output = tokens.output.saturating_add(usage.output);
        tokens.cached = tokens.cached.saturating_add(usage.cached);
        self.input = self.input.saturating_add(usage.input);
        self.output = self.output.saturating_add(usage.output);
        self.cached = self.cached.saturating_add(usage.cached);
        self.last_prompt = usage.input.saturating_add(usage.cached);
    }

    /// The estimated cost so far, when the model has a price.
    ///
    /// # Returns
    ///
    /// Dollars.
    pub fn cost(&self) -> Option<f64> {
        if self.role_prices.is_empty() {
            return self
                .price
                .map(|price| price.cost(self.input, self.output, self.cached));
        }
        self.by_role.keys().try_fold(0.0, |sum, role| {
            self.role_cost(*role).map(|cost| sum + cost)
        })
    }

    /// The estimated cost of one role's turns, at the price of the provider
    /// that served them.
    ///
    /// # Arguments
    ///
    /// * `role` - The role.
    ///
    /// # Returns
    ///
    /// Dollars, or `None` when the role has no price.
    pub fn role_cost(&self, role: Role) -> Option<f64> {
        let tokens = self.by_role.get(&role).copied().unwrap_or_default();
        let price = if self.role_prices.is_empty() {
            self.price
        } else {
            self.role_prices.get(&role).copied().flatten()
        };
        price.map(|price| tokens.cost(price))
    }

    /// The cost as the status line shows it: the total, and with a planner
    /// configured the part each role that spent anything accounts for.
    fn cost_label(&self) -> Option<String> {
        let total = self.cost()?;
        if self.role_prices.is_empty() || self.by_role.len() < 2 {
            return Some(format!("${total:.2}"));
        }
        let parts: Vec<String> = self
            .by_role
            .keys()
            .filter_map(|role| Some(format!("{role} ${:.2}", self.role_cost(*role)?)))
            .collect();
        Some(format!("${total:.2} ({})", parts.join(", ")))
    }

    /// How much of the context window the latest prompt used.
    ///
    /// # Returns
    ///
    /// A percentage, capped at 100.
    pub fn context_pct(&self) -> u8 {
        if self.context_window == 0 {
            return 0;
        }
        let pct = self.last_prompt.saturating_mul(100) / u64::from(self.context_window);
        u8::try_from(pct.min(100)).unwrap_or(100)
    }

    /// The line to draw, fitted to `width`, on the raised surface.
    ///
    /// # Arguments
    ///
    /// * `width` - Columns.
    /// * `now` - The current time, for the elapsed clock.
    /// * `theme` - Styles.
    ///
    /// # Returns
    ///
    /// One styled line exactly `width` wide.
    pub fn line(&self, width: u16, now: Instant, theme: &Theme) -> Line<'static> {
        let width = usize::from(width);
        let left = vec![
            Span::styled(format!(" {}", self.state), theme.bold(Shade::Accent)),
            Span::styled(" · ", theme.fg(Shade::Muted)),
            Span::styled(self.model.clone(), theme.fg(Shade::Secondary)),
        ];
        let middle = vec![
            Span::styled(
                format!("mode: {}", self.permission_mode.label()),
                theme.fg(Shade::Accent2),
            ),
            Span::styled(format!("  {CYCLE_HINT}"), theme.fg(Shade::Faint)),
        ];
        let mut parts = vec![
            format!("ctx {}%", self.context_pct()),
            format!("in {} · out {}", short(self.input), short(self.output)),
        ];
        if let Some(cost) = self.cost_label() {
            parts.push(cost);
        }
        parts.push(elapsed(
            now.saturating_duration_since(self.started).as_secs(),
        ));
        let right = vec![Span::styled(
            format!("{} ", parts.join(" · ")),
            theme.fg(Shade::Muted),
        )];
        let spans = fit_parts(left, middle, right, width);
        Line::from(spans).style(theme.on_surface(theme.fg(Shade::Text)))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use ratatui::style::Color;

    use super::*;
    use unicode_width::UnicodeWidthStr;

    use crate::tui::SessionKind;

    fn info() -> SessionInfo {
        SessionInfo {
            kind: SessionKind::Chat,
            cwd: std::path::PathBuf::new(),
            model: "claude-sonnet-5-5".to_owned(),
            context_window: 131_072,
            price: Some(Price {
                input: 3.0,
                output: 15.0,
                cached: Some(0.3),
            }),
            role_prices: BTreeMap::new(),
            permission_mode: PermissionMode::Ask,
            initial_state: State::Build,
            slash_commands: Vec::new(),
            version: String::new(),
            coder: String::new(),
            reviewer: None,
            rules: Vec::new(),
            skills: 0,
            mcp_servers: 0,
            recent_sessions: Vec::new(),
            animate: false,
        }
    }

    fn text_of(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn status_bar_layout() {
        let started = Instant::now();
        let mut bar = StatusBar::new(&info(), started);
        bar.record_for(
            Role::Coder,
            Usage {
                input: 41_200,
                output: 3_800,
                cached: 1_000,
            },
        );
        let theme = Theme::truecolor();
        let line = bar.line(120, started + Duration::from_secs(137), &theme);
        let text = text_of(&line);
        assert_eq!(text.width(), 120);
        assert!(text.starts_with(" BUILD · claude-sonnet-5-5"), "{text:?}");
        assert!(text.contains("mode: ask  shift+tab to cycle"), "{text:?}");
        assert!(
            text.ends_with("ctx 32% · in 41.2k · out 3.8k · $0.18 · 02:17 "),
            "{text:?}"
        );
        assert_eq!(line.spans[0].style.fg, Some(theme.accent));
        assert_eq!(line.style.bg, Some(theme.surface));
        let mode = line
            .spans
            .iter()
            .find(|span| span.content.starts_with("mode:"))
            .expect("mode span");
        assert_eq!(mode.style.fg, Some(theme.accent2));
    }

    #[test]
    fn each_role_is_priced_by_the_provider_that_served_it() {
        let cheap = Price {
            input: 1.0,
            output: 5.0,
            cached: None,
        };
        let dear = Price {
            input: 15.0,
            output: 75.0,
            cached: None,
        };
        let mut session = info();
        session.role_prices = BTreeMap::from([
            (Role::Planner, Some(dear)),
            (Role::Coder, Some(cheap)),
            (Role::Reviewer, Some(dear)),
        ]);
        let started = Instant::now();
        let mut bar = StatusBar::new(&session, started);
        let usage = Usage {
            input: 1_000_000,
            output: 100_000,
            cached: 0,
        };
        bar.record_for(Role::Planner, usage);
        bar.record_for(Role::Coder, usage);
        assert_eq!(bar.role_cost(Role::Planner), Some(22.5));
        assert_eq!(bar.role_cost(Role::Coder), Some(1.5));
        assert_eq!(bar.cost(), Some(24.0));
        let text = text_of(&bar.line(160, started, &Theme::mono()));
        assert!(
            text.contains("$24.00 (planner $22.50, coder $1.50)"),
            "{text:?}"
        );

        bar.record_for(Role::Summarizer, usage);
        assert_eq!(bar.cost(), None, "an unpriced role makes the total unknown");
    }

    #[test]
    fn without_role_prices_everything_is_priced_as_the_coder() {
        let started = Instant::now();
        let mut bar = StatusBar::new(&info(), started);
        let usage = Usage {
            input: 1_000_000,
            output: 0,
            cached: 0,
        };
        bar.record_for(Role::Reviewer, usage);
        assert_eq!(bar.cost(), Some(3.0));
        assert!(!text_of(&bar.line(160, started, &Theme::mono())).contains("reviewer"));
    }

    #[test]
    fn mode_changes_show_at_once() {
        let started = Instant::now();
        let mut bar = StatusBar::new(&info(), started);
        bar.set_mode(bar.mode().next());
        assert_eq!(bar.mode(), PermissionMode::AcceptEdits);
        let text = text_of(&bar.line(120, started, &Theme::mono()));
        assert!(text.contains("mode: accept-edits"), "{text:?}");
    }

    #[test]
    fn narrow_widths_drop_the_middle_then_trim() {
        let started = Instant::now();
        let bar = StatusBar::new(&info(), started);
        for width in [60, 30, 10, 1] {
            let text = text_of(&bar.line(width, started, &Theme::mono()));
            assert_eq!(text.width(), usize::from(width), "width {width}");
        }
        let text = text_of(&bar.line(60, started, &Theme::mono()));
        assert!(!text.contains("mode:"), "{text:?}");
    }

    #[test]
    fn mono_status_has_no_background() {
        let started = Instant::now();
        let bar = StatusBar::new(&info(), started);
        let line = bar.line(80, started, &Theme::mono());
        assert_eq!(line.style.bg, None);
        assert_ne!(Theme::truecolor().surface, Color::Reset);
    }
}
