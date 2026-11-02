//! The UI state: consumes messages, keeps the three regions, draws them.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use anvil_core::agent::{LoopEvent, State};
use crossterm::event::{KeyEvent, MouseButton, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::widgets::{Block, BorderType, Paragraph};
use unicode_width::UnicodeWidthStr;

use super::editor::{EditKey, Editor};
use super::keys::{self, KeyAction};
use super::modal::{Answer, Modal};
use super::status::StatusBar;
use super::theme::{Shade, Theme};
use super::transcript::{Entry, Transcript};
use super::welcome::Welcome;
use super::{Level, PermissionMode, SessionInfo, SessionKind, UiMsg, UiRequest};

/// How soon a second Ctrl+C must follow the first to quit.
pub const QUIT_WINDOW: Duration = Duration::from_secs(2);

/// What the UI asks the runner to do after handling a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Hand this line to the session.
    Submit(String),
    /// The user cycled the permission mode.
    SetMode(PermissionMode),
    /// Stop the running turn.
    CancelTurn,
    /// Close the UI and end the session.
    Quit,
}

/// The whole UI state. Pure: time comes in as ticks and nothing here
/// touches the terminal, the clock or the environment.
///
/// While a prompt is open every key goes to it, further prompts queue,
/// and every other message waits in `deferred`, so the screen under the
/// prompt does not change until it is answered.
#[derive(Debug)]
pub struct App {
    info: SessionInfo,
    theme: Theme,
    transcript: Transcript,
    editor: Editor,
    status: StatusBar,
    modal: Option<Modal>,
    pending: VecDeque<UiRequest>,
    deferred: VecDeque<UiMsg>,
    state: State,
    busy: bool,
    now: Instant,
    tick: u64,
    last_ctrl_c: Option<Instant>,
    /// Nothing has happened yet: the startup screen is showing.
    welcome: bool,
    /// The session ended; the next key closes the UI.
    finished: bool,
}

impl App {
    /// A UI for a session.
    ///
    /// # Arguments
    ///
    /// * `info` - The session.
    /// * `theme` - Styles.
    /// * `now` - The current time.
    ///
    /// # Returns
    ///
    /// The UI, empty.
    pub fn new(info: SessionInfo, theme: Theme, now: Instant) -> Self {
        let mut transcript = Transcript::new(info.cwd.clone());
        transcript.push(Entry::Welcome(Welcome::new(&info, now)));
        Self {
            status: StatusBar::new(&info, now),
            state: info.initial_state,
            info,
            theme,
            transcript,
            editor: Editor::new(),
            modal: None,
            pending: VecDeque::new(),
            deferred: VecDeque::new(),
            busy: false,
            now,
            tick: 0,
            last_ctrl_c: None,
            welcome: true,
            finished: false,
        }
    }

    /// Handles one message.
    ///
    /// # Arguments
    ///
    /// * `msg` - The message.
    ///
    /// # Returns
    ///
    /// What the runner should do, if anything.
    pub fn handle(&mut self, msg: UiMsg) -> Option<Action> {
        if self.modal.is_some() {
            return self.handle_under_modal(msg);
        }
        match msg {
            UiMsg::Loop(event) => {
                self.welcome = false;
                self.apply_loop(&event);
                None
            }
            UiMsg::Request(request) => {
                self.open(Modal::from_request(request));
                None
            }
            UiMsg::ToolResult {
                call_id,
                text,
                is_error,
            } => {
                self.transcript
                    .set_tool_result(&call_id, text, is_error, self.now);
                None
            }
            UiMsg::Plan { path, text } => {
                self.transcript.set_plan(path, &text);
                None
            }
            UiMsg::Mode(state) => {
                self.set_state(state);
                None
            }
            UiMsg::PermissionMode(mode) => {
                self.status.set_mode(mode);
                None
            }
            UiMsg::Model(model) => {
                self.status.set_model(&model);
                None
            }
            UiMsg::Busy(busy) => {
                self.busy = busy;
                if !busy {
                    self.transcript.cancel_running(self.now);
                }
                None
            }
            UiMsg::Notice { text, level } => {
                self.transcript.notice(text, level);
                None
            }
            UiMsg::Shutdown { wait_for_key } => {
                self.finished = true;
                if wait_for_key {
                    self.transcript
                        .notice("session over; press any key to close", Level::Warn);
                    None
                } else {
                    Some(Action::Quit)
                }
            }
            UiMsg::Tick(now) => {
                self.now = now;
                self.tick = self.tick.wrapping_add(1);
                None
            }
            UiMsg::Key(key) => self.key(key),
            UiMsg::Mouse(mouse) => {
                match mouse.kind {
                    MouseEventKind::ScrollUp => self.transcript.wheel(true),
                    MouseEventKind::ScrollDown => self.transcript.wheel(false),
                    MouseEventKind::Down(MouseButton::Left) => self.transcript.click(mouse.row),
                    _ => {}
                }
                None
            }
            UiMsg::Resize => None,
        }
    }

