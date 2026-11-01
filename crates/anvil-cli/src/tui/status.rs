//! The status line between the transcript and the input.

use std::time::Instant;

use anvil_core::agent::State;
use anvil_core::provider::Usage;
use anvil_types::Price;
use ratatui::text::Line;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::SessionInfo;
use super::theme::Theme;

/// What the status line shows: state and model on the left, the
/// permission mode in the middle, usage and time on the right.
#[derive(Debug)]
pub struct StatusBar {
    state: String,
    model: String,
    permission_mode: String,
    input: u64,
    output: u64,
    cached: u64,
    /// Prompt tokens of the latest completion, for the context percentage.
    last_prompt: u64,
    context_window: u32,
    price: Option<Price>,
    started: Instant,
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
            permission_mode: info.permission_mode.clone(),
            input: 0,
            output: 0,
            cached: 0,
            last_prompt: 0,
            context_window: info.context_window,
            price: info.price,
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
    /// * `model` - The model label.
    pub fn set_model(&mut self, model: &str) {
        self.model = model.to_owned();
    }

    /// Adds one completion's usage.
    ///
    /// # Arguments
    ///
    /// * `usage` - The tokens.
    pub fn record(&mut self, usage: Usage) {
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
        self.price
            .as_ref()
            .map(|price| price.cost(self.input, self.output, self.cached))
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

    /// The line to draw, fitted to `width`.
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
        let left = format!(" {} · {}", self.state, self.model);
        let middle = format!("mode {}", self.permission_mode);
        let mut parts = vec![
            format!("in {}", short(self.input)),
            format!("out {}", short(self.output)),
        ];
        if let Some(cost) = self.cost() {
            parts.push(format!("${cost:.2}"));
        }
        parts.push(format!("ctx {}%", self.context_pct()));
        parts.push(elapsed(
            now.saturating_duration_since(self.started).as_secs(),
        ));
        let right = format!("{} ", parts.join(" · "));
        Line::styled(fit(&left, &middle, &right, width), theme.status)
    }
}

/// Lays the three parts out over `width` columns, dropping the middle and
/// then trimming the left when they do not fit.
fn fit(left: &str, middle: &str, right: &str, width: usize) -> String {
    let (lw, mw, rw) = (left.width(), middle.width(), right.width());
    if lw + mw + rw + 2 <= width {
        let spare = width - lw - mw - rw;
        let gap1 = spare / 2;
        let gap2 = spare - gap1;
        return format!(
            "{left}{}{middle}{}{right}",
            " ".repeat(gap1),
            " ".repeat(gap2)
        );
    }
    if lw + rw < width {
        return format!("{left}{}{right}", " ".repeat(width - lw - rw));
    }
    let mut text = String::new();
    let mut used = 0;
    for ch in left.chars().chain(" ".chars()).chain(right.chars()) {
        let w = ch.width().unwrap_or(0);
        if used + w > width {
            break;
        }
        text.push(ch);
        used += w;
    }
    text.push_str(&" ".repeat(width - used));
    text
}

/// A token count with a `k` or `M` suffix past three digits.
fn short(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    }
}

/// Seconds as `m:ss` or `h:mm:ss`.
fn elapsed(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::tui::SessionKind;

    fn info() -> SessionInfo {
        SessionInfo {
            kind: SessionKind::Chat,
            model: "main (sonnet)".to_owned(),
            context_window: 131_072,
            price: Some(Price {
                input: 3.0,
                output: 15.0,
                cached: Some(0.3),
            }),
            permission_mode: "ask".to_owned(),
            initial_state: State::Build,
            slash_commands: Vec::new(),
        }
    }

    fn text_of(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn usage_cost_and_context_percent_format() {
        let started = Instant::now();
        let mut bar = StatusBar::new(&info(), started);
        bar.record(Usage {
            input: 12_345,
            output: 678,
            cached: 1_000,
        });
        let line = bar.line(100, started + Duration::from_secs(201), &Theme::mono());
        let text = text_of(&line);
        assert_eq!(text.width(), 100);
        assert!(text.starts_with(" BUILD · main (sonnet)"));
        assert!(text.contains("mode ask"));
        assert!(
            text.ends_with("in 12.3k · out 678 · $0.05 · ctx 10% · 3:21 "),
            "{text:?}"
        );
    }

    #[test]
    fn narrow_widths_drop_the_middle_then_trim() {
        let started = Instant::now();
        let bar = StatusBar::new(&info(), started);
        for width in [60, 30, 10, 1] {
            let text = text_of(&bar.line(width, started, &Theme::mono()));
            assert_eq!(text.width(), usize::from(width), "width {width}");
        }
    }

    #[test]
    fn short_and_elapsed() {
        assert_eq!(short(999), "999");
        assert_eq!(short(1_500), "1.5k");
        assert_eq!(short(2_000_000), "2.0M");
        assert_eq!(elapsed(59), "0:59");
        assert_eq!(elapsed(3_661), "1:01:01");
    }
}
