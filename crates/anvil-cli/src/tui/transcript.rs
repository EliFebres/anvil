//! The scrolling transcript: what happened in the session, as entries that
//! each render to styled lines.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anvil_core::agent::{LoopEvent, ReviewVerdict, State};
use anvil_core::provider::ToolCall;
use anvil_core::tools::ToolOutcome;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use super::Level;
use super::markdown::MarkdownDoc;
use super::theme::Theme;
use super::wrap::{segments, wrap};

/// Lines a mouse wheel notch scrolls.
pub const WHEEL_LINES: usize = 3;

/// Spinner frames for running tools and checks, advanced by the tick.
const FRAMES: [&str; 4] = ["|", "/", "-", "\\"];

/// Columns the expanded argument and result text are indented by.
const DETAIL_INDENT: usize = 6;

/// One thing that happened.
#[derive(Debug)]
pub enum Entry {
    /// A line the user sent.
    User {
        /// The text.
        text: String,
    },
    /// Text from the model, rendered as markdown while it streams.
    Model {
        /// The text so far.
        doc: MarkdownDoc,
        /// Whether more deltas may arrive.
        open: bool,
    },
    /// A tool call, one collapsed line or the full arguments and result.
    Tool(ToolEntry),
    /// A state the loop entered, drawn as a full-width rule.
    Banner {
        /// The state.
        state: State,
        /// Which visit, from one.
        visit: u32,
    },
    /// The plan with its checkboxes, updated as the model ticks them.
    Plan(PlanBlock),
    /// A project check the loop ran.
    Check {
        /// The command line.
        command: String,
        /// How it went.
        status: CheckStatus,
    },
    /// The reviewer's verdict.
    Review {
        /// The verdict.
        verdict: ReviewVerdict,
    },
    /// A line from the session or the loop.
    Notice {
        /// The text.
        text: String,
        /// How loud.
        level: Level,
    },
}

/// A tool call as the transcript shows it.
#[derive(Debug)]
pub struct ToolEntry {
    /// The call.
    pub call: ToolCall,
    /// The one-line argument summary.
    pub summary: String,
    /// Running or finished.
    pub status: ToolStatus,
    /// Whether the arguments and result are shown.
    pub expanded: bool,
    /// The result text and whether it was an error, once the hook saw it.
    pub result: Option<(String, bool)>,
}

/// Where a tool call is.
#[derive(Debug)]
pub enum ToolStatus {
    /// Still running.
    Running {
        /// When it started.
        started: Instant,
    },
    /// Done.
    Finished {
        /// How it ended.
        outcome: ToolOutcome,
        /// How long it took.
        duration: Duration,
        /// The first line of the result.
        preview: String,
    },
}

/// A parsed `plan.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanBlock {
    /// Where the plan lives.
    pub path: PathBuf,
    /// The heading, if the plan has one.
    pub title: Option<String>,
    /// The checkbox items in order.
    pub items: Vec<PlanItem>,
    /// Lines that are neither heading nor item.
    pub notes: Vec<String>,
}

/// One checkbox line of the plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanItem {
    /// Whether the model ticked it.
    pub done: bool,
    /// The step.
    pub text: String,
}

/// How a project check went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckStatus {
    /// Still running.
    Running,
    /// Exit zero.
    Passed,
    /// Non-zero, with the end of its output.
    Failed {
        /// The output tail.
        tail: String,
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
    /// The project root; paths under it are shown relative to it.
    cwd: PathBuf,
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
    /// The selected tool entry, highlighted and toggled by Enter.
    selected: Option<usize>,
    /// Each visible slot's first line and line count, from the last render.
    layout: Vec<(usize, usize, usize)>,
}

