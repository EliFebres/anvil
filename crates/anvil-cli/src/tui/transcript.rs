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
use super::diff::{Diff, RowKind, diff};
use super::markdown::{MarkdownDoc, inline_lines};
use super::theme::{Shade, Theme};
use super::welcome::Welcome;
use super::wrap::{segments, wrap};

/// Lines a mouse wheel notch scrolls.
pub const WHEEL_LINES: usize = 3;

/// Columns the expanded argument and result text are indented by.
const DETAIL_INDENT: usize = 6;

/// Columns the status token of a tool line is padded to.
const STATUS_WIDTH: usize = 6;

/// The label before the user's own messages.
const YOU: &str = "you";

/// The cursor drawn after streaming model text.
const CURSOR: &str = "█";

/// One thing that happened.
#[derive(Debug)]
pub enum Entry {
    /// The startup screen.
    Welcome(Welcome),
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
    /// What an edit is about to change, as a diff.
    Edit {
        /// The file as shown.
        shown_path: String,
        /// The change.
        diff: Diff,
    },
    /// A state the loop entered, drawn as a full-width rule.
    Banner {
        /// The state.
        state: State,
        /// Which visit, from one.
        visit: u32,
        /// When the state began.
        started: Instant,
        /// How long it ran, once the next state began.
        ended: Option<Duration>,
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
    /// A prompt the user answered.
    Answer {
        /// What was asked.
        question: String,
        /// What the user said.
        answer: String,
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
    /// Stopped by the user before it finished.
    Cancelled {
        /// How long it ran.
        duration: Duration,
    },
}

/// A parsed `plan.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanBlock {
    /// Where the plan lives.
    pub path: PathBuf,
    /// The path as shown: relative to the project when inside it.
    pub shown_path: String,
    /// The heading, if the plan has one.
    pub title: Option<String>,
    /// The checkbox items in order.
    pub items: Vec<PlanItem>,
    /// Lines that are neither heading nor item.
    pub notes: Vec<String>,
    /// Distinct files the plan names in backticks.
    pub files: usize,
    /// Lines in the file.
    pub lines: usize,
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

/// An entry with its rendered lines cached for one width and dimming.
#[derive(Debug)]
struct Slot {
    entry: Entry,
    cache: Option<((u16, bool), Vec<Line<'static>>)>,
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
                self.end_banner(now);
                self.push(Entry::Banner {
                    state: *state,
                    visit: *visit,
                    started: now,
                    ended: None,
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
                    if tool.summary.is_empty() && tool.call.arguments_json == "{}" {
                        tool.call.name = name.clone();
                    }
                    let started = match tool.status {
                        ToolStatus::Running { started } => started,
                        ToolStatus::Finished { .. } | ToolStatus::Cancelled { .. } => now,
                    };
                    tool.status = ToolStatus::Finished {
                        outcome: *outcome,
                        duration: now.saturating_duration_since(started),
                        preview: preview.clone(),
                    };
                    self.slots[index].cache = None;
                }
            }
            LoopEvent::ProposedEdit { path, old, new } => {
                self.close_model();
                let shown_path = self.display_path(path);
                self.push(Entry::Edit {
                    shown_path,
                    diff: diff(old, new),
                });
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
                self.end_banner(now);
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
        let shown_path = self.display_path(&path);
        let block = parse_plan(path, shown_path, text);
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
                // The model just streamed this very text; the checklist replaces
                // it rather than showing the plan twice.
                let streamed = self.slots.last().is_some_and(|slot| {
                    matches!(&slot.entry, Entry::Model { doc, .. } if doc.text().trim() == text.trim())
                });
                if streamed {
                    self.slots.pop();
                }
                self.push(Entry::Plan(block));
            }
        }
    }

    /// Ends what a cancelled turn left running: the open model text, tool
    /// calls still spinning and checks still running.
    ///
    /// # Arguments
    ///
    /// * `now` - The current time, for the durations.
    pub fn cancel_running(&mut self, now: Instant) {
        self.close_model();
        for slot in &mut self.slots {
            match &mut slot.entry {
                Entry::Tool(ToolEntry {
                    status: status @ ToolStatus::Running { .. },
                    ..
                }) => {
                    if let ToolStatus::Running { started } = *status {
                        *status = ToolStatus::Cancelled {
                            duration: now.saturating_duration_since(started),
                        };
                    }
                    slot.cache = None;
                }
                Entry::Check {
                    status: status @ CheckStatus::Running,
                    ..
                } => {
                    *status = CheckStatus::Failed {
                        tail: "cancelled".to_owned(),
                    };
                    slot.cache = None;
                }
                _ => {}
            }
        }
    }

    /// Whether a plan block is shown.
    ///
    /// # Returns
    ///
    /// True once a plan file was set.
    pub fn has_plan(&self) -> bool {
        self.slots
            .iter()
            .any(|slot| matches!(slot.entry, Entry::Plan(_)))
    }

    /// The most recent proposed edit of a file, for the permission prompt.
    ///
    /// # Arguments
    ///
    /// * `target` - The path as the tool call gave it.
    ///
    /// # Returns
    ///
    /// The diff, if an edit of that file was proposed.
    pub fn last_edit_for(&self, target: &str) -> Option<Diff> {
        let shown = self.display_path(&self.cwd.join(target));
        self.slots.iter().rev().find_map(|slot| match &slot.entry {
            Entry::Edit { shown_path, diff } if *shown_path == shown => Some(diff.clone()),
            _ => None,
        })
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
                Entry::Welcome(_) => "welcome".to_owned(),
                Entry::User { text } => format!("> {text}"),
                Entry::Model { doc, .. } => doc.text().to_owned(),
                Entry::Tool(tool) => format!("{} {}", tool.call.name, tool.summary),
                Entry::Edit { shown_path, .. } => format!("edit {shown_path}"),
                Entry::Banner { state, visit, .. } => banner_title(*state, *visit),
                Entry::Plan(plan) => plan
                    .items
                    .iter()
                    .map(|item| format!("[{}] {}", if item.done { "x" } else { " " }, item.text))
                    .collect::<Vec<_>>()
                    .join("\n"),
                Entry::Check { command, .. } => format!("check {command}"),
                Entry::Review { .. } => "review".to_owned(),
                Entry::Notice { text, .. } => text.clone(),
                Entry::Answer { question, answer } => format!("? {question} {answer}"),
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
    /// * `theme` - Styles, already dimmed when a prompt is open.
    /// * `tick` - The spinner frame counter.
    /// * `dimmed` - Whether `theme` is the dimmed one, for the cache.
    /// * `now` - The current time, for the startup animation.
    pub fn draw(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        theme: &Theme,
        tick: u64,
        dimmed: bool,
        now: Instant,
    ) {
        let lines = self.lines(area.width, theme, tick, dimmed, now);
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

    /// Marks the open banner as ended.
    fn end_banner(&mut self, now: Instant) {
        if let Some(slot) = self
            .slots
            .iter_mut()
            .rev()
            .find(|slot| matches!(slot.entry, Entry::Banner { .. }))
            && let Entry::Banner { started, ended, .. } = &mut slot.entry
            && ended.is_none()
        {
            *ended = Some(now.saturating_duration_since(*started));
            slot.cache = None;
        }
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
    fn lines(
        &mut self,
        width: u16,
        theme: &Theme,
        tick: u64,
        dimmed: bool,
        now: Instant,
    ) -> Vec<Line<'static>> {
        let mut out = Vec::new();
        let mut layout = Vec::new();
        let hidden_before = self.hidden_before;
        let selected = self.selected;
        let key = (width, dimmed);
        let mut previous: Option<&Entry> = None;
        for (index, slot) in self.slots.iter_mut().enumerate().skip(hidden_before) {
            if let Some(previous) = previous
                && separated(previous, &slot.entry)
            {
                out.push(Line::default());
            }
            let live = is_live(&slot.entry, now);
            if live || slot.cache.as_ref().is_none_or(|(k, _)| *k != key) {
                let lines = render_entry(&mut slot.entry, width, theme, tick, now);
                slot.cache = Some((key, lines));
            }
            // A live entry's lines are stale by the next draw; render it again then.
            let keep = !live;
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
            if !keep {
                slot.cache = None;
            }
            previous = Some(&slot.entry);
        }
        self.layout = layout;
        out
    }
}

/// Whether a blank line goes between two entries: not between one-liners,
/// and not between a tool call and the edit it proposes.
fn separated(previous: &Entry, next: &Entry) -> bool {
    if matches!(previous, Entry::Tool(_)) && matches!(next, Entry::Edit { .. }) {
        return false;
    }
    !(is_compact(previous) && is_compact(next))
}

/// Whether an entry is a one-liner that sits directly under another one.
fn is_compact(entry: &Entry) -> bool {
    matches!(
        entry,
        Entry::Tool(ToolEntry {
            expanded: false,
            ..
        }) | Entry::Check { .. }
    )
}

/// Whether an entry changes without an event, so it must not be cached.
fn is_live(entry: &Entry, now: Instant) -> bool {
    match entry {
        Entry::Welcome(welcome) => welcome.is_animating(now),
        Entry::Tool(ToolEntry {
            status: ToolStatus::Running { .. },
            ..
        })
        | Entry::Check {
            status: CheckStatus::Running,
            ..
        } => true,
        _ => false,
    }
}

/// Renders one entry, without the blank line that separates entries.
fn render_entry(
    entry: &mut Entry,
    width: u16,
    theme: &Theme,
    tick: u64,
    now: Instant,
) -> Vec<Line<'static>> {
    let columns = usize::from(width).max(1);
    let mut lines = match entry {
        Entry::Welcome(welcome) => welcome.lines(columns, theme, now),
        Entry::User { text } => {
            let lead_width = YOU.width() + 2;
            wrap(text, columns.saturating_sub(lead_width).max(1))
                .into_iter()
                .enumerate()
                .map(|(index, line)| {
                    let lead = if index == 0 {
                        Span::styled(format!("{YOU}  "), theme.bold(Shade::Accent2))
                    } else {
                        Span::raw(" ".repeat(lead_width))
                    };
                    Line::from(vec![lead, Span::styled(line, theme.fg(Shade::Text))])
                })
                .collect::<Vec<_>>()
        }
        Entry::Model { doc, open } => {
            let mut lines = doc.lines(width, theme);
            while lines.last().is_some_and(|line| line.width() == 0) {
                lines.pop();
            }
            if *open {
                append_cursor(&mut lines, columns, theme);
            }
            lines
        }
        Entry::Tool(tool) => render_tool(tool, columns, theme, tick),
        Entry::Edit { shown_path, diff } => render_edit(shown_path, diff, columns, theme),
        Entry::Banner {
            state,
            visit,
            ended,
            ..
        } => vec![render_banner(*state, *visit, *ended, columns, theme)],
        Entry::Plan(plan) => render_plan(plan, columns, theme),
        Entry::Check { command, status } => render_check(command, status, columns, theme, tick),
        Entry::Review { verdict } => match verdict {
            ReviewVerdict::Approve => {
                vec![Line::styled("review: approved", theme.fg(Shade::Muted))]
            }
            ReviewVerdict::Changes { items } => {
                let mut lines = vec![Line::styled(
                    "review: changes requested",
                    theme.fg(Shade::Secondary),
                )];
                for item in items {
                    lines.extend(
                        wrap(item, columns.saturating_sub(4).max(1))
                            .into_iter()
                            .enumerate()
                            .map(|(index, line)| {
                                let lead = if index == 0 { "  - " } else { "    " };
                                Line::styled(format!("{lead}{line}"), theme.fg(Shade::Text))
                            }),
                    );
                }
                lines
            }
        },
        Entry::Notice { text, level } => {
            let style = match level {
                Level::Info => theme.fg(Shade::Muted),
                Level::Warn => theme.fg(Shade::Secondary),
                Level::Error => theme.fg(Shade::Danger),
            };
            wrap(text, columns)
                .into_iter()
                .map(|line| Line::styled(line, style))
                .collect()
        }
        Entry::Answer { question, answer } => {
            let mut lines: Vec<Line<'static>> = wrap(question, columns.saturating_sub(2).max(1))
                .into_iter()
                .enumerate()
                .map(|(index, line)| {
                    let lead = if index == 0 { "? " } else { "  " };
                    Line::styled(format!("{lead}{line}"), theme.fg(Shade::Muted))
                })
                .collect();
            lines.extend(
                wrap(answer, columns.saturating_sub(2).max(1))
                    .into_iter()
                    .map(|line| Line::styled(format!("  {line}"), theme.fg(Shade::Secondary))),
            );
            lines
        }
    };
    while lines.last().is_some_and(|line| line.width() == 0) {
        lines.pop();
    }
    lines
}

/// Adds the block cursor after the last line of streaming text.
fn append_cursor(lines: &mut Vec<Line<'static>>, columns: usize, theme: &Theme) {
    let cursor = Span::styled(CURSOR, theme.fg(Shade::Text));
    match lines.last_mut() {
        Some(last) if last.width() < columns => last.spans.push(cursor),
        _ => lines.push(Line::from(cursor)),
    }
}

/// A state banner: the name, a rule to the edge and, once the state
/// ended, how long it took.
fn render_banner(
    state: State,
    visit: u32,
    ended: Option<Duration>,
    columns: usize,
    theme: &Theme,
) -> Line<'static> {
    let title = format!("{} ", banner_title(state, visit));
    let done = ended.map(|duration| format!(" done in {}", seconds_label(duration)));
    let tail_width = done.as_ref().map_or(0, |done| done.width());
    let rule = "─".repeat(columns.saturating_sub(title.width() + tail_width));
    let mut spans = vec![
        Span::styled(title, theme.bold(Shade::Accent)),
        Span::styled(rule, theme.border()),
    ];
    if let Some(done) = done {
        spans.push(Span::styled(done, theme.fg(Shade::Muted)));
    }
    Line::from(spans)
}

/// The collapsed tool line, plus the arguments and result when expanded.
fn render_tool(tool: &ToolEntry, width: usize, theme: &Theme, tick: u64) -> Vec<Line<'static>> {
    let _ = tick;
    let (status, status_style, duration) = match &tool.status {
        ToolStatus::Running { .. } => ("..".to_owned(), theme.fg(Shade::Accent), String::new()),
        ToolStatus::Finished {
            outcome, duration, ..
        } => {
            let style = if *outcome == ToolOutcome::Completed {
                theme.fg(Shade::Success)
            } else {
                theme.fg(Shade::Danger)
            };
            (
                outcome_mark(*outcome).to_owned(),
                style,
                duration_label(*duration),
            )
        }
        ToolStatus::Cancelled { duration } => (
            "cancelled".to_owned(),
            theme.fg(Shade::Muted),
            duration_label(*duration),
        ),
    };
    let status_text = format!("{status:<STATUS_WIDTH$} ");
    let name = format!("{}  ", tool.call.name);
    let used = status_text.width() + name.width();
    let summary = fit_text(
        &tool.summary,
        width.saturating_sub(used + duration.width() + 2),
    );
    let gap = width.saturating_sub(used + summary.width() + duration.width());
    let mut spans = vec![
        Span::styled(status_text, status_style),
        Span::styled(name, theme.fg(Shade::Secondary)),
        Span::styled(summary, theme.fg(Shade::Muted)),
    ];
    if !duration.is_empty() {
        spans.push(Span::raw(" ".repeat(gap)));
        spans.push(Span::styled(duration, theme.fg(Shade::Muted)));
    }
    let mut lines = vec![Line::from(spans)];
    if !tool.expanded {
        return lines;
    }
    let indent = " ".repeat(DETAIL_INDENT);
    let inner = width.saturating_sub(DETAIL_INDENT).max(1);
    lines.push(Line::styled("    args:", theme.fg(Shade::Muted)));
    let args = serde_json::from_str::<serde_json::Value>(&tool.call.arguments_json)
        .and_then(|value| serde_json::to_string_pretty(&value))
        .unwrap_or_else(|_| tool.call.arguments_json.clone());
    for line in args.lines() {
        for (_, segment) in segments(line, inner) {
            lines.push(Line::styled(
                format!("{indent}{segment}"),
                theme.fg(Shade::Text),
            ));
        }
    }
    match &tool.result {
        Some((text, is_error)) => {
            let (title, style) = if *is_error {
                ("    result (error):", theme.fg(Shade::Danger))
            } else {
                ("    result:", theme.fg(Shade::Muted))
            };
            lines.push(Line::styled(title, style));
            for line in text.lines() {
                for (_, segment) in segments(line, inner) {
                    lines.push(Line::styled(
                        format!("{indent}{segment}"),
                        theme.fg(Shade::Text),
                    ));
                }
            }
        }
        None => {
            let preview = match &tool.status {
                ToolStatus::Finished { preview, .. } if !preview.is_empty() => preview.as_str(),
                _ => "(not yet)",
            };
            lines.push(Line::styled("    result:", theme.fg(Shade::Muted)));
            for (_, segment) in segments(preview, inner) {
                lines.push(Line::styled(
                    format!("{indent}{segment}"),
                    theme.fg(Shade::Text),
                ));
            }
        }
    }
    lines
}

