//! Prompts drawn over the transcript: tool permission, plan approval,
//! memory keep/drop and the key help. While one is open every key goes to
//! it and nothing else on the screen changes.

use anvil_core::agent::PlanDecision;
use anvil_core::tools::{PermissionRequest, PromptAnswer};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};
use tokio::sync::oneshot;
use unicode_width::UnicodeWidthStr;

use super::editor::{EditKey, Editor};
use super::keys;
use super::markdown::MarkdownDoc;
use super::theme::{Shade, Theme};
use super::wrap::{segments, wrap};
use super::{MemoryAnswer, UiRequest};

/// The widest a prompt gets, borders included.
const MAX_WIDTH: u16 = 90;

/// Rows the revise editor may take inside the plan prompt.
const REVISE_ROWS: u16 = 3;

/// The prompt the user asked to revise the plan with.
const REVISE_PREFIX: &str = "what should change? ";

/// A prompt waiting for the user.
#[derive(Debug)]
pub enum Modal {
    /// May this tool call run?
    Permission {
        /// What is about to run.
        request: PermissionRequest,
        /// Where the answer goes.
        reply: oneshot::Sender<PromptAnswer>,
        /// First body line shown.
        scroll: usize,
    },
    /// Should this plan be built?
    Plan {
        /// The plan, rendered as markdown; boxed because it is large.
        doc: Box<MarkdownDoc>,
        /// The feedback editor, open after `r`.
        revise: Option<Editor>,
        /// Where the decision goes.
        reply: oneshot::Sender<PlanDecision>,
        /// First body line shown.
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
        /// First body line shown.
        scroll: usize,
    },
}

/// What a prompt was answered with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// A permission answer.
    Permission(PromptAnswer),
    /// A plan decision.
    Plan(PlanDecision),
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
            Answer::Permission(PromptAnswer::Yes) => "yes".to_owned(),
            Answer::Permission(PromptAnswer::No) => "no".to_owned(),
            Answer::Permission(PromptAnswer::Always) => "always".to_owned(),
            Answer::Plan(PlanDecision::Approve) => "yes".to_owned(),
            Answer::Plan(PlanDecision::Revise(feedback)) => format!("revise: {feedback}"),
            Answer::Plan(PlanDecision::Abort) => "quit".to_owned(),
            Answer::Memory(MemoryAnswer::Keep) => "keep".to_owned(),
            Answer::Memory(MemoryAnswer::Drop) => "drop".to_owned(),
            Answer::Help => String::new(),
        }
    }
}