    /// Draws the transcript, the status line, the input and the open
    /// prompt, if any, over them.
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame.
    pub fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let prefix = self.prompt_prefix();
        let input_height = self
            .editor
            .height(area.width.saturating_sub(2), prefix.width())
            .saturating_add(2);
        let [transcript_area, status_area, input_box] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(input_height),
        ])
        .areas(area);
        let dimmed = self.modal.is_some();
        let theme = if dimmed {
            self.theme.dimmed()
        } else {
            self.theme
        };
        self.transcript
            .draw(frame, transcript_area, &theme, self.tick, dimmed, self.now);
        frame.render_widget(
            self.status.line(status_area.width, self.now, &theme),
            status_area,
        );
        let border = if self.welcome && !dimmed {
            theme.fg(Shade::Accent)
        } else {
            theme.border()
        };
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(border);
        let input_area = block.inner(input_box);
        frame.render_widget(block, input_box);
        let prefix_shade = if dimmed { Shade::Faint } else { Shade::Accent };
        let (lines, (column, row)) =
            self.editor
                .render(input_area.width, &prefix, &theme, prefix_shade);
        frame.render_widget(Paragraph::new(lines), input_area);
        if let Some(modal) = &mut self.modal {
            modal.draw(frame, area, &self.theme);
            return;
        }
        let x = input_area
            .x
            .saturating_add(column)
            .min(input_area.right().saturating_sub(1));
        let y = input_area
            .y
            .saturating_add(row)
            .min(input_area.bottom().saturating_sub(1));
        frame.set_cursor_position((x, y));
    }

    /// The transcript, for tests.
    #[cfg(test)]
    pub fn transcript(&self) -> &Transcript {
        &self.transcript
    }

    /// The input editor, for tests.
    #[cfg(test)]
    pub fn editor(&self) -> &Editor {
        &self.editor
    }

    /// Whether the session has ended, for tests.
    #[cfg(test)]
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Whether a prompt is open, for tests.
    #[cfg(test)]
    pub fn has_modal(&self) -> bool {
        self.modal.is_some()
    }

    /// Handles a message while a prompt is open: keys and the wheel go to
    /// the prompt, new prompts queue, everything else waits.
    fn handle_under_modal(&mut self, msg: UiMsg) -> Option<Action> {
        match msg {
            UiMsg::Key(key) => {
                if keys::map(key, true) == KeyAction::Cancel {
                    return self.ctrl_c();
                }
                let answer = self.modal.as_mut().and_then(|modal| modal.key(key))?;
                let modal = self.modal.take()?;
                self.close_modal(modal, answer)
            }
            UiMsg::Mouse(mouse) => {
                if let Some(modal) = &mut self.modal {
                    match mouse.kind {
                        MouseEventKind::ScrollUp => modal.wheel(true),
                        MouseEventKind::ScrollDown => modal.wheel(false),
                        _ => {}
                    }
                }
                None
            }
            UiMsg::Request(request) => {
                self.pending.push_back(request);
                None
            }
            UiMsg::Tick(_) => None,
            other => {
                self.deferred.push_back(other);
                None
            }
        }
    }

    /// Opens a prompt. The quit window resets so the first Ctrl+C in a
    /// prompt only closes it.
    fn open(&mut self, modal: Modal) {
        self.last_ctrl_c = None;
        self.modal = Some(modal);
    }

    /// Answers a prompt: records it, replies, replays what waited and
    /// opens the next queued prompt.
    fn close_modal(&mut self, modal: Modal, answer: Answer) -> Option<Action> {
        if let Some(question) = modal.question() {
            self.transcript.push(Entry::Answer {
                question,
                answer: answer.label(),
            });
        }
        modal.reply(answer);
        let mut action = None;
        while let Some(msg) = self.deferred.pop_front() {
            if let Some(next) = self.handle(msg) {
                action = Some(next);
            }
        }
        if let Some(request) = self.pending.pop_front() {
            self.open(Modal::from_request(request));
        }
        action
    }

    /// Applies a loop event to the transcript and the status line.
    fn apply_loop(&mut self, event: &LoopEvent) {
        match event {
            LoopEvent::StateEntered { state, .. } => self.set_state(*state),
            LoopEvent::TurnFinished {
                usage: Some(usage), ..
            }
            | LoopEvent::Compacted {
                usage: Some(usage), ..
            } => self.status.record(*usage),
            _ => {}
        }
        self.transcript.apply(event, self.now);
    }

    /// Shows a new state in the prompt and the status line.
    fn set_state(&mut self, state: State) {
        self.state = state;
        self.status.set_state(state);
    }

    /// Handles a key.
    fn key(&mut self, key: KeyEvent) -> Option<Action> {
        if self.finished {
            return Some(Action::Quit);
        }
        match keys::map(key, self.editor.is_empty()) {
            KeyAction::Submit => self.submit(),
            KeyAction::Newline => self.edit(EditKey::Newline),
            KeyAction::Cancel => self.ctrl_c(),
            KeyAction::ClearView => {
                self.transcript.clear_view();
                None
            }
            KeyAction::PageUp => {
                self.transcript.page_up();
                None
            }
            KeyAction::PageDown => {
                self.transcript.page_down();
                None
            }
            KeyAction::Up => {
                if self.editor.at_first_line() {
                    self.editor.history_prev();
                } else {
                    self.editor.key(EditKey::Up);
                }
                None
            }
            KeyAction::Down => {
                if self.editor.at_last_line() {
                    self.editor.history_next();
                } else {
                    self.editor.key(EditKey::Down);
                }
                None
            }
            KeyAction::SelectPrev => {
                self.transcript.select_prev();
                None
            }
            KeyAction::SelectNext => {
                self.transcript.select_next();
                None
            }
            KeyAction::Complete => {
                self.editor.complete(&self.info.slash_commands);
                None
            }
            KeyAction::CycleMode => {
                let mode = self.status.mode().next();
                self.status.set_mode(mode);
                Some(Action::SetMode(mode))
            }
            KeyAction::Escape => {
                self.transcript.clear_selection();
                None
            }
            KeyAction::Ignore => None,
            KeyAction::Help => {
                self.show_help();
                None
            }
            KeyAction::Edit(key) => self.edit(key),
        }
    }

    /// Edits the input; typing jumps the transcript back to the bottom.
    fn edit(&mut self, key: EditKey) -> Option<Action> {
        self.editor.key(key);
        self.transcript.jump_bottom();
        None
    }

    /// Sends the input, or handles it when it is a UI-only command. An
    /// empty input toggles the selected tool call instead.
    fn submit(&mut self) -> Option<Action> {
        if self.editor.is_empty() {
            self.transcript.toggle_selected();
            return None;
        }
        let text = self.editor.take().trim().to_owned();
        self.editor.push_history(&text);
        self.welcome = false;
        self.transcript.jump_bottom();
        self.transcript.push(Entry::User { text: text.clone() });
        let Some(command) = text.strip_prefix('/') else {
            if self.info.kind == SessionKind::Run {
                self.transcript.notice(
                    "a run takes only slash commands; /help lists them",
                    Level::Warn,
                );
                return None;
            }
            return Some(Action::Submit(text));
        };
        let word = command.split_whitespace().next().unwrap_or("");
        match word {
            "expand" => {
                self.transcript.expand_all();
                return None;
            }
            "plain" => {
                self.transcript
                    .notice("/plain is not available yet", Level::Warn);
                return None;
            }
            _ => {}
        }
        if self.info.kind == SessionKind::Run {
            match word {
                "help" => self.show_help(),
                "quit" | "exit" | "q" => return Some(Action::Quit),
                _ => self.transcript.notice(
                    format!("`/{word}` is not available during a run"),
                    Level::Warn,
                ),
            }
            return None;
        }
        Some(Action::Submit(text))
    }

    /// Ctrl+C: quit on a quick second press. Otherwise close any prompt
    /// with its safe answer, which unblocks the session, and cancel the
    /// turn if one is running.
    fn ctrl_c(&mut self) -> Option<Action> {
        if let Some(last) = self.last_ctrl_c
            && self.now.saturating_duration_since(last) <= QUIT_WINDOW
        {
            return Some(Action::Quit);
        }
        self.last_ctrl_c = Some(self.now);
        let mut action = None;
        if let Some(modal) = self.modal.take() {
            let answer = modal.safe_answer();
            action = self.close_modal(modal, answer);
        }
        if self.is_busy() {
            self.transcript.notice(
                "cancelling; press Ctrl+C again within 2 s to quit",
                Level::Warn,
            );
            return action.or(Some(Action::CancelTurn));
        }
        self.transcript
            .notice("press Ctrl+C again within 2 s to quit", Level::Warn);
        action
    }

    /// Whether a turn is running. A run is busy until it ends.
    fn is_busy(&self) -> bool {
        self.busy || (self.info.kind == SessionKind::Run && !self.finished)
    }

    /// Opens the key help.
    fn show_help(&mut self) {
        self.open(Modal::help(self.info.kind == SessionKind::Run));
    }

    /// The input prompt: the state and a chevron.
    fn prompt_prefix(&self) -> String {
        format!("{} > ", self.state.name())
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use anvil_core::agent::{
        AgentLoop, AutoApprove, CheckCommand, EventSink, LoopConfig, LoopProviders, PlanDecision,
    };
    use anvil_core::provider::{Event, FakeProvider, Provider, StopReason, ToolCall, Usage};
    use anvil_core::tools::{
        OutputStats, PermissionRequest, Permissions, PromptAnswer, StaticPrompt, ToolContext,
        ToolOutcome, ToolRegistry,
    };
    use anvil_types::LimitsConfig;
    use crossterm::event::{KeyCode, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use tokio::sync::oneshot;

    use super::*;
    use crate::tui::runner::plan_message;
    use crate::tui::welcome::LOGO;
    use crate::tui::{MemoryAnswer, RecentSession};

    const PLAN_LINE: &str = "1. [ ] Write notes.txt";

    fn info() -> SessionInfo {
        SessionInfo {
            kind: SessionKind::Chat,
            cwd: PathBuf::new(),
            model: "main (test-model)".to_owned(),
            context_window: 100_000,
            price: None,
            permission_mode: PermissionMode::Ask,
            initial_state: State::Build,
            slash_commands: vec!["/plan".to_owned(), "/plain".to_owned()],
            version: "0.1.0".to_owned(),
            coder: "main (test-model)".to_owned(),
            reviewer: Some("reviewer (test-reviewer)".to_owned()),
            rules: vec![
                (
                    "docstrings".to_owned(),
                    "Every public item gets a doc comment.".to_owned(),
                ),
                (
                    "no-unwrap".to_owned(),
                    "No unwrap outside tests.".to_owned(),
                ),
            ],
            skills: 3,
            mcp_servers: 1,
            recent_sessions: vec![
                RecentSession {
                    id: "01K9ABCDEFGHJKMNPQRSTVWXYZ".to_owned(),
                    status: "done".to_owned(),
                    ok: Some(true),
                    task: "add a --dry-run flag".to_owned(),
                    when: "2026-11-01 09:30".to_owned(),
                },
                RecentSession {
                    id: "01K9ABCDEFGHJKMNPQRSTVWXY0".to_owned(),
                    status: "incomplete".to_owned(),
                    ok: Some(false),
                    task: "rename the config module".to_owned(),
                    when: "2026-10-31 18:02".to_owned(),
                },
            ],
            animate: false,
        }
    }

    fn new_app() -> (App, Instant) {
        let base = Instant::now();
        (App::new(info(), Theme::mono(), base), base)
    }

    fn key(app: &mut App, code: KeyCode) -> Option<Action> {
        app.handle(UiMsg::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    fn ctrl_c(app: &mut App) -> Option<Action> {
        app.handle(UiMsg::Key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        )))
    }

    fn type_line(app: &mut App, text: &str) {
        for ch in text.chars() {
            key(app, KeyCode::Char(ch));
        }
    }

    fn rows(terminal: &Terminal<TestBackend>) -> Vec<String> {
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| {
                        buffer
                            .cell((x, y))
                            .map(|cell| cell.symbol().to_owned())
                            .unwrap_or_default()
                    })
                    .collect::<String>()
            })
            .collect()
    }

    fn shell_call(id: &str, command: &str) -> ToolCall {
        ToolCall {
            id: id.to_owned(),
            name: "shell".to_owned(),
            arguments_json: format!("{{\"command\": \"{command}\"}}"),
        }
    }

    /// Opens a permission prompt for a shell command.
    fn ask_permission(app: &mut App, command: &str) -> oneshot::Receiver<PromptAnswer> {
        let (tx, rx) = oneshot::channel();
        app.handle(UiMsg::Request(UiRequest::Permission {
            request: PermissionRequest {
                tool: "shell".to_owned(),
                target: Some(command.to_owned()),
                arguments: serde_json::json!({ "command": command }),
            },
            reply: tx,
        }));
        rx
    }

    /// Runs the loop on scripted providers and returns every event it
    /// emitted, the way the renderer receives them.
    async fn scripted_run(cwd: &Path) -> Vec<LoopEvent> {
        let plan_script = vec![
            Ok(Event::TextDelta("# Plan\n\n1. [ ] Write".to_owned())),
            Ok(Event::TextDelta(" notes.txt\n2. [ ] Add".to_owned())),
            Ok(Event::TextDelta(" a `test`\n".to_owned())),
            Ok(Event::Usage(Usage {
                input: 10,
                output: 5,
                cached: 0,
            })),
            Ok(Event::Done {
                stop_reason: StopReason::EndTurn,
            }),
        ];
        let write = ToolCall {
            id: "c1".to_owned(),
            name: "write_file".to_owned(),
            arguments_json: "{\"path\": \"notes.txt\", \"content\": \"hello\\n\"}".to_owned(),
        };
        let coder = Arc::new(FakeProvider::new(vec![
            plan_script,
            FakeProvider::tool_turn(vec![write]),
            FakeProvider::text_turn("Built it.\n"),
            FakeProvider::text_turn("Addressed the review.\n"),
            FakeProvider::text_turn("Nothing to simplify.\n"),
            FakeProvider::text_turn("Verified.\n"),
            FakeProvider::text_turn("Done.\n"),
        ]));
        let reviewer = Arc::new(FakeProvider::new(vec![
            FakeProvider::text_turn(
                "```json\n{\"verdict\": \"changes\", \"items\": [\"add a test\"]}\n```",
            ),
            FakeProvider::text_turn("```json\n{\"verdict\": \"approve\"}\n```"),
            FakeProvider::text_turn("```json\n{\"verdict\": \"approve\"}\n```"),
        ]));
        let session_dir = cwd.join(".anvil").join("sessions").join("01TESTSESSION");
        let config = LoopConfig {
            task: "write a note".to_owned(),
            cwd: cwd.to_path_buf(),
            session_dir,
            limits: LimitsConfig::default(),
            check: CheckCommand::Command("exit 0".to_owned()),
            context_window: 100_000,
            home_dir: None,
            extra_instructions: Vec::new(),
            sections: Vec::new(),
            review_sections: Vec::new(),
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let agent = AgentLoop::new(
            config,
            LoopProviders {
                coder: coder as Arc<dyn Provider>,
                reviewer: reviewer as Arc<dyn Provider>,
                summarizer: Arc::new(FakeProvider::new(vec![])) as Arc<dyn Provider>,
            },
            ToolRegistry::with_builtins(
                Permissions::empty(),
                Box::new(StaticPrompt(PromptAnswer::Yes)),
            ),
            ToolContext::new(cwd),
            Box::new(AutoApprove),
            EventSink::new(tx),
        );
        let _ = agent.run().await;
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        events
    }

    /// Feeds events into the UI one by one, drawing after each so the
    /// streaming path is exercised, and reads the plan like the runner.
    fn replay(app: &mut App, terminal: &mut Terminal<TestBackend>, events: &[LoopEvent]) {
        for event in events {
            if let LoopEvent::PlanWritten { path } = event
                && let Some(msg) = plan_message(path)
            {
                app.handle(msg);
            }
            app.handle(UiMsg::Loop(event.clone()));
            terminal.draw(|frame| app.draw(frame)).expect("draw");
        }
    }

    /// The session description for a scripted run in `cwd`.
    fn run_info(cwd: &Path) -> SessionInfo {
        let mut info = info();
        info.kind = SessionKind::Run;
        info.cwd = cwd.to_path_buf();
        info
    }

    #[test]
    fn startup_120() {
        let (mut app, _) = new_app();
        let mut terminal = Terminal::new(TestBackend::new(120, 38)).expect("terminal");
        terminal.draw(|frame| app.draw(frame)).expect("draw");
        insta::assert_snapshot!(terminal.backend());
    }

    #[test]
    fn startup_80() {
        let (mut app, _) = new_app();
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
        terminal.draw(|frame| app.draw(frame)).expect("draw");
        insta::assert_snapshot!(terminal.backend());
    }

    #[test]
    fn the_logo_draws_in_over_the_first_ticks() {
        let mut info = info();
        info.animate = true;
        let base = Instant::now();
        let mut app = App::new(info, Theme::mono(), base);
        let mut terminal = Terminal::new(TestBackend::new(120, 38)).expect("terminal");
        terminal.draw(|frame| app.draw(frame)).expect("draw");
        let first = rows(&terminal);
        let logo_rows = |rows: &[String]| {
            rows.iter()
                .filter(|row| LOGO.iter().any(|logo| row.starts_with(logo)))
                .count()
        };
        assert_eq!(logo_rows(&first), 1, "{}", first.join("\n"));
        app.handle(UiMsg::Tick(base + Duration::from_secs(1)));
        terminal.draw(|frame| app.draw(frame)).expect("draw");
        assert_eq!(logo_rows(&rows(&terminal)), LOGO.len());
    }

    #[test]
    fn the_input_border_is_accented_until_something_happens() {
        let (mut app, _) = new_app();
        let theme = Theme::truecolor();
        let mut app_color = App::new(info(), theme, Instant::now());
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
        terminal.draw(|frame| app_color.draw(frame)).expect("draw");
        let corner = |terminal: &Terminal<TestBackend>| {
            terminal
                .backend()
                .buffer()
                .cell((0, 21))
                .map(|cell| (cell.symbol().to_owned(), cell.fg))
                .expect("cell")
        };
        assert_eq!(corner(&terminal), ("╭".to_owned(), theme.accent));
        app_color.handle(UiMsg::Loop(LoopEvent::TextDelta("hi".to_owned())));
        terminal.draw(|frame| app_color.draw(frame)).expect("draw");
        assert_eq!(corner(&terminal), ("╭".to_owned(), theme.border));
        type_line(&mut app, "x");
        key(&mut app, KeyCode::Enter);
        assert!(!app.welcome);
    }

    #[test]
    fn empty_session() {
        let (mut app, _) = new_app();
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).expect("terminal");
        terminal.draw(|frame| app.draw(frame)).expect("draw");
        insta::assert_snapshot!(terminal.backend());
    }

    #[tokio::test]
    async fn streamed_plan() {
        let cwd = tempfile::tempdir().expect("cwd");
        let events = scripted_run(cwd.path()).await;
        let mut app = App::new(run_info(cwd.path()), Theme::mono(), Instant::now());
        let mut terminal = Terminal::new(TestBackend::new(100, 56)).expect("terminal");
        replay(&mut app, &mut terminal, &events);
        insta::assert_snapshot!(terminal.backend());
    }

    #[tokio::test]
    async fn streamed_plan_80_columns() {
        let cwd = tempfile::tempdir().expect("cwd");
        let events = scripted_run(cwd.path()).await;
        let mut app = App::new(run_info(cwd.path()), Theme::mono(), Instant::now());
        let mut terminal = Terminal::new(TestBackend::new(80, 60)).expect("terminal");
        replay(&mut app, &mut terminal, &events);
        insta::assert_snapshot!(terminal.backend());
    }

    #[tokio::test]
    async fn plan_lines_appear_exactly_once() {
        let cwd = tempfile::tempdir().expect("cwd");
        let events = scripted_run(cwd.path()).await;
        let mut app = App::new(run_info(cwd.path()), Theme::mono(), Instant::now());
        let mut terminal = Terminal::new(TestBackend::new(100, 120)).expect("terminal");
        replay(&mut app, &mut terminal, &events);
        let text = app.transcript().plain_text();
        assert_eq!(text.matches(PLAN_LINE).count(), 1, "{text}");
        let drawn = rows(&terminal);
        let hits = drawn.iter().filter(|row| row.contains(PLAN_LINE)).count();
        assert_eq!(hits, 1, "{}", drawn.join("\n"));
    }

    #[test]
    fn expanded_tool_call() {
        let (mut app, base) = new_app();
        let call = ToolCall {
            id: "c1".to_owned(),
            name: "shell".to_owned(),
            arguments_json: "{\"command\": \"cargo test --all\", \"timeout_s\": 120}".to_owned(),
        };
        app.handle(UiMsg::Loop(LoopEvent::ToolStarted { call }));
        app.handle(UiMsg::ToolResult {
            call_id: "c1".to_owned(),
            text: "running 3 tests\ntest a ... ok\ntest b ... ok\ntest c ... ok\n\ntest result: ok. 3 passed".to_owned(),
            is_error: false,
        });
        app.handle(UiMsg::Tick(base + Duration::from_millis(1_250)));
        app.handle(UiMsg::Loop(LoopEvent::ToolFinished {
            call_id: "c1".to_owned(),
            name: "shell".to_owned(),
            outcome: ToolOutcome::Completed,
            preview: "running 3 tests".to_owned(),
            stats: OutputStats {
                bytes: 80,
                truncated: false,
                digest: 0,
            },
        }));
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).expect("terminal");
        terminal.draw(|frame| app.draw(frame)).expect("draw");
        let drawn = rows(&terminal);
        let collapsed = drawn
            .iter()
            .find(|row| row.starts_with("ok     shell  cargo test --all"))
            .expect("the tool line");
        assert!(collapsed.trim_end().ends_with("1.2s"), "{collapsed:?}");
        key(&mut app, KeyCode::Enter);
        terminal.draw(|frame| app.draw(frame)).expect("draw");
        insta::assert_snapshot!(terminal.backend());
    }

    #[test]
    fn open_permission_modal() {
        let (mut app, _) = new_app();
        type_line(&mut app, "run the tests");
        key(&mut app, KeyCode::Enter);
        app.handle(UiMsg::Busy(true));
        app.handle(UiMsg::Loop(LoopEvent::TextDelta(
            "Running the suite now.\n".to_owned(),
        )));
        app.handle(UiMsg::Loop(LoopEvent::ToolStarted {
            call: shell_call("c1", "cargo test --all"),
        }));
        let _reply = ask_permission(&mut app, "cargo test --all");
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).expect("terminal");
        terminal.draw(|frame| app.draw(frame)).expect("draw");
        insta::assert_snapshot!(terminal.backend());
    }

    #[test]
    fn modal_is_never_overdrawn() {
        let (mut app, base) = new_app();
        app.handle(UiMsg::Busy(true));
        let mut reply = ask_permission(&mut app, "cargo test");
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).expect("terminal");
        terminal.draw(|frame| app.draw(frame)).expect("draw");
        let before = terminal.backend().buffer().clone();
        for n in 1..=20u64 {
            app.handle(UiMsg::Tick(base + Duration::from_millis(120 * n)));
        }
        app.handle(UiMsg::Loop(LoopEvent::ToolStarted {
            call: shell_call("c2", "ls"),
        }));
        app.handle(UiMsg::Loop(LoopEvent::TextDelta("more text".to_owned())));
        app.handle(UiMsg::Notice {
            text: "a notice".to_owned(),
            level: Level::Warn,
        });
        terminal.draw(|frame| app.draw(frame)).expect("draw");
        assert_eq!(terminal.backend().buffer(), &before);
        assert_eq!(app.transcript().plain_text(), "welcome");
        assert_eq!(key(&mut app, KeyCode::Char('y')), None);
        assert_eq!(reply.try_recv(), Ok(PromptAnswer::Yes));
        assert!(!app.has_modal());
        let text = app.transcript().plain_text();
        assert!(text.contains("? allow shell cargo test? yes"), "{text}");
        assert!(text.contains("shell ls"), "{text}");
        assert!(text.contains("more text"), "{text}");
        assert!(text.contains("a notice"), "{text}");
        terminal.draw(|frame| app.draw(frame)).expect("draw");
        assert_ne!(terminal.backend().buffer(), &before);
    }

    #[test]
    fn typed_answer_lands_in_modal_not_transcript() {
        let (mut app, _) = new_app();
        let mut reply = ask_permission(&mut app, "ls");
        type_line(&mut app, "helloy");
        assert!(app.editor().is_empty());
        assert_eq!(reply.try_recv(), Ok(PromptAnswer::Yes));
        let entries = app.transcript().entries();
        assert!(matches!(entries.last(), Some(Entry::Answer { .. })));
        assert!(
            !entries
                .iter()
                .any(|entry| matches!(entry, Entry::User { .. }))
        );
    }

    #[test]
    fn prompts_queue_and_open_in_order() {
        let (mut app, _) = new_app();
        let mut first = ask_permission(&mut app, "first");
        let mut second = ask_permission(&mut app, "second");
        assert!(app.has_modal());
        key(&mut app, KeyCode::Char('n'));
        assert_eq!(first.try_recv(), Ok(PromptAnswer::No));
        assert!(app.has_modal(), "the second prompt opens next");
        assert!(second.try_recv().is_err());
        key(&mut app, KeyCode::Char('a'));
        assert_eq!(second.try_recv(), Ok(PromptAnswer::Always));
        assert!(!app.has_modal());
    }

    #[test]
    fn esc_gives_safe_answers() {
        let (mut app, _) = new_app();
        let mut permission = ask_permission(&mut app, "rm -rf build");
        key(&mut app, KeyCode::Esc);
        assert_eq!(permission.try_recv(), Ok(PromptAnswer::No));

        let (tx, mut memory) = oneshot::channel();
        app.handle(UiMsg::Request(UiRequest::Memory {
            topic: "workflow".to_owned(),
            fact: "commits on dev".to_owned(),
            reply: tx,
        }));
        key(&mut app, KeyCode::Esc);
        assert_eq!(memory.try_recv(), Ok(MemoryAnswer::Drop));

        let (tx, mut plan) = oneshot::channel();
        app.handle(UiMsg::Request(UiRequest::Plan {
            plan: "# Plan\n\n1. [ ] a\n".to_owned(),
            reply: tx,
        }));
        key(&mut app, KeyCode::Esc);
        assert!(app.has_modal(), "Esc on a plan asks for feedback");
        type_line(&mut app, "no, smaller");
        key(&mut app, KeyCode::Enter);
        assert_eq!(
            plan.try_recv(),
            Ok(PlanDecision::Revise("no, smaller".to_owned()))
        );
        let text = app.transcript().plain_text();
        assert!(
            text.contains("? build this plan? revise: no, smaller"),
            "{text}"
        );
    }

    #[test]
    fn ctrl_c_once_cancels_twice_quits() {
        let (mut app, base) = new_app();
        assert_eq!(ctrl_c(&mut app), None, "idle: nothing to cancel");
        app.handle(UiMsg::Tick(base + Duration::from_secs(3)));
        app.handle(UiMsg::Busy(true));
        assert_eq!(ctrl_c(&mut app), Some(Action::CancelTurn));
        app.handle(UiMsg::Tick(base + Duration::from_secs(4)));
        assert_eq!(ctrl_c(&mut app), Some(Action::Quit));
    }

    #[test]
    fn ctrl_c_closes_the_prompt_safely_then_cancels() {
        let (mut app, base) = new_app();
        app.handle(UiMsg::Busy(true));
        let mut reply = ask_permission(&mut app, "cargo test");
        assert_eq!(ctrl_c(&mut app), Some(Action::CancelTurn));
        assert_eq!(reply.try_recv(), Ok(PromptAnswer::No));
        assert!(!app.has_modal());
        app.handle(UiMsg::Tick(base + Duration::from_millis(500)));
        assert_eq!(ctrl_c(&mut app), Some(Action::Quit));
    }

    #[test]
    fn a_run_is_busy_until_it_ends() {
        let mut info = info();
        info.kind = SessionKind::Run;
        let mut app = App::new(info, Theme::mono(), Instant::now());
        assert_eq!(ctrl_c(&mut app), Some(Action::CancelTurn));
    }

    #[test]
    fn question_mark_opens_the_help_and_any_key_closes_it() {
        let (mut app, _) = new_app();
        key(&mut app, KeyCode::Char('?'));
        assert!(app.has_modal());
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).expect("terminal");
        terminal.draw(|frame| app.draw(frame)).expect("draw");
        let drawn = rows(&terminal).join("\n");
        assert!(drawn.contains("Ctrl+C"), "{drawn}");
        key(&mut app, KeyCode::Char('x'));
        assert!(!app.has_modal());
        assert!(app.editor().is_empty());
        assert_eq!(app.transcript().plain_text(), "welcome");
    }

    #[test]
    fn shift_tab_cycles_the_mode() {
        let (mut app, _) = new_app();
        let shift_tab = KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT);
        assert_eq!(
            app.handle(UiMsg::Key(shift_tab)),
            Some(Action::SetMode(PermissionMode::AcceptEdits))
        );
        let mut terminal = Terminal::new(TestBackend::new(100, 10)).expect("terminal");
        terminal.draw(|frame| app.draw(frame)).expect("draw");
        let row = &rows(&terminal)[6];
        assert!(row.contains("mode: accept-edits"), "{row:?}");
        for expected in [
            PermissionMode::Plan,
            PermissionMode::Bypass,
            PermissionMode::Ask,
        ] {
            assert_eq!(
                app.handle(UiMsg::Key(shift_tab)),
                Some(Action::SetMode(expected))
            );
        }
        app.handle(UiMsg::PermissionMode(PermissionMode::Plan));
        terminal.draw(|frame| app.draw(frame)).expect("draw");
        assert!(rows(&terminal)[6].contains("mode: plan"));
    }

    #[test]
    fn tab_completes_slash_commands() {
        let (mut app, _) = new_app();
        type_line(&mut app, "/pl");
        key(&mut app, KeyCode::Tab);
        assert_eq!(app.editor().text(), "/plain");
        key(&mut app, KeyCode::Tab);
        assert_eq!(app.editor().text(), "/plan");
    }

    #[test]
    fn shift_alt_ctrlj_enter_insert_newline() {
        let (mut app, _) = new_app();
        type_line(&mut app, "a");
        app.handle(UiMsg::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::SHIFT,
        )));
        type_line(&mut app, "b");
        app.handle(UiMsg::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)));
        type_line(&mut app, "c");
        app.handle(UiMsg::Key(KeyEvent::new(
            KeyCode::Char('j'),
            KeyModifiers::CONTROL,
        )));
        type_line(&mut app, "d");
        assert_eq!(
            app.editor().text(),
            "a
b
c
d"
        );
        assert_eq!(
            key(&mut app, KeyCode::Enter),
            Some(Action::Submit(
                "a
b
c
d"
                .to_owned()
            ))
        );
    }

    #[test]
    fn plan_message_reads_the_file() {
        let dir = tempfile::tempdir().expect("dir");
        let path: PathBuf = dir.path().join("plan.md");
        assert!(plan_message(&path).is_none());
        std::fs::write(&path, "# Plan\n\n- [ ] a\n").expect("write");
        match plan_message(&path) {
            Some(UiMsg::Plan { path: p, text }) => {
                assert_eq!(p, path);
                assert_eq!(text, "# Plan\n\n- [ ] a\n");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn submit_records_the_line_and_hands_it_over() {
        let (mut app, _) = new_app();
        type_line(&mut app, "hello there");
        assert_eq!(
            key(&mut app, KeyCode::Enter),
            Some(Action::Submit("hello there".to_owned()))
        );
        assert!(app.editor().is_empty());
        assert_eq!(
            app.transcript().plain_text(),
            "welcome
> hello there"
        );
        assert_eq!(key(&mut app, KeyCode::Enter), None);
    }

    #[test]
    fn run_mode_rejects_free_text_and_handles_its_own_commands() {
        let mut info = info();
        info.kind = SessionKind::Run;
        let mut app = App::new(info, Theme::mono(), Instant::now());
        type_line(&mut app, "do it");
        assert_eq!(key(&mut app, KeyCode::Enter), None);
        type_line(&mut app, "/compact");
        assert_eq!(key(&mut app, KeyCode::Enter), None);
        type_line(&mut app, "/help");
        assert_eq!(key(&mut app, KeyCode::Enter), None);
        assert!(app.has_modal());
        key(&mut app, KeyCode::Esc);
        type_line(&mut app, "/quit");
        assert_eq!(key(&mut app, KeyCode::Enter), Some(Action::Quit));
    }

    #[test]
    fn expand_command_expands_every_tool_call() {
        let (mut app, _) = new_app();
        for id in ["a", "b"] {
            app.handle(UiMsg::Loop(LoopEvent::ToolStarted {
                call: shell_call(id, "ls"),
            }));
        }
        type_line(&mut app, "/expand");
        assert_eq!(key(&mut app, KeyCode::Enter), None);
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).expect("terminal");
        terminal.draw(|frame| app.draw(frame)).expect("draw");
        let drawn = rows(&terminal);
        assert_eq!(drawn.iter().filter(|row| row.contains("args:")).count(), 2);
    }

    #[test]
    fn typing_jumps_to_bottom() {
        let (mut app, _) = new_app();
        for n in 0..100 {
            app.handle(UiMsg::Notice {
                text: format!("line {n}"),
                level: Level::Info,
            });
        }
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).expect("terminal");
        terminal.draw(|frame| app.draw(frame)).expect("draw");
        key(&mut app, KeyCode::PageUp);
        assert!(!app.transcript().is_following());
        key(&mut app, KeyCode::Char('x'));
        assert!(app.transcript().is_following());
    }

    #[test]
    fn shutdown_without_waiting_quits_at_once_and_with_waiting_on_a_key() {
        let (mut app, _) = new_app();
        assert_eq!(app.handle(UiMsg::Shutdown { wait_for_key: true }), None);
        assert!(app.is_finished());
        assert_eq!(key(&mut app, KeyCode::Char('x')), Some(Action::Quit));
        let (mut app, _) = new_app();
        assert_eq!(
            app.handle(UiMsg::Shutdown {
                wait_for_key: false
            }),
            Some(Action::Quit)
        );
    }

    #[test]
    fn state_and_usage_reach_the_status_line() {
        let (mut app, _) = new_app();
        app.handle(UiMsg::Mode(State::Plan));
        assert_eq!(app.prompt_prefix(), "plan > ");
        app.handle(UiMsg::Loop(LoopEvent::StateEntered {
            state: State::Review,
            visit: 1,
        }));
        assert_eq!(app.prompt_prefix(), "review > ");
        let mut terminal = Terminal::new(TestBackend::new(80, 10)).expect("terminal");
        terminal.draw(|frame| app.draw(frame)).expect("draw");
        let row = &rows(&terminal)[6];
        assert!(row.starts_with(" REVIEW · main (test-model)"), "{row:?}");
    }
}