/// A proposed edit: the path and line range, then the diff rows behind a
/// left rule, each colored line filled to the edge.
fn render_edit(shown_path: &str, diff: &Diff, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let rule = Span::styled("▌ ", theme.border());
    let range = if diff.start_line == diff.end_line {
        format!("{shown_path}:{}", diff.start_line)
    } else {
        format!("{shown_path}:{}-{}", diff.start_line, diff.end_line)
    };
    let mut lines = vec![Line::from(vec![
        rule.clone(),
        Span::styled(range, theme.fg(Shade::Muted)),
    ])];
    for line in diff_lines(diff, width.saturating_sub(2).max(1), theme) {
        let style = line.style;
        let mut spans = vec![rule.clone()];
        spans.extend(line.spans.into_iter().map(|span| span.patch_style(style)));
        lines.push(Line::from(spans));
    }
    lines
}

/// The rows of a diff as lines: context muted, removed and added lines on
/// their backgrounds, each filled to `width`.
///
/// # Arguments
///
/// * `diff` - The change.
/// * `width` - Columns.
/// * `theme` - Styles.
///
/// # Returns
///
/// The lines; one faint `(no change)` line for an empty diff.
pub fn diff_lines(diff: &Diff, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let inner = width.saturating_sub(2).max(1);
    let mut lines = Vec::new();
    for row in &diff.rows {
        let (mark, style) = match row.kind {
            RowKind::Context => (' ', theme.fg(Shade::Muted)),
            RowKind::Removed => ('-', theme.removed()),
            RowKind::Added => ('+', theme.added()),
        };
        for (_, segment) in segments(&row.text, inner) {
            let text = format!("{mark} {segment}");
            let pad = " ".repeat(width.saturating_sub(text.width()));
            lines.push(Line::styled(format!("{text}{pad}"), style));
        }
    }
    if lines.is_empty() {
        lines.push(Line::styled("(no change)", theme.fg(Shade::Faint)));
    }
    lines
}

