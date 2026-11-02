//! The status line between the transcript and the input.

use std::time::Instant;

use anvil_core::agent::State;
use anvil_core::provider::Usage;
use anvil_types::Price;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

use super::theme::{Shade, Theme};
use super::{PermissionMode, SessionInfo};

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
            permission_mode: info.permission_mode,
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
        if let Some(cost) = self.cost() {
            parts.push(format!("${cost:.2}"));
        }
        parts.push(elapsed(
            now.saturating_duration_since(self.started).as_secs(),
        ));
        let right = vec![Span::styled(
            format!("{} ", parts.join(" · ")),
            theme.fg(Shade::Muted),
        )];
        let spans = fit(left, middle, right, width);
        Line::from(spans).style(theme.on_surface(theme.fg(Shade::Text)))
    }
}

/// Lays the three parts out over `width` columns, dropping the middle and
/// then trimming the left when they do not fit.
fn fit(
    left: Vec<Span<'static>>,
    middle: Vec<Span<'static>>,
    right: Vec<Span<'static>>,
    width: usize,
) -> Vec<Span<'static>> {
    let (lw, mw, rw) = (
        spans_width(&left),
        spans_width(&middle),
        spans_width(&right),
    );
    if lw + mw + rw + 2 <= width {
        let spare = width - lw - mw - rw;
        let gap1 = spare / 2;
        let gap2 = spare - gap1;
        let mut spans = left;
        spans.push(Span::raw(" ".repeat(gap1)));
        spans.extend(middle);
        spans.push(Span::raw(" ".repeat(gap2)));
        spans.extend(right);
        return spans;
    }
    if lw + rw < width {
        let mut spans = left;
        spans.push(Span::raw(" ".repeat(width - lw - rw)));
        spans.extend(right);
        return spans;
    }
    let mut spans = Vec::new();
    let mut used = 0;
    for span in left.into_iter().chain([Span::raw(" ")]).chain(right) {
        let mut text = String::new();
        for ch in span.content.chars() {
            let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if used + w > width {
                break;
            }
            text.push(ch);
            used += w;
        }
        if !text.is_empty() {
            spans.push(Span::styled(text, span.style));
        }
    }
    spans.push(Span::raw(" ".repeat(width - used)));
    spans
}

/// The columns a run of spans takes.
fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|span| span.content.width()).sum()
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

/// Seconds as `mm:ss`, or `h:mm:ss` past an hour.
fn elapsed(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use ratatui::style::Color;

    use super::*;
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
            permission_mode: PermissionMode::Ask,
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
    fn status_bar_layout() {
        let started = Instant::now();
        let mut bar = StatusBar::new(&info(), started);
        bar.record(Usage {
            input: 41_200,
            output: 3_800,
            cached: 1_000,
        });
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
    fn short_and_elapsed() {
        assert_eq!(short(999), "999");
        assert_eq!(short(1_500), "1.5k");
        assert_eq!(short(2_000_000), "2.0M");
        assert_eq!(elapsed(59), "00:59");
        assert_eq!(elapsed(201), "03:21");
        assert_eq!(elapsed(3_661), "1:01:01");
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