impl Transcript {
    /// An empty transcript that follows the bottom.
    ///
    /// # Arguments
    ///
    /// * `cwd` - The project root; paths under it are shown relative to it.
    ///
    /// # Returns
    ///
    /// The transcript.
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            follow: true,
            ..Self::default()
        }
    }

    /// A path as the transcript shows it: relative to the project root
    /// with `/` separators when it is inside it, otherwise as given.
    fn display_path(&self, path: &Path) -> String {
        match path.strip_prefix(&self.cwd) {
            Ok(relative) if !self.cwd.as_os_str().is_empty() => relative
                .components()
                .map(|part| part.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/"),
            _ => path.display().to_string(),
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
                entry: Entry::Model { doc, open: true },
                cache,
            }) => {
                doc.push(delta);
                *cache = None;
            }
            _ => {
                let mut doc = MarkdownDoc::new();
                doc.push(delta);
                self.push(Entry::Model { doc, open: true });
            }
        }
    }

    /// Closes the open model entry, if any, so later text starts a new one.
    pub fn close_model(&mut self) {
        if let Some(Slot {
            entry: Entry::Model { doc, open },
            cache,
        }) = self.slots.last_mut()
            && *open
        {
            doc.finish();
            *open = false;
            *cache = None;
        }
    }

    /// Applies a loop event.
    ///
    /// # Arguments
    ///
    /// * `event` - The event.
    /// * `now` - The current time, for tool durations.
    pub fn apply(&mut self, event: &LoopEvent, now: Instant) {
        match event {
            LoopEvent::TextDelta(text) => self.append_text(text),
            LoopEvent::ReasoningDelta(_)
            | LoopEvent::Transition { .. }
            | LoopEvent::PlanWritten { .. } => {}
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
                self.push(Entry::Tool(ToolEntry {
                    summary: crate::render::call_summary(call),
                    call: call.clone(),
                    status: ToolStatus::Running { started: now },
                    expanded: false,
                    result: None,
                }));
            }
            LoopEvent::ToolFinished {
                call_id,
                name,
                outcome,
                preview,
                ..
            } => {
                let index = self.ensure_tool(call_id, name, now);
                if let Entry::Tool(tool) = &mut self.slots[index].entry {
                    let started = match tool.status {
                        ToolStatus::Running { started } => started,
                        ToolStatus::Finished { .. } => now,
                    };
                    tool.status = ToolStatus::Finished {
                        outcome: *outcome,
                        duration: now.saturating_duration_since(started),
                        preview: preview.clone(),
                    };
                    self.slots[index].cache = None;
                }
            }
            LoopEvent::ReviewVerdict { verdict } => {
                self.close_model();
                self.push(Entry::Review {
                    verdict: verdict.clone(),
                });
            }
            LoopEvent::CheckStarted { command } => {
                self.close_model();
                self.push(Entry::Check {
                    command: command.clone(),
                    status: CheckStatus::Running,
                });
            }
            LoopEvent::CheckFinished {
                command,
                passed,
                output_tail,
            } => {
                let status = if *passed {
                    CheckStatus::Passed
                } else {
                    CheckStatus::Failed {
                        tail: output_tail.clone(),
                    }
                };
                let running = self.slots.iter().rposition(|slot| {
                    matches!(&slot.entry, Entry::Check { command: c, status: CheckStatus::Running } if c == command)
                });
                match running {
                    Some(index) => {
                        if let Entry::Check { status: s, .. } = &mut self.slots[index].entry {
                            *s = status;
                        }
                        self.slots[index].cache = None;
                    }
                    None => self.push(Entry::Check {
                        command: command.clone(),
                        status,
                    }),
                }
            }
            LoopEvent::Compacted {
                dropped_messages,
                summary_path,
                ..
            } => {
                self.close_model();
                let text = format!(
                    "compacted {dropped_messages} messages; summary at {}",
                    self.display_path(summary_path)
                );
                self.notice(text, Level::Info);
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
                let text = format!("{status}\nsummary at {}", self.display_path(summary_path));
                self.notice(text, Level::Warn);
            }
        }
    }

    /// Records a tool call's full result, before or after it finished.
    ///
    /// # Arguments
    ///
    /// * `call_id` - The call.
    /// * `text` - The result as the model saw it, already capped.
    /// * `is_error` - Whether the tool failed.
    /// * `now` - The current time, in case the call is not known yet.
    pub fn set_tool_result(&mut self, call_id: &str, text: String, is_error: bool, now: Instant) {
        let index = self.ensure_tool(call_id, "tool", now);
        if let Entry::Tool(tool) = &mut self.slots[index].entry {
            tool.result = Some((text, is_error));
            self.slots[index].cache = None;
        }
    }

    /// Shows a plan, replacing the block for the same file in place so the
    /// checkboxes update where they are.
    ///
    /// # Arguments
    ///
    /// * `path` - The plan file.
    /// * `text` - Its contents.
    pub fn set_plan(&mut self, path: PathBuf, text: &str) {
        let block = parse_plan(path, text);
        let existing = self
            .slots
            .iter()
            .rposition(|slot| matches!(&slot.entry, Entry::Plan(plan) if plan.path == block.path));
        match existing {
            Some(index) => {
                self.slots[index].entry = Entry::Plan(block);
                self.slots[index].cache = None;
            }
            None => {
                self.close_model();
                self.push(Entry::Plan(block));
            }
        }
    }

    /// Expands every tool call.
    pub fn expand_all(&mut self) {
        for slot in &mut self.slots {
            if let Entry::Tool(tool) = &mut slot.entry
                && !tool.expanded
            {
                tool.expanded = true;
                slot.cache = None;
            }
        }
    }

    /// Toggles the selected tool call, or the most recent one when nothing
    /// is selected.
    ///
    /// # Returns
    ///
    /// Whether there was a tool call to toggle.
    pub fn toggle_selected(&mut self) -> bool {
        let index = match self.selected {
            Some(index) => index,
            None => match self.tool_indices().last() {
                Some(index) => {
                    self.selected = Some(*index);
                    *index
                }
                None => return false,
            },
        };
        self.toggle(index)
    }

    /// Moves the selection to the previous tool call.
    pub fn select_prev(&mut self) {
        let tools = self.tool_indices();
        let next = match self.selected {
            Some(current) => tools.iter().rev().find(|index| **index < current).copied(),
            None => tools.last().copied(),
        };
        if next.is_some() {
            self.selected = next;
            self.scroll_to_selected();
        }
    }

    /// Moves the selection to the next tool call.
    pub fn select_next(&mut self) {
        let tools = self.tool_indices();
        let next = match self.selected {
            Some(current) => tools.iter().find(|index| **index > current).copied(),
            None => tools.last().copied(),
        };
        if next.is_some() {
            self.selected = next;
            self.scroll_to_selected();
        }
    }

    /// Drops the selection.
    pub fn clear_selection(&mut self) {
        self.selected = None;
    }

    /// Handles a click on a transcript row: a tool call there is selected
    /// and toggled.
    ///
    /// # Arguments
    ///
    /// * `row` - The row within the transcript area.
    pub fn click(&mut self, row: u16) {
        if usize::from(row) >= self.viewport {
            return;
        }
        let line = self.top + usize::from(row);
        let hit = self
            .layout
            .iter()
            .find(|(_, start, len)| line >= *start && line < start + len)
            .map(|(index, _, _)| *index);
        if let Some(index) = hit
            && matches!(self.slots[index].entry, Entry::Tool(_))
        {
            self.selected = Some(index);
            self.toggle(index);
        }
    }

    /// The entries, hidden ones included, for tests.
    #[cfg(test)]
    pub fn entries(&self) -> Vec<&Entry> {
        self.slots.iter().map(|slot| &slot.entry).collect()
    }

    /// Every entry's text, one block per entry, for tests.
    #[cfg(test)]
    pub fn plain_text(&self) -> String {
        self.slots
            .iter()
            .map(|slot| match &slot.entry {
                Entry::User { text } => format!("> {text}"),
                Entry::Model { doc, .. } => doc.text().to_owned(),
                Entry::Tool(tool) => format!("{} {}", tool.call.name, tool.summary),
                Entry::Banner { state, visit } => banner_title(*state, *visit),
                Entry::Plan(plan) => plan
                    .items
                    .iter()
                    .map(|item| format!("[{}] {}", if item.done { "x" } else { " " }, item.text))
                    .collect::<Vec<_>>()
                    .join("\n"),
                Entry::Check { command, .. } => format!("check {command}"),
                Entry::Review { .. } => "review".to_owned(),
                Entry::Notice { text, .. } => text.clone(),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Hides everything so far; the entries stay for the session.
    pub fn clear_view(&mut self) {
        self.hidden_before = self.slots.len();
        self.selected = None;
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
    /// * `tick` - The spinner frame counter.
    pub fn draw(&mut self, frame: &mut Frame, area: Rect, theme: &Theme, tick: u64) {
        let lines = self.lines(area.width, theme, tick);
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

    /// Scrolls so the selected entry's first line is on screen.
    fn scroll_to_selected(&mut self) {
        let Some(selected) = self.selected else {
            return;
        };
        let Some((_, start, _)) = self.layout.iter().find(|(index, _, _)| *index == selected)
        else {
            return;
        };
        if *start < self.top || *start >= self.top + self.viewport {
            self.follow = false;
            self.top = start.saturating_sub(self.viewport / 2);
        }
    }

    /// Every visible tool entry's slot index.
    fn tool_indices(&self) -> Vec<usize> {
        self.slots
            .iter()
            .enumerate()
            .skip(self.hidden_before)
            .filter(|(_, slot)| matches!(slot.entry, Entry::Tool(_)))
            .map(|(index, _)| index)
            .collect()
    }

    /// Toggles one tool entry.
    fn toggle(&mut self, index: usize) -> bool {
        match &mut self.slots[index].entry {
            Entry::Tool(tool) => {
                tool.expanded = !tool.expanded;
                self.slots[index].cache = None;
                true
            }
            _ => false,
        }
    }

    /// The slot for a tool call, created as a placeholder when the start
    /// event was never seen.
    fn ensure_tool(&mut self, call_id: &str, name: &str, now: Instant) -> usize {
        let found = self
            .slots
            .iter()
            .rposition(|slot| matches!(&slot.entry, Entry::Tool(tool) if tool.call.id == call_id));
        match found {
            Some(index) => index,
            None => {
                self.push(Entry::Tool(ToolEntry {
                    call: ToolCall {
                        id: call_id.to_owned(),
                        name: name.to_owned(),
                        arguments_json: "{}".to_owned(),
                    },
                    summary: String::new(),
                    status: ToolStatus::Running { started: now },
                    expanded: false,
                    result: None,
                }));
                self.slots.len() - 1
            }
        }
    }

    /// Every visible entry's lines, rendered or taken from the cache, with
    /// the selection highlighted and the layout recorded for clicks.
    fn lines(&mut self, width: u16, theme: &Theme, tick: u64) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        let mut layout = Vec::new();
        let hidden_before = self.hidden_before;
        let selected = self.selected;
        for (index, slot) in self.slots.iter_mut().enumerate().skip(hidden_before) {
            let live = is_live(&slot.entry);
            if live || slot.cache.as_ref().is_none_or(|(w, _)| *w != width) {
                let lines = render_entry(&mut slot.entry, width, theme, tick);
                slot.cache = Some((width, lines));
            }
            let Some((_, lines)) = &slot.cache else {
                continue;
            };
            let mut lines = lines.clone();
            if selected == Some(index)
                && let Some(first) = lines.first_mut()
            {
                *first = first
                    .clone()
                    .patch_style(Style::new().add_modifier(Modifier::REVERSED));
            }
            layout.push((index, out.len(), lines.len()));
            out.extend(lines);
        }
        self.layout = layout;
        out
    }
}

/// Whether an entry changes without an event, so it must not be cached.
fn is_live(entry: &Entry) -> bool {
    matches!(
        entry,
        Entry::Tool(ToolEntry {
            status: ToolStatus::Running { .. },
            ..
        }) | Entry::Check {
            status: CheckStatus::Running,
            ..
        }
    )
}

/// Renders one entry, followed by a blank line.
fn render_entry(entry: &mut Entry, width: u16, theme: &Theme, tick: u64) -> Vec<Line<'static>> {
    let columns = usize::from(width).max(1);
    let mut lines = match entry {
        Entry::User { text } => wrap(text, columns.saturating_sub(2).max(1))
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
        Entry::Model { doc, .. } => doc.lines(width, theme),
        Entry::Tool(tool) => render_tool(tool, columns, theme, tick),
        Entry::Banner { state, visit } => {
            let title = format!("── {} ", banner_title(*state, *visit));
            let rule = "─".repeat(columns.saturating_sub(title.chars().count()));
            vec![Line::styled(format!("{title}{rule}"), theme.state)]
        }
        Entry::Plan(plan) => render_plan(plan, columns, theme),
        Entry::Check { command, status } => render_check(command, status, columns, theme, tick),
        Entry::Review { verdict } => match verdict {
            ReviewVerdict::Approve => vec![Line::styled("review: approved", theme.dim)],
            ReviewVerdict::Changes { items } => {
                let mut lines = vec![Line::from("review: changes requested")];
                for item in items {
                    lines.extend(
                        wrap(item, columns.saturating_sub(4).max(1))
                            .into_iter()
                            .enumerate()
                            .map(|(index, line)| {
                                let lead = if index == 0 { "  - " } else { "    " };
                                Line::from(format!("{lead}{line}"))
                            }),
                    );
                }
                lines
            }
        },
        Entry::Notice { text, level } => {
            let style = match level {
                Level::Info => theme.dim,
                Level::Warn => Style::default(),
                Level::Error => theme.failure,
            };
            wrap(text, columns)
                .into_iter()
                .map(|line| Line::styled(line, style))
                .collect()
        }
    };
    lines.push(Line::default());
    lines
}

/// The collapsed tool line, plus the arguments and result when expanded.
fn render_tool(tool: &ToolEntry, width: usize, theme: &Theme, tick: u64) -> Vec<Line<'static>> {
    let spinner = FRAMES[usize::try_from(tick % 4).unwrap_or(0)];
    let (status, status_style, duration, preview) = match &tool.status {
        ToolStatus::Running { .. } => (spinner.to_owned(), theme.dim, String::new(), String::new()),
        ToolStatus::Finished {
            outcome,
            duration,
            preview,
        } => {
            let style = if *outcome == ToolOutcome::Completed {
                theme.dim
            } else {
                theme.failure
            };
            (
                outcome_mark(*outcome).to_owned(),
                style,
                duration_label(*duration),
                preview.clone(),
            )
        }
    };
    let head = format!("{} {}", tool_icon(&tool.call.name), tool.call.name);
    let mut spans = vec![Span::raw(head.clone()), Span::raw(" ")];
    let mut used = head.width() + 1;
    let summary = fit_text(
        &tool.summary,
        width.saturating_sub(used + status.width() + 4),
    );
    used += summary.width();
    spans.push(Span::styled(summary, theme.dim));
    spans.push(Span::raw("  "));
    spans.push(Span::styled(status.clone(), status_style));
    used += 2 + status.width();
    if !duration.is_empty() {
        spans.push(Span::styled(format!(" {duration}"), theme.dim));
        used += 1 + duration.width();
    }
    let remaining = width.saturating_sub(used + 2);
    if !preview.is_empty() && remaining >= 4 {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(fit_text(&preview, remaining), theme.dim));
    }
    let mut lines = vec![Line::from(spans)];
    if !tool.expanded {
        return lines;
    }
    let indent = " ".repeat(DETAIL_INDENT);
    let inner = width.saturating_sub(DETAIL_INDENT).max(1);
    lines.push(Line::styled("    args:", theme.dim));
    let args = serde_json::from_str::<serde_json::Value>(&tool.call.arguments_json)
        .and_then(|value| serde_json::to_string_pretty(&value))
        .unwrap_or_else(|_| tool.call.arguments_json.clone());
    for line in args.lines() {
        for (_, segment) in segments(line, inner) {
            lines.push(Line::from(format!("{indent}{segment}")));
        }
    }
    match &tool.result {
        Some((text, is_error)) => {
            let (title, style) = if *is_error {
                ("    result (error):", theme.failure)
            } else {
                ("    result:", theme.dim)
            };
            lines.push(Line::styled(title, style));
            for line in text.lines() {
                for (_, segment) in segments(line, inner) {
                    lines.push(Line::from(format!("{indent}{segment}")));
                }
            }
        }
        None => lines.push(Line::styled("    result: (not yet)", theme.dim)),
    }
    lines
}