/// The plan in a bordered box: title, assumptions, numbered steps with
/// their marks, and a summary line.
fn render_plan(plan: &PlanBlock, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let inner = width.saturating_sub(4).max(1);
    let mut body = vec![Line::styled(
        plan.title.clone().unwrap_or_else(|| "Plan".to_owned()),
        theme.bold(Shade::Text),
    )];
    for note in &plan.notes {
        body.extend(render_note(note, inner, theme));
    }
    let current = plan.items.iter().position(|item| !item.done);
    for (index, item) in plan.items.iter().enumerate() {
        let number = format!("{}. ", index + 1);
        let (mark, mark_style) = if item.done {
            ("[x]", theme.fg(Shade::Success))
        } else if current == Some(index) {
            ("[>]", theme.fg(Shade::Accent))
        } else {
            ("[ ]", theme.fg(Shade::Faint))
        };
        let lead_width = number.width() + mark.width() + 1;
        let text = inline_lines(&item.text, inner.saturating_sub(lead_width).max(1), theme);
        for (line_index, line) in text.into_iter().enumerate() {
            let mut spans = if line_index == 0 {
                vec![
                    Span::styled(number.clone(), theme.fg(Shade::Faint)),
                    Span::styled(mark, mark_style),
                    Span::raw(" "),
                ]
            } else {
                vec![Span::raw(" ".repeat(lead_width))]
            };
            spans.extend(line.spans);
            body.push(Line::from(spans));
        }
    }
    body.push(Line::styled(
        format!(
            "{} · {} · {}",
            count_label(plan.files, "file"),
            count_label(plan.lines, "line"),
            plan.shown_path
        ),
        theme.fg(Shade::Faint),
    ));
    boxed(body, width, theme)
}