impl Modal {
    /// The prompt for a request.
    ///
    /// # Arguments
    ///
    /// * `request` - The question and its reply channel.
    ///
    /// # Returns
    ///
    /// The prompt.
    pub fn from_request(request: UiRequest) -> Self {
        match request {
            UiRequest::Permission { request, reply } => Modal::Permission {
                request,
                reply,
                scroll: 0,
            },
            UiRequest::Plan { plan, reply } => {
                let mut doc = MarkdownDoc::new();
                doc.push(&plan);
                doc.finish();
                Modal::Plan {
                    doc: Box::new(doc),
                    revise: None,
                    reply,
                    scroll: 0,
                }
            }
            UiRequest::Memory { topic, fact, reply } => Modal::Memory { topic, fact, reply },
        }
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
            Modal::Plan { .. } => Some("build this plan?".to_owned()),
            Modal::Memory { topic, fact, .. } => Some(format!("keep [{topic}] {fact}?")),
            Modal::Help { .. } => None,
        }
    }

    /// The answer that does nothing irreversible: no, quit, drop.
    ///
    /// # Returns
    ///
    /// The answer.
    pub fn safe_answer(&self) -> Answer {
        match self {
            Modal::Permission { .. } => Answer::Permission(PromptAnswer::No),
            Modal::Plan { .. } => Answer::Plan(PlanDecision::Abort),
            Modal::Memory { .. } => Answer::Memory(MemoryAnswer::Drop),
            Modal::Help { .. } => Answer::Help,
        }
    }

    /// Whether the plan prompt is taking feedback, for tests.
    #[cfg(test)]
    pub fn is_editing(&self) -> bool {
        matches!(
            self,
            Modal::Plan {
                revise: Some(_),
                ..
            }
        )
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
                _ => {
                    self.scroll_key(key.code);
                    None
                }
            },
            Modal::Plan {
                revise: Some(editor),
                ..
            } => match key.code {
                KeyCode::Enter if !editor.is_empty() => {
                    let feedback = editor.take().trim().to_owned();
                    Some(Answer::Plan(PlanDecision::Revise(feedback)))
                }
                KeyCode::Esc => {
                    if let Modal::Plan { revise, .. } = self {
                        *revise = None;
                    }
                    None
                }
                _ => {
                    if let Some(edit) = edit_key(key) {
                        editor.key(edit);
                    }
                    None
                }
            },
            Modal::Plan { revise, .. } => match key.code {
                KeyCode::Char('y' | 'Y') if plain => Some(Answer::Plan(PlanDecision::Approve)),
                KeyCode::Char('q' | 'Q' | 'n' | 'N') if plain => {
                    Some(Answer::Plan(PlanDecision::Abort))
                }
                KeyCode::Char('r' | 'R') | KeyCode::Esc if plain => {
                    *revise = Some(Editor::new());
                    None
                }
                _ => {
                    self.scroll_key(key.code);
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
                KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown => {
                    self.scroll_key(key.code);
                    None
                }
                _ => Some(Answer::Help),
            },
        }
    }

    /// Scrolls the body by a mouse wheel notch.
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
            (Modal::Plan { reply, .. }, Answer::Plan(decision)) => {
                let _ = reply.send(decision);
            }
            (Modal::Memory { reply, .. }, Answer::Memory(answer)) => {
                let _ = reply.send(answer);
            }
            _ => {}
        }
    }

    /// Draws the prompt centered over `area`.
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame.
    /// * `area` - The whole screen.
    /// * `theme` - Styles.
    pub fn draw(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let width = area.width.saturating_sub(4).clamp(20, MAX_WIDTH);
        let inner_width = usize::from(width.saturating_sub(4)).max(1);
        let body = self.body(inner_width, theme);
        let editor_rows = match self {
            Modal::Plan {
                revise: Some(editor),
                ..
            } => editor
                .height(width.saturating_sub(4), REVISE_PREFIX.width())
                .min(REVISE_ROWS),
            _ => 0,
        };
        let fixed = 2 + editor_rows + 2;
        let max_body = usize::from(area.height.saturating_sub(fixed).max(1));
        let shown = body.len().clamp(1, max_body);
        let scroll = self.clamp_scroll(body.len().saturating_sub(shown));
        let height = u16::try_from(shown)
            .unwrap_or(u16::MAX)
            .saturating_add(fixed);
        let rect = centered(area, width, height.min(area.height));
        frame.render_widget(Clear, rect);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(theme.fg(Shade::Accent))
            .title(Span::styled(
                format!(" {} ", self.title()),
                theme.bold(Shade::Accent),
            ));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        let padded = Rect {
            x: inner.x.saturating_add(1),
            width: inner.width.saturating_sub(2),
            ..inner
        };
        let [body_area, editor_area, _, footer_area] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(editor_rows),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(padded);
        let visible: Vec<Line<'static>> = body.into_iter().skip(scroll).take(shown).collect();
        frame.render_widget(Paragraph::new(visible), body_area);
        if let Modal::Plan {
            revise: Some(editor),
            ..
        } = self
        {
            let (lines, (column, row)) =
                editor.render(editor_area.width, REVISE_PREFIX, theme, Shade::Accent);
            frame.render_widget(Paragraph::new(lines), editor_area);
            let x = editor_area
                .x
                .saturating_add(column)
                .min(editor_area.right().saturating_sub(1));
            let y = editor_area
                .y
                .saturating_add(row)
                .min(editor_area.bottom().saturating_sub(1));
            frame.set_cursor_position((x, y));
        }
        frame.render_widget(
            Paragraph::new(Line::styled(self.footer(), theme.fg(Shade::Muted))),
            footer_area,
        );
    }

    /// The title in the border.
    fn title(&self) -> &'static str {
        match self {
            Modal::Permission { .. } => "permission",
            Modal::Plan { .. } => "plan",
            Modal::Memory { .. } => "remember?",
            Modal::Help { .. } => "keys",
        }
    }

    /// The key hints under the body.
    fn footer(&self) -> &'static str {
        match self {
            Modal::Permission { .. } => "[y]es  [n]o  [a]lways    Esc = no",
            Modal::Plan {
                revise: Some(_), ..
            } => "Enter = send the feedback    Esc = back",
            Modal::Plan { .. } => "[y]es, build it  [r]evise  [q]uit    Esc = revise",
            Modal::Memory { .. } => "[k]eep  [d]rop    Esc = drop",
            Modal::Help { .. } => "any key closes",
        }
    }

    /// The body lines at a width. The plan's markdown caches its lines, so
    /// this takes the prompt mutably.
    fn body(&mut self, width: usize, theme: &Theme) -> Vec<Line<'static>> {
        match self {
            Modal::Permission { request, .. } => {
                let mut lines = vec![Line::from(vec![
                    Span::styled(request.tool.clone(), theme.bold(Shade::Text)),
                    Span::raw(" "),
                    Span::raw(target_text(request)),
                ])];
                let args = serde_json::to_string_pretty(&request.arguments)
                    .unwrap_or_else(|_| request.arguments.to_string());
                if request.arguments.is_object() || request.arguments.is_array() {
                    lines.push(Line::default());
                    for line in args.lines() {
                        for (_, segment) in segments(line, width) {
                            lines.push(Line::styled(segment, theme.fg(Shade::Muted)));
                        }
                    }
                }
                lines
            }
            Modal::Plan { doc, .. } => doc.lines(u16::try_from(width).unwrap_or(u16::MAX), theme),
            Modal::Memory { topic, fact, .. } => {
                let mut lines = vec![Line::styled(format!("[{topic}]"), theme.fg(Shade::Muted))];
                lines.extend(wrap(fact, width).into_iter().map(Line::from));
                lines
            }
            Modal::Help { run, .. } => {
                let key_width = keys::HELP
                    .iter()
                    .map(|(key, _)| key.width())
                    .max()
                    .unwrap_or(0);
                let mut lines: Vec<Line<'static>> = keys::HELP
                    .iter()
                    .map(|(key, meaning)| {
                        Line::from(vec![
                            Span::styled(format!("{key:<key_width$}"), theme.bold(Shade::Text)),
                            Span::raw(format!("  {meaning}")),
                        ])
                    })
                    .collect();
                if *run {
                    lines.push(Line::default());
                    lines.push(Line::from(
                        "during a run the input takes /expand, /help and /quit",
                    ));
                }
                lines
            }
        }
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

    /// Moves the first shown line; the draw clamps it to the body.
    fn scroll_by(&mut self, delta: isize) {
        if let Some(scroll) = self.scroll_mut() {
            *scroll = scroll.saturating_add_signed(delta);
        }
    }

    /// Keeps the scroll within the body and returns it.
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
            Modal::Permission { scroll, .. }
            | Modal::Plan { scroll, .. }
            | Modal::Help { scroll, .. } => Some(scroll),
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

