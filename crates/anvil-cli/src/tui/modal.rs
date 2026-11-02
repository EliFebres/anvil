//! Prompts drawn over the transcript: tool permission, memory keep/drop and
//! the key help. While one is open every key goes to it, the screen behind
//! it is dimmed, and nothing else changes until it is answered.

use anvil_core::tools::shell::DEFAULT_TIMEOUT_S;
use anvil_core::tools::{DEFAULT_OUTPUT_CAP, PermissionRequest, PromptAnswer};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};
use tokio::sync::oneshot;
use unicode_width::UnicodeWidthStr;

use super::MemoryAnswer;
use super::diff::Diff;
use super::keys;
use super::theme::{Shade, Theme};
use super::transcript::diff_lines;
use super::wrap::{segments, wrap};

/// The prompt's share of the screen width.
const WIDTH_PERCENT: u16 = 60;

/// Rows the prompt needs besides the payload: borders, context, gaps,
/// the inset box's borders, the limits line and the choices line.
const FIXED_ROWS: u16 = 10;

/// A prompt waiting for the user.
#[derive(Debug)]
pub enum Modal {
    /// May this tool call run?
    Permission {
        /// What is about to run.
        request: PermissionRequest,
        /// The change it would make, when the call edits a file.
        edit: Option<Diff>,
        /// Where the answer goes.
        reply: oneshot::Sender<PromptAnswer>,
        /// First payload line shown.
        scroll: usize,
    },
    /// Should this fact be remembered?
    Memory {
        /// The topic it would go under.
        topic: String,
        /// The fact.
        fact: String,
        /// Where the answer goes.
        reply: oneshot::Sender<MemoryAnswer>,
    },
    /// The key help.
    Help {
        /// Whether the session is a run, which takes only slash commands.
        run: bool,
        /// First payload line shown.
        scroll: usize,
    },
}

/// What a prompt was answered with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// A permission answer.
    Permission(PromptAnswer),
    /// A memory answer.
    Memory(MemoryAnswer),
    /// The help was closed.
    Help,
}

impl Answer {
    /// The answer as the transcript records it.
    ///
    /// # Returns
    ///
    /// A short label.
    pub fn label(&self) -> String {
        match self {
            Answer::Permission(PromptAnswer::Yes) => "allow".to_owned(),
            Answer::Permission(PromptAnswer::No) => "deny".to_owned(),
            Answer::Permission(PromptAnswer::Always) => "always".to_owned(),
            Answer::Memory(MemoryAnswer::Keep) => "keep".to_owned(),
            Answer::Memory(MemoryAnswer::Drop) => "drop".to_owned(),
            Answer::Help => String::new(),
        }
    }
}

/// One `key description` choice on the last line of a prompt.
struct Choice {
    key: &'static str,
    label: &'static str,
    shade: Shade,
}

impl Modal {
    /// The permission prompt for a call.
    ///
    /// # Arguments
    ///
    /// * `request` - What is about to run.
    /// * `edit` - The change it would make, when known.
    /// * `reply` - Where the answer goes.
    ///
    /// # Returns
    ///
    /// The prompt.
    pub fn permission(
        request: PermissionRequest,
        edit: Option<Diff>,
        reply: oneshot::Sender<PromptAnswer>,
    ) -> Self {
        Modal::Permission {
            request,
            edit,
            reply,
            scroll: 0,
        }
    }

    /// The keep/drop prompt for a proposed fact.
    ///
    /// # Arguments
    ///
    /// * `topic` - The topic it would go under.
    /// * `fact` - The fact.
    /// * `reply` - Where the answer goes.
    ///
    /// # Returns
    ///
    /// The prompt.
    pub fn memory(topic: String, fact: String, reply: oneshot::Sender<MemoryAnswer>) -> Self {
        Modal::Memory { topic, fact, reply }
    }

    /// The key help.
    ///
    /// # Arguments
    ///
    /// * `run` - Whether the session is a run.
    ///
    /// # Returns
    ///
    /// The prompt.
    pub fn help(run: bool) -> Self {
        Modal::Help { run, scroll: 0 }
    }

    /// The question, as the transcript records it; the help has none.
    ///
    /// # Returns
    ///
    /// The question text.
    pub fn question(&self) -> Option<String> {
        match self {
            Modal::Permission { request, .. } => {
                Some(format!("allow {} {}?", request.tool, target_text(request)))
            }
            Modal::Memory { topic, fact, .. } => Some(format!("keep [{topic}] {fact}?")),
            Modal::Help { .. } => None,
        }
    }

    /// The answer that does nothing irreversible: deny or drop.
    ///
    /// # Returns
    ///
    /// The answer.
    pub fn safe_answer(&self) -> Answer {
        match self {
            Modal::Permission { .. } => Answer::Permission(PromptAnswer::No),
            Modal::Memory { .. } => Answer::Memory(MemoryAnswer::Drop),
            Modal::Help { .. } => Answer::Help,
        }
    }