/// A plan note; an `Assumption:` line shows its label muted.
fn render_note(note: &str, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let lower = note.to_ascii_lowercase();
    if lower.starts_with("assumption")
        && let Some(colon) = note.find(':')
    {
        let (label, rest) = note.split_at(colon + 1);
        let label = format!("{label} ");
        return inline_lines(
            rest.trim(),
            width.saturating_sub(label.width()).max(1),
            theme,
        )
        .into_iter()
        .enumerate()
        .map(|(index, line)| {
            let lead = if index == 0 {
                Span::styled(label.clone(), theme.fg(Shade::Muted))
            } else {
                Span::raw(" ".repeat(label.width()))
            };
            let mut spans = vec![lead];
            spans.extend(line.spans);
            Line::from(spans)
        })
        .collect();
    }
    wrap(note, width)
        .into_iter()
        .map(|line| Line::styled(line, theme.fg(Shade::Muted)))
        .collect()
}

/// Wraps lines in a rounded border, padding each to the inner width.
fn boxed(body: Vec<Line<'static>>, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let inner = width.saturating_sub(4).max(1);
    let mut out = vec![Line::styled(
        format!("╭{}╮", "─".repeat(width.saturating_sub(2))),
        theme.border(),
    )];
    for line in body {
        let pad = " ".repeat(inner.saturating_sub(line.width()));
        let mut spans = vec![Span::styled("│ ", theme.border())];
        spans.extend(line.spans);
        spans.push(Span::styled(format!("{pad} │"), theme.border()));
        out.push(Line::from(spans));
    }
    out.push(Line::styled(
        format!("╰{}╯", "─".repeat(width.saturating_sub(2))),
        theme.border(),
    ));
    out
}

