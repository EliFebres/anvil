//! The scrolling transcript: what happened in the session, as entries that
//! each render to styled lines.

use anvil_core::agent::{LoopEvent, ReviewVerdict, State};
use anvil_core::tools::ToolOutcome;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::Level;
use super::theme::Theme;
use super::wrap::wrap;

/// Lines a mouse wheel notch scrolls.
pub const WHEEL_LINES: usize = 3;

/// One thing that happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    /// A line the user sent.
    User {
        /// The text.
        text: String,
    },
    /// Text from the model, possibly still streaming.
    Model {
        /// The text so far.
        text: String,
        /// Whether more deltas may arrive.
        open: bool,
    },
    /// A state the loop entered, drawn as a full-width rule.
    Banner {
        /// The state.
        state: State,
        /// Which visit, from one.
        visit: u32,
    },
    /// A line from the session or the loop.
    Notice {
        /// The text.
        text: String,
        /// How loud.
        level: Level,
    },
}

/// An entry with its rendered lines cached for one width.
#[derive(Debug)]
struct Slot {
    entry: Entry,
    cache: Option<(u16, Vec<Line<'static>>)>,
}

/// The entries and the view over them.
#[derive(Debug, Default)]
pub struct Transcript {
    slots: Vec<Slot>,
    /// Entries before this index are hidden by Ctrl+L.
    hidden_before: usize,
    /// Whether the view sticks to the bottom.
    follow: bool,
    /// The first visible line when not following.
    top: usize,
    /// Rows the last draw had.
    viewport: usize,
    /// Lines the last draw rendered in total.
    total: usize,
}

impl Transcript {
    /// An empty transcript that follows the bottom.
    ///
    /// # Returns
    ///
    /// The transcript.
    pub fn new() -> Self {
        Self {
            follow: true,
            ..Self::default()
        }
    }

    /// Appends an entry.
    ///
    /// # Arguments
    ///
    /// * `entry` - The entry.
    pub fn push(&mut self, entry: Entry) {
        self.slots.push(Slot { entry, cache: None });
    }

    /// Appends a notice.
    ///
    /// # Arguments
    ///
    /// * `text` - The text.
    /// * `level` - How loud.
    pub fn notice(&mut self, text: impl Into<String>, level: Level) {
        self.push(Entry::Notice {
            text: text.into(),
            level,
        });
    }

    /// Appends model text to the open model entry, opening one if needed.
    ///
    /// # Arguments
    ///
    /// * `delta` - The new text.
    pub fn append_text(&mut self, delta: &str) {
        match self.slots.last_mut() {
            Some(Slot {
                entry: Entry::Model { text, open: true },
                cache,
            }) => {
                text.push_str(delta);
                *cache = None;
            }
            _ => self.push(Entry::Model {
                text: delta.to_owned(),
                open: true,
            }),
        }
    }

    /// Closes the open model entry, if any, so later text starts a new one.
    pub fn close_model(&mut self) {
        if let Some(Slot {
            entry: Entry::Model { open, .. },
            cache,
        }) = self.slots.last_mut()
            && *open
        {
            *open = false;
            *cache = None;
        }
    }