    /// Handles a key.
    ///
    /// # Arguments
    ///
    /// * `key` - The key.
    ///
    /// # Returns
    ///
    /// The answer when the key closes the prompt.
    pub fn key(&mut self, key: KeyEvent) -> Option<Answer> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        match self {
            Modal::Permission { .. } => match key.code {
                KeyCode::Char('y' | 'Y') if plain => Some(Answer::Permission(PromptAnswer::Yes)),
                KeyCode::Char('n' | 'N') if plain => Some(Answer::Permission(PromptAnswer::No)),
                KeyCode::Char('a' | 'A') if plain => Some(Answer::Permission(PromptAnswer::Always)),
                KeyCode::Esc => Some(Answer::Permission(PromptAnswer::No)),
                code => {
                    self.scroll_key(code);
                    None
                }
            },
            Modal::Memory { .. } => match key.code {
                KeyCode::Char('k' | 'K') if plain => Some(Answer::Memory(MemoryAnswer::Keep)),
                KeyCode::Char('d' | 'D') if plain => Some(Answer::Memory(MemoryAnswer::Drop)),
                KeyCode::Esc => Some(Answer::Memory(MemoryAnswer::Drop)),
                _ => None,
            },
            Modal::Help { .. } => match key.code {
                code @ (KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown) => {
                    self.scroll_key(code);
                    None
                }
                _ => Some(Answer::Help),
            },
        }
    }

    /// Scrolls the payload by a mouse wheel notch.
    ///
    /// # Arguments
    ///
    /// * `up` - Whether the wheel moved toward the top.
    pub fn wheel(&mut self, up: bool) {
        self.scroll_by(if up { -3 } else { 3 });
    }

    /// Sends the answer to whoever asked, consuming the prompt. An answer
    /// of another kind drops the reply, which the asker reads as the safe
    /// answer.
    ///
    /// # Arguments
    ///
    /// * `answer` - The answer.
    pub fn reply(self, answer: Answer) {
        match (self, answer) {
            (Modal::Permission { reply, .. }, Answer::Permission(answer)) => {
                let _ = reply.send(answer);
            }
            (Modal::Memory { reply, .. }, Answer::Memory(answer)) => {
                let _ = reply.send(answer);
            }
            _ => {}
        }
    }

    /// Draws the prompt centered over `area`: a box on the raised surface
    /// with the title, a line of context, the payload in an inset box,
    /// the limits and the choices.
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame.
    /// * `area` - The whole screen.
    /// * `theme` - Styles, at full contrast.
    ///
    /// # Returns
    ///
    /// The rectangle the prompt covered.
    pub fn draw(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) -> Rect {
        let width = (area.width * WIDTH_PERCENT / 100).clamp(30.min(area.width), area.width);
        let inner_width = usize::from(width.saturating_sub(4)).max(1);
        let payload_width = inner_width.saturating_sub(4).max(1);
        let payload = self.payload(payload_width, theme);
        let max_payload = usize::from(area.height.saturating_sub(FIXED_ROWS).max(1));
        let shown = payload.len().clamp(1, max_payload);
        let scroll = self.clamp_scroll(payload.len().saturating_sub(shown));
        let height = u16::try_from(shown)
            .unwrap_or(u16::MAX)
            .saturating_add(FIXED_ROWS)
            .min(area.height);
        let rect = centered(area, width, height);
        let surface = theme.on_surface(theme.fg(Shade::Text));
        frame.render_widget(Clear, rect);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(theme.on_surface(theme.fg(Shade::Accent)))
            .style(surface)
            .title(Span::styled(
                format!(" {} ", self.title()),
                theme.on_surface(theme.bold(Shade::Accent)),
            ));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        let padded = Rect {
            x: inner.x.saturating_add(1),
            width: inner.width.saturating_sub(2),
            ..inner
        };
        let [context_area, _, box_area, _, limits_area, _, choices_area] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(padded);
        frame.render_widget(
            Paragraph::new(Line::styled(self.context(), theme.fg(Shade::Muted))),
            context_area,
        );
        let inset = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(theme.on_bg(theme.border()))
            .style(theme.on_bg(theme.fg(Shade::Text)));
        let payload_area = inset.inner(box_area);
        frame.render_widget(inset, box_area);
        let visible: Vec<Line<'static>> = payload
            .into_iter()
            .skip(scroll)
            .take(usize::from(payload_area.height))
            .collect();
        let payload_area = Rect {
            x: payload_area.x.saturating_add(1),
            width: payload_area.width.saturating_sub(2),
            ..payload_area
        };
        frame.render_widget(Paragraph::new(visible), payload_area);
        frame.render_widget(
            Paragraph::new(Line::styled(self.limits(), theme.fg(Shade::Muted))),
            limits_area,
        );
        frame.render_widget(
            Paragraph::new(self.choices_line(usize::from(choices_area.width), theme)),
            choices_area,
        );
        rect
    }

    /// The title in the border.
    fn title(&self) -> &'static str {
        match self {
            Modal::Permission { .. } => "Permission",
            Modal::Memory { .. } => "Remember these?",
            Modal::Help { .. } => "Keys",
        }
    }

    /// The muted line under the title.
    fn context(&self) -> String {
        match self {
            Modal::Permission { request, edit, .. } => match (request.tool.as_str(), edit) {
                ("shell", _) => "shell wants to run".to_owned(),
                (tool, Some(_)) => format!("{tool} wants to change {}", target_text(request)),
                (tool, None) => format!("{tool} wants to run with"),
            },
            Modal::Memory { topic, .. } => format!("proposed from this session, under {topic}"),
            Modal::Help { run: true, .. } => {
                "during a run the input takes /expand, /help and /quit".to_owned()
            }
            Modal::Help { run: false, .. } => "what the keys do".to_owned(),
        }
    }

    /// The payload lines at a width.
    fn payload(&self, width: usize, theme: &Theme) -> Vec<Line<'static>> {
        match self {
            Modal::Permission { request, edit, .. } => {
                if let Some(diff) = edit {
                    return diff_lines(diff, width, theme);
                }
                if request.tool == "shell"
                    && let Some(target) = &request.target
                {
                    return segments(target, width)
                        .into_iter()
                        .map(|(_, segment)| Line::styled(segment, theme.fg(Shade::Text)))
                        .collect();
                }
                let args = serde_json::to_string_pretty(&request.arguments)
                    .unwrap_or_else(|_| request.arguments.to_string());
                args.lines()
                    .flat_map(|line| segments(line, width))
                    .map(|(_, segment)| Line::styled(segment, theme.fg(Shade::Text)))
                    .collect()
            }
            Modal::Memory { fact, .. } => wrap(fact, width)
                .into_iter()
                .map(|line| Line::styled(line, theme.fg(Shade::Text)))
                .collect(),
            Modal::Help { .. } => {
                let key_width = keys::HELP
                    .iter()
                    .map(|(key, _)| key.width())
                    .max()
                    .unwrap_or(0);
                keys::HELP
                    .iter()
                    .map(|(key, meaning)| {
                        Line::from(vec![
                            Span::styled(format!("{key:<key_width$}"), theme.bold(Shade::Text)),
                            Span::styled(format!("  {meaning}"), theme.fg(Shade::Text)),
                        ])
                    })
                    .collect()
            }
        }
    }

    /// The limits line: what bounds the call.
    fn limits(&self) -> String {
        match self {
            Modal::Permission { request, .. } => {
                let cap = format!("output cap {} KiB", DEFAULT_OUTPUT_CAP / 1024);
                if request.tool == "shell" {
                    let timeout = request
                        .arguments
                        .get("timeout_s")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(DEFAULT_TIMEOUT_S);
                    format!("timeout {timeout}s · {cap}")
                } else {
                    cap
                }
            }
            Modal::Memory { .. } => "kept facts load into every session in this repo".to_owned(),
            Modal::Help { .. } => "PageUp and PageDown scroll this list".to_owned(),
        }
    }

    /// The choices and the safe answer, laid out over `width`.
    fn choices_line(&self, width: usize, theme: &Theme) -> Line<'static> {
        let (choices, escape): (Vec<Choice>, &str) = match self {
            Modal::Permission { .. } => (
                vec![
                    Choice {
                        key: "y",
                        label: "allow",
                        shade: Shade::Success,
                    },
                    Choice {
                        key: "n",
                        label: "deny",
                        shade: Shade::Danger,
                    },
                    Choice {
                        key: "a",
                        label: "always",
                        shade: Shade::Success,
                    },
                ],
                "esc = deny",
            ),
            Modal::Memory { .. } => (
                vec![
                    Choice {
                        key: "k",
                        label: "keep",
                        shade: Shade::Success,
                    },
                    Choice {
                        key: "d",
                        label: "drop",
                        shade: Shade::Danger,
                    },
                ],
                "esc = drop",
            ),
            Modal::Help { .. } => (Vec::new(), "any key closes"),
        };
        let mut spans = Vec::new();
        let mut used = 0;
        for (index, choice) in choices.iter().enumerate() {
            if index > 0 {
                spans.push(Span::raw("   "));
                used += 3;
            }
            spans.push(Span::styled(choice.key, theme.bold(choice.shade)));
            spans.push(Span::styled(
                format!(" {}", choice.label),
                theme.fg(Shade::Text),
            ));
            used += choice.key.width() + 1 + choice.label.width();
        }
        let gap = width.saturating_sub(used + escape.width()).max(1);
        spans.push(Span::raw(" ".repeat(gap)));
        spans.push(Span::styled(escape, theme.fg(Shade::Faint)));
        Line::from(spans)
    }

    /// Scrolls on the arrow and page keys.
    fn scroll_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Up => self.scroll_by(-1),
            KeyCode::Down => self.scroll_by(1),
            KeyCode::PageUp => self.scroll_by(-10),
            KeyCode::PageDown => self.scroll_by(10),
            _ => {}
        }
    }

    /// Moves the first shown payload line; the draw clamps it.
    fn scroll_by(&mut self, delta: isize) {
        if let Some(scroll) = self.scroll_mut() {
            *scroll = scroll.saturating_add_signed(delta);
        }
    }

    /// Keeps the scroll within the payload and returns it.
    fn clamp_scroll(&mut self, max: usize) -> usize {
        match self.scroll_mut() {
            Some(scroll) => {
                *scroll = (*scroll).min(max);
                *scroll
            }
            None => 0,
        }
    }

    /// The scroll position of the prompts that have one.
    fn scroll_mut(&mut self) -> Option<&mut usize> {
        match self {
            Modal::Permission { scroll, .. } | Modal::Help { scroll, .. } => Some(scroll),
            Modal::Memory { .. } => None,
        }
    }
}