/// A project check: running, passed, or failed with its output tail.
fn render_check(
    command: &str,
    status: &CheckStatus,
    width: usize,
    theme: &Theme,
    tick: u64,
) -> Vec<Line<'static>> {
    let _ = tick;
    match status {
        CheckStatus::Running => vec![Line::from(vec![
            Span::styled(format!("{:<STATUS_WIDTH$} ", ".."), theme.fg(Shade::Accent)),
            Span::styled(format!("check  {command}"), theme.fg(Shade::Secondary)),
        ])],
        CheckStatus::Passed => vec![Line::from(vec![
            Span::styled(
                format!("{:<STATUS_WIDTH$} ", "ok"),
                theme.fg(Shade::Success),
            ),
            Span::styled(format!("check  {command}"), theme.fg(Shade::Secondary)),
        ])],
        CheckStatus::Failed { tail } => {
            let mut lines = vec![Line::from(vec![
                Span::styled(
                    format!("{:<STATUS_WIDTH$} ", "failed"),
                    theme.fg(Shade::Danger),
                ),
                Span::styled(format!("check  {command}"), theme.fg(Shade::Secondary)),
            ])];
            for line in tail.lines() {
                for (_, segment) in segments(line, width.saturating_sub(4).max(1)) {
                    lines.push(Line::styled(
                        format!("    {segment}"),
                        theme.fg(Shade::Muted),
                    ));
                }
            }
            lines
        }
    }
}