/// The plan: its title, the checkbox items and any notes.
fn render_plan(plan: &PlanBlock, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let mut lines = vec![Line::styled(
        plan.title.clone().unwrap_or_else(|| "Plan".to_owned()),
        theme.heading,
    )];
    for item in &plan.items {
        let (lead, style) = if item.done {
            ("  [x] ", theme.dim)
        } else {
            ("  [ ] ", Style::default())
        };
        lines.extend(
            wrap(&item.text, width.saturating_sub(lead.len()).max(1))
                .into_iter()
                .enumerate()
                .map(|(index, line)| {
                    let prefix = if index == 0 { lead } else { "      " };
                    Line::styled(format!("{prefix}{line}"), style)
                }),
        );
    }
    for note in &plan.notes {
        lines.extend(
            wrap(note, width.saturating_sub(2).max(1))
                .into_iter()
                .map(|line| Line::styled(format!("  {line}"), theme.dim)),
        );
    }
    lines
}

/// A project check: running, passed, or failed with its output tail.
fn render_check(
    command: &str,
    status: &CheckStatus,
    width: usize,
    theme: &Theme,
    tick: u64,
) -> Vec<Line<'static>> {
    match status {
        CheckStatus::Running => {
            let spinner = FRAMES[usize::try_from(tick % 4).unwrap_or(0)];
            vec![Line::styled(
                format!("{spinner} running `{command}`"),
                theme.dim,
            )]
        }
        CheckStatus::Passed => vec![Line::styled(format!("ok `{command}`"), theme.dim)],
        CheckStatus::Failed { tail } => {
            let mut lines = vec![Line::styled(format!("failed `{command}`:"), theme.failure)];
            for line in tail.lines() {
                for (_, segment) in segments(line, width.saturating_sub(4).max(1)) {
                    lines.push(Line::styled(format!("    {segment}"), theme.dim));
                }
            }
            lines
        }
    }
}