    /// Applies a loop event.
    ///
    /// # Arguments
    ///
    /// * `event` - The event.
    pub fn apply(&mut self, event: &LoopEvent) {
        match event {
            LoopEvent::TextDelta(text) => self.append_text(text),
            LoopEvent::ReasoningDelta(_) | LoopEvent::Transition { .. } => {}
            LoopEvent::TurnFinished { .. } => self.close_model(),
            LoopEvent::StateEntered { state, visit } => {
                self.close_model();
                self.push(Entry::Banner {
                    state: *state,
                    visit: *visit,
                });
            }
            LoopEvent::ToolStarted { call } => {
                self.close_model();
                self.notice(
                    format!("{} {}", call.name, crate::render::call_summary(call)),
                    Level::Info,
                );
            }
            LoopEvent::ToolFinished {
                name,
                outcome,
                preview,
                ..
            } => {
                let level = match outcome {
                    ToolOutcome::Completed => Level::Info,
                    _ => Level::Error,
                };
                self.notice(
                    format!("{name} .. {}: {preview}", outcome_mark(*outcome)),
                    level,
                );
            }
            LoopEvent::PlanWritten { path } => {
                self.close_model();
                self.notice(format!("plan written to {}", path.display()), Level::Info);
            }
            LoopEvent::ReviewVerdict { verdict } => {
                self.close_model();
                match verdict {
                    ReviewVerdict::Approve => self.notice("review: approved", Level::Info),
                    ReviewVerdict::Changes { items } => {
                        self.notice("review: changes requested", Level::Warn);
                        for item in items {
                            self.notice(format!("  - {item}"), Level::Warn);
                        }
                    }
                }
            }
            LoopEvent::CheckStarted { command } => {
                self.close_model();
                self.notice(format!("running `{command}`"), Level::Info);
            }
            LoopEvent::CheckFinished {
                command,
                passed,
                output_tail,
            } => {
                if *passed {
                    self.notice(format!("`{command}` passed"), Level::Info);
                } else {
                    self.notice(format!("`{command}` failed:\n{output_tail}"), Level::Error);
                }
            }
            LoopEvent::Compacted {
                dropped_messages,
                summary_path,
                ..
            } => {
                self.close_model();
                self.notice(
                    format!(
                        "compacted {dropped_messages} messages; summary at {}",
                        summary_path.display()
                    ),
                    Level::Info,
                );
            }
            LoopEvent::Warning(message) => {
                self.close_model();
                self.notice(format!("warning: {message}"), Level::Warn);
            }
            LoopEvent::Finished {
                status,
                summary_path,
            } => {
                self.close_model();
                self.notice(
                    format!("{status}\nsummary at {}", summary_path.display()),
                    Level::Warn,
                );
            }
        }
    }

    /// The entries, hidden ones included, for tests.
    #[cfg(test)]
    pub fn entries(&self) -> Vec<&Entry> {
        self.slots.iter().map(|slot| &slot.entry).collect()
    }