/// Parses `plan.md` into its title, checkbox items, notes and counts.
fn parse_plan(path: PathBuf, shown_path: String, text: &str) -> PlanBlock {
    let mut block = PlanBlock {
        path,
        shown_path,
        title: None,
        items: Vec::new(),
        notes: Vec::new(),
        files: count_files(text),
        lines: text.lines().count(),
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

/// How many distinct backticked tokens look like file paths.
fn count_files(text: &str) -> usize {
    let mut files: Vec<&str> = Vec::new();
    for (index, part) in text.split('`').enumerate() {
        let inside = index % 2 == 1;
        if inside
            && !part.contains(char::is_whitespace)
            && (part.contains('/') || part.contains('.'))
            && !files.contains(&part)
        {
            files.push(part);
        }
    }
    files.len()
}

/// `1 file` or `3 files`.
fn count_label(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
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

/// `41s` or `2m05s`, for how long a state ran.
fn seconds_label(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else {
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
        ToolOutcome::DeniedByRule => "denied",
        ToolOutcome::DeclinedByUser => "declined",
        ToolOutcome::Blocked => "blocked",
        ToolOutcome::UnknownTool => "unknown",
        ToolOutcome::InvalidArguments => "bad args",
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
            .lines(width, &Theme::mono(), 0, false, Instant::now())
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
        assert!(joined(&mut transcript, 40).starts_with("hello█"));
        transcript.close_model();
        transcript.append_text("again");
        assert_eq!(transcript.entries().len(), 2);
        assert_eq!(transcript.plain_text(), "hello\nagain");
        assert!(joined(&mut transcript, 40).starts_with("hello\n"));
    }

    #[test]
    fn user_lines_carry_the_you_label() {
        let mut transcript = fresh();
        transcript.push(Entry::User {
            text: "run the tests please".to_owned(),
        });
        let text = joined(&mut transcript, 20);
        assert_eq!(text, "you  run the tests\n     please");
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
            let collapsed = joined(&mut transcript, 40);
            assert_eq!(
                collapsed, "ok     shell  ls                   340ms",
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
        assert!(text.starts_with("failed shell"), "{text:?}");
        assert!(text.contains("result (error):"), "{text:?}");
    }

    #[test]
    fn plan_ticks_update_in_place() {
        let mut transcript = fresh();
        let path = PathBuf::from("/project/plan.md");
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
            text.starts_with(
                "╭──────────────────────────────────────╮\n\
                 │ Plan                                 │\n\
                 │ note                                 │\n\
                 │ 1. [x] first                         │\n\
                 │ 2. [>] second                        │\n\
                 │ 0 files · 5 lines · plan.md          │\n\
                 ╰──────────────────────────────────────╯"
            ),
            "{text}"
        );
    }

    #[test]
    fn plan_marks_the_current_step() {
        let theme = Theme::truecolor();
        let mut transcript = fresh();
        transcript.set_plan(
            PathBuf::from("/project/plan.md"),
            "# Plan\n\nAssumption: `src/main.rs` is the entry point.\n\n\
             1. [x] Read `src/main.rs`\n2. [ ] Edit `src/lib.rs`\n3. [ ] Run `cargo test`\n",
        );
        let lines = transcript.lines(60, &theme, 0, false, Instant::now());
        let marks: Vec<(String, Option<ratatui::style::Color>)> = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .filter(|span| span.content.starts_with('['))
            .map(|span| (span.content.to_string(), span.style.fg))
            .collect();
        assert_eq!(
            marks,
            vec![
                ("[x]".to_owned(), Some(theme.success)),
                ("[>]".to_owned(), Some(theme.accent)),
                ("[ ]".to_owned(), Some(theme.faint)),
            ]
        );
        let code = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content == "src/lib.rs")
            .expect("inline code span");
        assert_eq!(code.style.fg, Some(theme.accent2));
        let text = lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("│ Assumption: src/main.rs is the entry point."),
            "{text}"
        );
        assert!(text.contains("2 files · 7 lines · plan.md"), "{text}");
    }

    #[test]
    fn banner_shows_done_in_when_the_state_ends() {
        let now = Instant::now();
        let theme = Theme::truecolor();
        let mut transcript = fresh();
        transcript.apply(
            &LoopEvent::StateEntered {
                state: State::Plan,
                visit: 1,
            },
            now,
        );
        let open = transcript.lines(40, &theme, 0, false, Instant::now());
        assert_eq!(
            open[0].to_string(),
            "PLAN ───────────────────────────────────"
        );
        assert_eq!(open[0].spans[0].style.fg, Some(theme.accent));
        assert_eq!(open[0].spans[1].style.fg, Some(theme.border));
        transcript.apply(
            &LoopEvent::StateEntered {
                state: State::Build,
                visit: 1,
            },
            now + Duration::from_secs(41),
        );
        let lines = transcript.lines(40, &theme, 0, false, Instant::now());
        assert_eq!(
            lines[0].to_string(),
            "PLAN ─────────────────────── done in 41s"
        );
        assert_eq!(lines[0].spans[2].style.fg, Some(theme.muted));
        assert_eq!(
            lines[2].to_string(),
            "BUILD ──────────────────────────────────"
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
        assert_eq!(text, "failed check  cargo test\n    boom");
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
    fn cancel_running_ends_the_open_text_tools_and_checks() {
        let now = Instant::now();
        let mut transcript = fresh();
        transcript.append_text("half a thought");
        transcript.apply(
            &LoopEvent::ToolStarted {
                call: call("a", "shell", "{\"command\":\"sleep 9\"}"),
            },
            now,
        );
        transcript.apply(
            &LoopEvent::CheckStarted {
                command: "cargo test".to_owned(),
            },
            now,
        );
        transcript.cancel_running(now + Duration::from_millis(340));
        transcript.append_text("later");
        assert_eq!(transcript.entries().len(), 4);
        let text = joined(&mut transcript, 60);
        assert!(text.contains("cancelled shell  sleep 9"), "{text:?}");
        assert!(text.contains("340ms"), "{text:?}");
        assert!(
            text.contains("failed check  cargo test\n    cancelled"),
            "{text:?}"
        );
        assert!(
            !transcript.entries().iter().any(|entry| is_live(entry, now)),
            "nothing keeps spinning"
        );
    }

    #[test]
    fn edits_render_as_a_diff_block_under_their_tool_line() {
        let now = Instant::now();
        let theme = Theme::truecolor();
        let mut transcript = fresh();
        transcript.apply(
            &LoopEvent::ToolStarted {
                call: call(
                    "e1",
                    "edit_file",
                    "{\"path\":\"src/lib.rs\",\"old\":\"fn b() {}\",\"new\":\"fn beta() {}\"}",
                ),
            },
            now,
        );
        transcript.apply(
            &LoopEvent::ProposedEdit {
                path: PathBuf::from("/project/src/lib.rs"),
                old: "fn a() {}\r\nfn b() {}\r\nfn c() {}\r\n".to_owned(),
                new: "fn a() {}\r\nfn beta() {}\r\nfn c() {}\r\n".to_owned(),
            },
            now,
        );
        let lines = transcript.lines(40, &theme, 0, false, now);
        let text: Vec<String> = lines.iter().map(ToString::to_string).collect();
        assert_eq!(
            text,
            vec![
                "..     edit_file  src/lib.rs".to_owned(),
                "▌ src/lib.rs:1-3".to_owned(),
                "▌   fn a() {}                           ".to_owned(),
                "▌ - fn b() {}                           ".to_owned(),
                "▌ + fn beta() {}                        ".to_owned(),
                "▌   fn c() {}                           ".to_owned(),
            ]
        );
        assert_eq!(lines[1].spans[0].style.fg, Some(theme.border));
        assert_eq!(lines[3].spans[1].style.bg, Some(theme.removed_bg));
        assert_eq!(lines[3].spans[1].style.fg, Some(theme.danger));
        assert_eq!(lines[4].spans[1].style.bg, Some(theme.added_bg));
        assert_eq!(lines[4].spans[1].style.fg, Some(theme.success));
        assert_eq!(lines[2].spans[1].style.fg, Some(theme.muted));
    }

    #[test]
    fn clear_view_hides_but_keeps_entries() {
        let mut transcript = fresh();
        transcript.notice("one", Level::Info);
        transcript.clear_view();
        transcript.notice("two", Level::Info);
        assert_eq!(transcript.entries().len(), 2);
        assert_eq!(joined(&mut transcript, 40), "two");
    }

    #[test]
    fn dimmed_lines_are_cached_apart_from_bright_ones() {
        let theme = Theme::truecolor();
        let mut transcript = fresh();
        transcript.notice("hello", Level::Error);
        let bright = transcript.lines(40, &theme, 0, false, Instant::now());
        let dim = transcript.lines(40, &theme.dimmed(), 0, true, Instant::now());
        let bright_again = transcript.lines(40, &theme, 0, false, Instant::now());
        assert_eq!(bright[0].style.fg, Some(theme.danger));
        assert_eq!(dim[0].style.fg, Some(theme.muted));
        assert_eq!(bright_again[0].style.fg, Some(theme.danger));
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
        assert_eq!(seconds_label(Duration::from_millis(41_900)), "41s");
        assert_eq!(seconds_label(Duration::from_secs(125)), "2m05s");
        assert_eq!(fit_text("abcdef", 4), "abc…");
        assert_eq!(fit_text("abc", 4), "abc");
        assert_eq!(
            count_files("see `a/b.rs`, `a/b.rs` and `c.txt` but not `cargo test`"),
            2
        );
    }
}
