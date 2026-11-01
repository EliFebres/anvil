//! The UI state: consumes messages, keeps the three regions, draws them.

use std::time::{Duration, Instant};

use anvil_core::agent::{LoopEvent, State};
use crossterm::event::{KeyEvent, MouseEventKind};
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
        Self {
            status: StatusBar::new(&info, now),
            state: info.initial_state,
            info,
            theme,
            transcript: Transcript::new(),
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
        self.transcript.draw(frame, transcript_area, &self.theme);
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
        self.transcript.apply(event);
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
            KeyAction::Complete => {
                self.editor.complete(&self.info.slash_commands);
                None
            }
            KeyAction::Escape | KeyAction::Ignore => None,
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

    /// Sends the input, or handles it when it is a UI-only command.
    fn submit(&mut self) -> Option<Action> {
        if self.editor.is_empty() {
            return None;
        }
        let text = self.editor.take().trim().to_owned();
        self.editor.push_history(&text);
        self.transcript.jump_bottom();
        self.transcript.push(Entry::User { text: text.clone() });
        if let Some(command) = text.strip_prefix('/') {
            match command.split_whitespace().next().unwrap_or("") {
                "expand" => {
                    self.transcript
                        .notice("/expand is not available yet", Level::Warn);
                    return None;
                }
                "plain" => {
                    self.transcript
                        .notice("/plain is not available yet", Level::Warn);
                    return None;
                }
                _ => {}
            }
        } else if self.info.kind == SessionKind::Run {
            self.transcript.notice(
                "a run takes only slash commands; /help lists them",
                Level::Warn,
            );
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
    use crossterm::event::{KeyCode, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;

    fn info() -> SessionInfo {
        SessionInfo {
            kind: SessionKind::Chat,
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

    #[test]
    fn empty_session() {
        let (mut app, _) = new_app();
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).expect("terminal");
        terminal.draw(|frame| app.draw(frame)).expect("draw");
        insta::assert_snapshot!(terminal.backend());
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
    fn run_mode_rejects_free_text() {
        let mut info = info();
        info.kind = SessionKind::Run;
        let mut app = App::new(info, Theme::mono(), Instant::now());
        type_line(&mut app, "do it");
        assert_eq!(key(&mut app, KeyCode::Enter), None);
        type_line(&mut app, "/help");
        assert_eq!(
            key(&mut app, KeyCode::Enter),
            Some(Action::Submit("/help".to_owned()))
        );
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
        let row: String = (0..80)
            .map(|x| {
                terminal
                    .backend()
                    .buffer()
                    .cell((x, 8))
                    .map(|cell| cell.symbol().to_owned())
                    .unwrap_or_default()
            })
            .collect();
        assert!(row.starts_with(" REVIEW · main (test-model)"), "{row:?}");
    }
}