    /// Every entry's text, one per line, for tests.
    #[cfg(test)]
    pub fn plain_text(&self) -> String {
        self.slots
            .iter()
            .map(|slot| match &slot.entry {
                Entry::User { text } => format!("> {text}"),
                Entry::Model { text, .. } | Entry::Notice { text, .. } => text.clone(),
                Entry::Banner { state, visit } => banner_title(*state, *visit),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Hides everything so far; the entries stay for the session.
    pub fn clear_view(&mut self) {
        self.hidden_before = self.slots.len();
        self.follow = true;
    }

    /// Scrolls up one page and stops following the bottom.
    pub fn page_up(&mut self) {
        self.follow = false;
        self.top = self
            .top
            .saturating_sub(self.viewport.saturating_sub(1).max(1));
    }

    /// Scrolls down one page; reaching the bottom resumes following.
    pub fn page_down(&mut self) {
        self.scroll_down(self.viewport.saturating_sub(1).max(1));
    }

    /// Scrolls by a mouse wheel notch.
    ///
    /// # Arguments
    ///
    /// * `up` - Whether the wheel moved toward the top.
    pub fn wheel(&mut self, up: bool) {
        if up {
            self.follow = false;
            self.top = self.top.saturating_sub(WHEEL_LINES);
        } else {
            self.scroll_down(WHEEL_LINES);
        }
    }

    /// Follows the bottom again.
    pub fn jump_bottom(&mut self) {
        self.follow = true;
    }

    /// Whether the view follows the bottom, for tests.
    #[cfg(test)]
    pub fn is_following(&self) -> bool {
        self.follow
    }

    /// Draws the visible lines.
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame.
    /// * `area` - Where to draw.
    /// * `theme` - Styles.
    pub fn draw(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let lines = self.lines(area.width, theme);
        let height = usize::from(area.height);
        self.viewport = height;
        self.total = lines.len();
        let bottom = self.total.saturating_sub(height);
        self.top = if self.follow {
            bottom
        } else {
            self.top.min(bottom)
        };
        let visible: Vec<Line<'static>> = lines.into_iter().skip(self.top).take(height).collect();
        frame.render_widget(Paragraph::new(visible), area);
    }

    /// Scrolls down, resuming following at the bottom.
    fn scroll_down(&mut self, lines: usize) {
        let bottom = self.total.saturating_sub(self.viewport);
        self.top = (self.top + lines).min(bottom);
        if self.top >= bottom {
            self.follow = true;
        }
    }

    /// Every visible entry's lines, rendered or taken from the cache.
    fn lines(&mut self, width: u16, theme: &Theme) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        for slot in self.slots.iter_mut().skip(self.hidden_before) {
            if slot.cache.as_ref().is_none_or(|(w, _)| *w != width) {
                slot.cache = Some((width, render_entry(&slot.entry, width, theme)));
            }
            if let Some((_, lines)) = &slot.cache {
                out.extend(lines.iter().cloned());
            }
        }
        out
    }
}

/// Renders one entry, followed by a blank line.
fn render_entry(entry: &Entry, width: u16, theme: &Theme) -> Vec<Line<'static>> {
    let width = usize::from(width).max(1);
    let mut lines = match entry {
        Entry::User { text } => wrap(text, width.saturating_sub(2).max(1))
            .into_iter()
            .enumerate()
            .map(|(index, line)| {
                let lead = if index == 0 { "> " } else { "  " };
                Line::from(vec![
                    Span::styled(lead, theme.user),
                    Span::styled(line, theme.user),
                ])
            })
            .collect::<Vec<_>>(),
        Entry::Model { text, .. } => wrap(text, width).into_iter().map(Line::from).collect(),
        Entry::Banner { state, visit } => {
            let title = format!("── {} ", banner_title(*state, *visit));
            let rule = "─".repeat(width.saturating_sub(title.chars().count()));
            vec![Line::styled(format!("{title}{rule}"), theme.state)]
        }
        Entry::Notice { text, level } => {
            let style = match level {
                Level::Info => theme.dim,
                Level::Warn => Style::default(),
                Level::Error => theme.failure,
            };
            wrap(text, width)
                .into_iter()
                .map(|line| Line::styled(line, style))
                .collect()
        }
    };
    lines.push(Line::default());
    lines
}

/// The banner text for a state visit: the name, with the round from the
/// second visit on.
fn banner_title(state: State, visit: u32) -> String {
    if visit > 1 {
        format!("{} (round {visit})", state.name().to_uppercase())
    } else {
        state.name().to_uppercase()
    }
}

/// The short word for a tool outcome.
fn outcome_mark(outcome: ToolOutcome) -> &'static str {
    match outcome {
        ToolOutcome::Completed => "ok",
        ToolOutcome::Failed => "failed",
        ToolOutcome::DeniedByRule => "denied by rule",
        ToolOutcome::DeclinedByUser => "declined",
        ToolOutcome::Blocked => "blocked",
        ToolOutcome::UnknownTool => "unknown tool",
        ToolOutcome::InvalidArguments => "bad arguments",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deltas_extend_the_open_model_entry_only() {
        let mut transcript = Transcript::new();
        transcript.append_text("hel");
        transcript.append_text("lo");
        transcript.close_model();
        transcript.append_text("again");
        let entries = transcript.entries();
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[0],
            &Entry::Model {
                text: "hello".to_owned(),
                open: false
            }
        );
        assert_eq!(transcript.plain_text(), "hello\nagain");
    }

    #[test]
    fn clear_view_hides_but_keeps_entries() {
        let mut transcript = Transcript::new();
        transcript.notice("one", Level::Info);
        transcript.clear_view();
        transcript.notice("two", Level::Info);
        assert_eq!(transcript.entries().len(), 2);
        let lines = transcript.lines(40, &Theme::mono());
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].to_string(), "two");
    }

    #[test]
    fn scroll_follow_rules() {
        let mut transcript = Transcript::new();
        for n in 0..50 {
            transcript.notice(format!("line {n}"), Level::Info);
        }
        transcript.viewport = 10;
        transcript.total = 100;
        transcript.top = 90;
        transcript.page_up();
        assert!(!transcript.is_following());
        assert_eq!(transcript.top, 81);
        transcript.wheel(true);
        assert_eq!(transcript.top, 78);
        transcript.page_down();
        transcript.page_down();
        assert!(transcript.is_following());
        transcript.page_up();
        transcript.jump_bottom();
        assert!(transcript.is_following());
    }
}