/// Parses `plan.md` into its title, checkbox items and notes.
fn parse_plan(path: PathBuf, text: &str) -> PlanBlock {
    let mut block = PlanBlock {
        path,
        title: None,
        items: Vec::new(),
        notes: Vec::new(),
    };
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(title) = line.strip_prefix('#') {
            if block.title.is_none() {
                block.title = Some(title.trim_start_matches('#').trim().to_owned());
            }
            continue;
        }
        if let Some(item) = parse_plan_item(line) {
            block.items.push(item);
        } else {
            block.notes.push(line.to_owned());
        }
    }
    block
}

/// One `- [ ] step` or `1. [x] step` line.
fn parse_plan_item(line: &str) -> Option<PlanItem> {
    let rest = if let Some(rest) = line.strip_prefix(['-', '*', '+']) {
        rest
    } else {
        let digits = line.chars().take_while(char::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        line[digits..].strip_prefix(['.', ')'])?
    };
    let rest = rest.trim_start();
    let (done, text) = if let Some(text) = rest.strip_prefix("[ ]") {
        (false, text)
    } else if let Some(text) = rest
        .strip_prefix("[x]")
        .or_else(|| rest.strip_prefix("[X]"))
    {
        (true, text)
    } else {
        return None;
    };
    Some(PlanItem {
        done,
        text: text.trim().to_owned(),
    })
}

/// Cuts text to `width` columns with an ellipsis.
fn fit_text(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }
    if width < 2 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > width - 1 {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

/// A glyph for the tool kind; ASCII so every console font has it.
fn tool_icon(name: &str) -> &'static str {
    match name {
        "shell" => "$",
        "read_file" => "<",
        "write_file" | "edit_file" => ">",
        "glob" | "grep" => "/",
        _ => "*",
    }
}

/// `340ms`, `1.2s` or `2m05s`.
fn duration_label(duration: Duration) -> String {
    let millis = duration.as_millis();
    if millis < 1_000 {
        format!("{millis}ms")
    } else if millis < 60_000 {
        format!("{:.1}s", duration.as_secs_f64())
    } else {
        let secs = duration.as_secs();
        format!("{}m{:02}s", secs / 60, secs % 60)
    }
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

/// Whether a path names the plan file, for tests and the runner.
///
/// # Arguments
///
/// * `candidate` - A path a tool wrote.
/// * `plan` - The plan file.
///
/// # Returns
///
/// True when both resolve to the same file.
pub fn is_plan_file(candidate: &Path, plan: &Path) -> bool {
    match (candidate.canonicalize(), plan.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => candidate == plan,
    }
}

#[cfg(test)]
mod tests {
    use anvil_core::tools::OutputStats;

    use super::*;

    fn call(id: &str, name: &str, args: &str) -> ToolCall {
        ToolCall {
            id: id.to_owned(),
            name: name.to_owned(),
            arguments_json: args.to_owned(),
        }
    }

    fn finished(id: &str, name: &str, outcome: ToolOutcome, preview: &str) -> LoopEvent {
        LoopEvent::ToolFinished {
            call_id: id.to_owned(),
            name: name.to_owned(),
            outcome,
            preview: preview.to_owned(),
            stats: OutputStats {
                bytes: 0,
                truncated: false,
                digest: 0,
            },
        }
    }

    fn fresh() -> Transcript {
        Transcript::new(PathBuf::from("/project"))
    }

    #[test]
    fn paths_under_the_project_are_shown_relative_with_slashes() {
        let transcript = fresh();
        assert_eq!(
            transcript.display_path(Path::new("/project/.anvil/sessions/x/summary.md")),
            ".anvil/sessions/x/summary.md"
        );
        assert_eq!(
            transcript.display_path(Path::new("/elsewhere/summary.md")),
            Path::new("/elsewhere/summary.md").display().to_string()
        );
    }

    fn joined(transcript: &mut Transcript, width: u16) -> String {
        transcript
            .lines(width, &Theme::mono(), 0)
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn deltas_extend_the_open_model_entry_only() {
        let mut transcript = fresh();
        transcript.append_text("hel");
        transcript.append_text("lo");
        transcript.close_model();
        transcript.append_text("again");
        assert_eq!(transcript.entries().len(), 2);
        assert_eq!(transcript.plain_text(), "hello\nagain");
    }

    #[test]
    fn tool_result_before_or_after_finished() {
        let now = Instant::now();
        let shell = call("c1", "shell", "{\"command\":\"ls\"}");
        for result_first in [true, false] {
            let mut transcript = fresh();
            transcript.apply(
                &LoopEvent::ToolStarted {
                    call: shell.clone(),
                },
                now,
            );
            if result_first {
                transcript.set_tool_result("c1", "a\nb".to_owned(), false, now);
            }
            transcript.apply(
                &finished("c1", "shell", ToolOutcome::Completed, "a"),
                now + Duration::from_millis(340),
            );
            if !result_first {
                transcript.set_tool_result("c1", "a\nb".to_owned(), false, now);
            }
            assert_eq!(transcript.entries().len(), 1);
            let collapsed = joined(&mut transcript, 60);
            assert!(
                collapsed.starts_with("$ shell ls  ok 340ms  a"),
                "{collapsed:?}"
            );
            assert!(!collapsed.contains("args:"));
            assert!(transcript.toggle_selected());
            let expanded = joined(&mut transcript, 60);
            assert!(expanded.contains("args:"), "{expanded:?}");
            assert!(expanded.contains("\"command\": \"ls\""));
            assert!(
                expanded.contains("result:\n      a\n      b"),
                "{expanded:?}"
            );
        }
    }

    #[test]
    fn a_result_for_an_unknown_call_makes_a_placeholder() {
        let now = Instant::now();
        let mut transcript = fresh();
        transcript.set_tool_result("zz", "out".to_owned(), true, now);
        transcript.apply(&finished("zz", "shell", ToolOutcome::Failed, "out"), now);
        assert_eq!(transcript.entries().len(), 1);
        transcript.expand_all();
        let text = joined(&mut transcript, 60);
        assert!(text.contains("result (error):"), "{text:?}");
    }

    #[test]
    fn plan_ticks_update_in_place() {
        let mut transcript = fresh();
        let path = PathBuf::from("plan.md");
        transcript.set_plan(
            path.clone(),
            "# Plan\n\n1. [ ] first\n2. [ ] second\nnote\n",
        );
        transcript.notice("later", Level::Info);
        transcript.set_plan(path, "# Plan\n\n1. [x] first\n2. [ ] second\nnote\n");
        assert_eq!(transcript.entries().len(), 2);
        assert!(matches!(transcript.entries()[0], Entry::Plan(_)));
        let text = joined(&mut transcript, 40);
        assert!(
            text.starts_with("Plan\n  [x] first\n  [ ] second\n  note"),
            "{text:?}"
        );
    }

    #[test]
    fn checks_update_the_running_entry() {
        let mut transcript = fresh();
        transcript.apply(
            &LoopEvent::CheckStarted {
                command: "cargo test".to_owned(),
            },
            Instant::now(),
        );
        transcript.apply(
            &LoopEvent::CheckFinished {
                command: "cargo test".to_owned(),
                passed: false,
                output_tail: "boom".to_owned(),
            },
            Instant::now(),
        );
        assert_eq!(transcript.entries().len(), 1);
        let text = joined(&mut transcript, 40);
        assert_eq!(text, "failed `cargo test`:\n    boom\n");
    }

    #[test]
    fn selection_moves_over_tool_calls_and_esc_clears_it() {
        let now = Instant::now();
        let mut transcript = fresh();
        transcript.apply(
            &LoopEvent::ToolStarted {
                call: call("a", "shell", "{}"),
            },
            now,
        );
        transcript.notice("between", Level::Info);
        transcript.apply(
            &LoopEvent::ToolStarted {
                call: call("b", "glob", "{}"),
            },
            now,
        );
        transcript.select_prev();
        assert_eq!(transcript.selected, Some(2));
        transcript.select_prev();
        assert_eq!(transcript.selected, Some(0));
        transcript.select_prev();
        assert_eq!(transcript.selected, Some(0));
        transcript.select_next();
        assert_eq!(transcript.selected, Some(2));
        transcript.clear_selection();
        assert_eq!(transcript.selected, None);
        let _ = joined(&mut transcript, 40);
        transcript.viewport = 10;
        transcript.click(0);
        assert_eq!(transcript.selected, Some(0));
    }

    #[test]
    fn clear_view_hides_but_keeps_entries() {
        let mut transcript = fresh();
        transcript.notice("one", Level::Info);
        transcript.clear_view();
        transcript.notice("two", Level::Info);
        assert_eq!(transcript.entries().len(), 2);
        assert_eq!(joined(&mut transcript, 40), "two\n");
    }

    #[test]
    fn scroll_follow_rules() {
        let mut transcript = fresh();
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

    #[test]
    fn durations_and_fit() {
        assert_eq!(duration_label(Duration::from_millis(340)), "340ms");
        assert_eq!(duration_label(Duration::from_millis(1_250)), "1.2s");
        assert_eq!(duration_label(Duration::from_secs(125)), "2m05s");
        assert_eq!(fit_text("abcdef", 4), "abc…");
        assert_eq!(fit_text("abc", 4), "abc");
    }
}