/// The path or command of a request, or its arguments when it has neither.
fn target_text(request: &PermissionRequest) -> String {
    request
        .target
        .clone()
        .unwrap_or_else(|| request.arguments.to_string())
}

/// A rectangle of the given size in the middle of `area`.
fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn permission() -> (Modal, oneshot::Receiver<PromptAnswer>) {
        let (tx, rx) = oneshot::channel();
        let modal = Modal::permission(
            PermissionRequest {
                tool: "shell".to_owned(),
                target: Some("cargo test".to_owned()),
                arguments: serde_json::json!({"command": "cargo test", "timeout_s": 300}),
            },
            None,
            tx,
        );
        (modal, rx)
    }

    fn press(modal: &mut Modal, code: KeyCode) -> Option<Answer> {
        modal.key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn permission_keys_reply_and_limits() {
        let (mut modal, mut rx) = permission();
        assert_eq!(press(&mut modal, KeyCode::Char('x')), None);
        assert_eq!(
            press(&mut modal, KeyCode::Char('a')),
            Some(Answer::Permission(PromptAnswer::Always))
        );
        assert_eq!(modal.question().as_deref(), Some("allow shell cargo test?"));
        assert_eq!(modal.context(), "shell wants to run");
        assert_eq!(modal.limits(), "timeout 300s · output cap 30 KiB");
        modal.reply(Answer::Permission(PromptAnswer::Always));
        assert_eq!(rx.try_recv(), Ok(PromptAnswer::Always));
    }

    #[test]
    fn a_mismatched_answer_drops_the_reply() {
        let (modal, mut rx) = permission();
        modal.reply(Answer::Help);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn memory_and_help_keys() {
        let (tx, mut rx) = oneshot::channel();
        let mut modal = Modal::memory("tooling".to_owned(), "uses just".to_owned(), tx);
        assert_eq!(press(&mut modal, KeyCode::Char('q')), None);
        assert_eq!(
            press(&mut modal, KeyCode::Esc),
            Some(Answer::Memory(MemoryAnswer::Drop))
        );
        modal.reply(Answer::Memory(MemoryAnswer::Keep));
        assert_eq!(rx.try_recv(), Ok(MemoryAnswer::Keep));
        let mut help = Modal::help(true);
        assert_eq!(press(&mut help, KeyCode::Down), None);
        assert_eq!(press(&mut help, KeyCode::Char('x')), Some(Answer::Help));
        assert_eq!(help.question(), None);
        assert_eq!(help.safe_answer(), Answer::Help);
    }

    #[test]
    fn choices_put_the_safe_answer_at_the_right_edge() {
        let (modal, _rx) = permission();
        let theme = Theme::truecolor();
        let line = modal.choices_line(50, &theme);
        let text: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(text.width(), 50);
        assert!(text.starts_with("y allow   n deny   a always"), "{text:?}");
        assert!(text.ends_with("esc = deny"), "{text:?}");
        assert_eq!(line.spans[0].style.fg, Some(theme.success));
        assert_eq!(line.spans[3].style.fg, Some(theme.danger));
    }

    #[test]
    fn centered_fits_inside_the_area() {
        let area = Rect::new(0, 0, 100, 30);
        assert_eq!(centered(area, 60, 10), Rect::new(20, 10, 60, 10));
        assert_eq!(centered(area, 200, 50), area);
    }
}