/// The editing key for the revise editor, if the key is one.
fn edit_key(key: KeyEvent) -> Option<EditKey> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        KeyCode::Char(ch) if !ctrl && !alt => Some(EditKey::Char(ch)),
        KeyCode::Backspace => Some(EditKey::Backspace),
        KeyCode::Delete => Some(EditKey::Delete),
        KeyCode::Left => Some(EditKey::Left),
        KeyCode::Right => Some(EditKey::Right),
        KeyCode::Home => Some(EditKey::Home),
        KeyCode::End => Some(EditKey::End),
        _ => None,
    }
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
        let modal = Modal::from_request(UiRequest::Permission {
            request: PermissionRequest {
                tool: "shell".to_owned(),
                target: Some("cargo test".to_owned()),
                arguments: serde_json::json!({"command": "cargo test"}),
            },
            reply: tx,
        });
        (modal, rx)
    }

    fn plan() -> (Modal, oneshot::Receiver<PlanDecision>) {
        let (tx, rx) = oneshot::channel();
        let modal = Modal::from_request(UiRequest::Plan {
            plan: "# Plan\n\n1. [ ] a\n".to_owned(),
            reply: tx,
        });
        (modal, rx)
    }

    fn press(modal: &mut Modal, code: KeyCode) -> Option<Answer> {
        modal.key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn permission_keys_and_reply() {
        let (mut modal, mut rx) = permission();
        assert_eq!(press(&mut modal, KeyCode::Char('x')), None);
        assert_eq!(
            press(&mut modal, KeyCode::Char('a')),
            Some(Answer::Permission(PromptAnswer::Always))
        );
        assert_eq!(modal.question().as_deref(), Some("allow shell cargo test?"));
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
    fn plan_revise_takes_feedback_and_esc_goes_back() {
        let (mut modal, mut rx) = plan();
        assert_eq!(press(&mut modal, KeyCode::Esc), None);
        assert!(modal.is_editing());
        assert_eq!(
            press(&mut modal, KeyCode::Enter),
            None,
            "empty feedback stays"
        );
        assert_eq!(press(&mut modal, KeyCode::Esc), None);
        assert!(!modal.is_editing());
        assert_eq!(press(&mut modal, KeyCode::Char('r')), None);
        for ch in "add tests".chars() {
            press(&mut modal, KeyCode::Char(ch));
        }
        let answer = press(&mut modal, KeyCode::Enter);
        assert_eq!(
            answer,
            Some(Answer::Plan(PlanDecision::Revise("add tests".to_owned())))
        );
        assert_eq!(modal.safe_answer(), Answer::Plan(PlanDecision::Abort));
        modal.reply(answer.expect("answered"));
        assert_eq!(
            rx.try_recv(),
            Ok(PlanDecision::Revise("add tests".to_owned()))
        );
    }

    #[test]
    fn memory_and_help_keys() {
        let (tx, mut rx) = oneshot::channel();
        let mut modal = Modal::from_request(UiRequest::Memory {
            topic: "tooling".to_owned(),
            fact: "uses just".to_owned(),
            reply: tx,
        });
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
    }

    #[test]
    fn centered_fits_inside_the_area() {
        let area = Rect::new(0, 0, 100, 30);
        assert_eq!(centered(area, 60, 10), Rect::new(20, 10, 60, 10));
        assert_eq!(centered(area, 200, 50), area);
    }
}
