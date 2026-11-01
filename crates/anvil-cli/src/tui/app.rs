//! The UI state: consumes messages, keeps the three regions, draws them.

use std::time::{Duration, Instant};

use anvil_core::agent::{LoopEvent, State};
use crossterm::event::{KeyEvent, MouseButton, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::widgets::Paragraph;
use unicode_width::UnicodeWidthStr;

use super::editor::{EditKey, Editor};
use super::keys::{self, KeyAction};
use super::status::StatusBar;
use super::theme::Theme;
use super::transcript::{Entry, Transcript};
use super::{Level, SessionInfo, SessionKind, UiMsg};

/// How soon a second Ctrl+C must follow the first to quit.
pub const QUIT_WINDOW: Duration = Duration::from_secs(2);

/// What the UI asks the runner to do after handling a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Hand this line to the session.
    Submit(String),
    /// Close the UI and end the session.
    Quit,
}

/// The whole UI state. Pure: time comes in as ticks and nothing here
/// touches the terminal, the clock or the environment.
#[derive(Debug)]
pub struct App {
    info: SessionInfo,
    theme: Theme,
    transcript: Transcript,
    editor: Editor,
    status: StatusBar,
    state: State,
    busy: bool,
    now: Instant,
    tick: u64,
    last_ctrl_c: Option<Instant>,
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
        let transcript = Transcript::new(info.cwd.clone());
        Self {
            status: StatusBar::new(&info, now),
            state: info.initial_state,
            info,
            theme,
            transcript,
            editor: Editor::new(),
            busy: false,
            now,
            tick: 0,
            last_ctrl_c: None,
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
        match msg {
            UiMsg::Loop(event) => {
                self.apply_loop(&event);
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
            UiMsg::Model(model) => {
                self.status.set_model(&model);
                None
            }
            UiMsg::Busy(busy) => {
                self.busy = busy;
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

    /// Draws the transcript, the status line and the input.
    ///
    /// # Arguments
    ///
    /// * `frame` - The frame.
    pub fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let prefix = self.prompt_prefix();
        let input_height = self.editor.height(area.width, prefix.width());
        let [transcript_area, status_area, input_area] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(input_height),
        ])
        .areas(area);
        self.transcript
            .draw(frame, transcript_area, &self.theme, self.tick);
        frame.render_widget(
            self.status.line(status_area.width, self.now, &self.theme),
            status_area,
        );
        let (lines, (column, row)) = self.editor.render(input_area.width, &prefix, &self.theme);
        frame.render_widget(Paragraph::new(lines), input_area);
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
                "help" => {
                    self.show_help();
                    self.transcript
                        .notice("during a run: /expand, /help, /quit", Level::Warn);
                }
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

    /// Ctrl+C: quit on a quick second press, otherwise explain.
    fn ctrl_c(&mut self) -> Option<Action> {
        if let Some(last) = self.last_ctrl_c
            && self.now.saturating_duration_since(last) <= QUIT_WINDOW
        {
            return Some(Action::Quit);
        }
        self.last_ctrl_c = Some(self.now);
        let text = if self.busy {
            "cancelling a turn is not available yet; press Ctrl+C again within 2 s to quit"
        } else {
            "press Ctrl+C again within 2 s to quit"
        };
        self.transcript.notice(text, Level::Warn);
        None
    }

    /// Shows the key help in the transcript.
    fn show_help(&mut self) {
        let width = keys::HELP
            .iter()
            .map(|(key, _)| key.width())
            .max()
            .unwrap_or(0);
        let text = keys::HELP
            .iter()
            .map(|(key, meaning)| format!("  {key:<width$}  {meaning}"))
            .collect::<Vec<_>>()
            .join("\n");
        self.transcript.notice(format!("keys\n{text}"), Level::Warn);
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
        AgentLoop, AutoApprove, CheckCommand, EventSink, LoopConfig, LoopProviders,
    };
    use anvil_core::provider::{Event, FakeProvider, Provider, StopReason, ToolCall, Usage};
    use anvil_core::tools::{
        OutputStats, Permissions, PromptAnswer, StaticPrompt, ToolContext, ToolOutcome,
        ToolRegistry,
    };
    use anvil_types::LimitsConfig;
    use crossterm::event::{KeyCode, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::tui::runner::plan_message;

    const PLAN_LINE: &str = "1. [ ] Write notes.txt";

    fn info() -> SessionInfo {
        SessionInfo {
            kind: SessionKind::Chat,
            cwd: PathBuf::new(),
            model: "main (test-model)".to_owned(),
            context_window: 100_000,
            price: None,
            permission_mode: "ask".to_owned(),
            initial_state: State::Build,
            slash_commands: vec!["/plan".to_owned(), "/plain".to_owned()],
        }
    }

    fn new_app() -> (App, Instant) {
        let base = Instant::now();
        (App::new(info(), Theme::mono(), base), base)
    }

    fn key(app: &mut App, code: KeyCode) -> Option<Action> {
        app.handle(UiMsg::Key(KeyEvent::new(code, KeyModifiers::NONE)))
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
        let collapsed = rows(&terminal);
        assert!(
            collapsed[0].starts_with("$ shell cargo test --all  ok 1.2s  running 3 tests"),
            "{:?}",
            collapsed[0]
        );
        key(&mut app, KeyCode::Enter);
        terminal.draw(|frame| app.draw(frame)).expect("draw");
        insta::assert_snapshot!(terminal.backend());
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
        assert_eq!(app.transcript().plain_text(), "> hello there");
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
        type_line(&mut app, "/quit");
        assert_eq!(key(&mut app, KeyCode::Enter), Some(Action::Quit));
    }

    #[test]
    fn expand_command_expands_every_tool_call() {
        let (mut app, _) = new_app();
        for id in ["a", "b"] {
            app.handle(UiMsg::Loop(LoopEvent::ToolStarted {
                call: ToolCall {
                    id: id.to_owned(),
                    name: "shell".to_owned(),
                    arguments_json: "{\"command\": \"ls\"}".to_owned(),
                },
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
    fn ctrl_c_twice_within_two_seconds_quits() {
        let (mut app, base) = new_app();
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(app.handle(UiMsg::Key(ctrl_c)), None);
        app.handle(UiMsg::Tick(base + Duration::from_secs(3)));
        assert_eq!(app.handle(UiMsg::Key(ctrl_c)), None);
        app.handle(UiMsg::Tick(base + Duration::from_secs(4)));
        assert_eq!(app.handle(UiMsg::Key(ctrl_c)), Some(Action::Quit));
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
        let row = &rows(&terminal)[8];
        assert!(row.starts_with(" REVIEW · main (test-model)"), "{row:?}");
    }
}
