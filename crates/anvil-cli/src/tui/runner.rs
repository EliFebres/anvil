//! The UI task: owns the terminal, feeds the [`App`] and draws it.
//!
//! This is the only module under `tui` that touches the terminal, the
//! clock, the file system or the environment.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Once;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anvil_core::agent::LoopEvent;
use anvil_core::provider::ToolCall;
use crossterm::event::{Event, EventStream};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures_util::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::{mpsc, watch};
use tokio::time::MissedTickBehavior;

use super::app::{Action, App};
use super::transcript::is_plan_file;
use super::{Input, UiExit, UiMsg};

/// How often the UI ticks for spinners and the clock.
pub const TICK: Duration = Duration::from_millis(120);

/// Whether the terminal is in raw mode on the alternate screen.
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Installs the panic hook once.
static PANIC_HOOK: Once = Once::new();

/// Raw mode and the alternate screen, undone on drop, on panic, or by an
/// explicit [`TerminalGuard::restore`]. The mouse is left to the terminal
/// so dragging still selects and copies text.
#[derive(Debug)]
pub struct TerminalGuard {
    _private: (),
}

impl TerminalGuard {
    /// Puts the terminal into full-screen mode.
    ///
    /// # Returns
    ///
    /// The guard.
    ///
    /// # Errors
    ///
    /// When the terminal refuses raw mode or the alternate screen; the
    /// terminal is left as it was.
    pub fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        if let Err(error) = execute!(io::stdout(), EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(error);
        }
        ACTIVE.store(true, Ordering::SeqCst);
        PANIC_HOOK.call_once(|| {
            let previous = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                Self::restore();
                previous(info);
            }));
        });
        Ok(Self { _private: () })
    }

    /// Restores the terminal. Safe to call more than once.
    pub fn restore() {
        if ACTIVE.swap(false, Ordering::SeqCst) {
            let _ = execute!(io::stdout(), LeaveAlternateScreen);
            let _ = disable_raw_mode();
        }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        Self::restore();
    }
}

/// The channels the UI task reads and writes.
#[derive(Debug)]
pub struct UiChannels {
    /// Events from the agent loop or the chat turn.
    pub events: mpsc::UnboundedReceiver<LoopEvent>,
    /// Everything else the session sends.
    pub msgs: mpsc::UnboundedReceiver<UiMsg>,
    /// Lines and quit go back to the session here.
    pub input: mpsc::UnboundedSender<Input>,
    /// Bumped once per cancelled turn; the session watches it.
    pub cancel: watch::Sender<u64>,
    /// The project root, which relative tool paths resolve against.
    pub cwd: PathBuf,
}

/// Follows the plan file: it is read when the loop writes it and again
/// after every `edit_file` or `write_file` that targets it, since ticking a
/// checkbox emits no event of its own.
#[derive(Debug)]
struct PlanWatch {
    cwd: PathBuf,
    plan: Option<PathBuf>,
    /// The file each pending edit call targets, by call id.
    edits: HashMap<String, PathBuf>,
}

impl PlanWatch {
    fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            plan: None,
            edits: HashMap::new(),
        }
    }

    /// The plan message an event calls for, if any.
    fn observe(&mut self, event: &LoopEvent) -> Option<UiMsg> {
        match event {
            LoopEvent::ToolStarted { call } if edits_a_file(call) => {
                if let Some(path) = path_argument(call) {
                    self.edits.insert(call.id.clone(), self.cwd.join(path));
                }
                None
            }
            LoopEvent::PlanWritten { path } => {
                self.plan = Some(path.clone());
                plan_message(path)
            }
            LoopEvent::ToolFinished { call_id, .. } => {
                let edited = self.edits.remove(call_id)?;
                let plan = self.plan.as_ref()?;
                if is_plan_file(&edited, plan) {
                    plan_message(plan)
                } else {
                    None
                }
            }
            _ => None,
        }
    }
}

/// Whether a call can change a file's contents.
fn edits_a_file(call: &ToolCall) -> bool {
    call.name == "edit_file" || call.name == "write_file"
}

/// The `path` argument of a call, if it has one.
fn path_argument(call: &ToolCall) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(&call.arguments_json)
        .ok()?
        .get("path")?
        .as_str()
        .map(str::to_owned)
}

/// The message that shows a plan file, read from disk.
///
/// # Arguments
///
/// * `path` - The plan file.
///
/// # Returns
///
/// The message, or `None` when the file cannot be read.
pub fn plan_message(path: &Path) -> Option<UiMsg> {
    let text = std::fs::read_to_string(path).ok()?;
    Some(UiMsg::Plan {
        path: path.to_path_buf(),
        text,
    })
}

/// Runs the UI until the session ends or the user leaves.
///
/// Every message that is already waiting is handled before a draw, so a
/// fast stream of deltas does not become a frame per delta.
///
/// # Arguments
///
/// * `app` - The UI state.
/// * `channels` - The session's channels.
///
/// # Returns
///
/// How the UI ended, with the terminal restored.
///
/// # Errors
///
/// When the terminal cannot be set up or drawn to.
pub async fn run_ui(mut app: App, mut channels: UiChannels) -> io::Result<UiExit> {
    let guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut keys = EventStream::new();
    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut plan = PlanWatch::new(channels.cwd.clone());
    let mut events_open = true;
    terminal.draw(|frame| app.draw(frame))?;
    loop {
        let mut actions: Vec<Action> = Vec::new();
        tokio::select! {
            event = channels.events.recv(), if events_open => match event {
                Some(event) => actions.extend(handle_event(&mut app, &mut plan, event)),
                None => events_open = false,
            },
            msg = channels.msgs.recv() => match msg {
                Some(msg) => actions.extend(app.handle(msg)),
                None => break,
            },
            key = keys.next() => match key {
                Some(Ok(event)) => {
                    if let Some(msg) = terminal_event(event) {
                        actions.extend(app.handle(msg));
                    }
                }
                Some(Err(error)) => return Err(error),
                None => break,
            },
            _ = ticker.tick() => actions.extend(app.handle(UiMsg::Tick(Instant::now()))),
        }
        while let Ok(event) = channels.events.try_recv() {
            actions.extend(handle_event(&mut app, &mut plan, event));
        }
        while let Ok(msg) = channels.msgs.try_recv() {
            actions.extend(app.handle(msg));
        }
        let mut quit = false;
        for action in actions {
            match action {
                Action::Submit(line) => {
                    let _ = channels.input.send(Input::Line(line));
                }
                Action::SetMode(mode) => {
                    let _ = channels.input.send(Input::Mode(mode));
                }
                Action::CancelTurn => channels.cancel.send_modify(|n| *n = n.wrapping_add(1)),
                Action::Quit => quit = true,
            }
        }
        if quit {
            break;
        }
        terminal.draw(|frame| app.draw(frame))?;
    }
    let _ = channels.input.send(Input::Quit);
    drop(terminal);
    drop(guard);
    Ok(UiExit::Quit {
        events: channels.events,
    })
}

/// Hands a loop event to the UI, with the plan update it implies.
fn handle_event(app: &mut App, plan: &mut PlanWatch, event: LoopEvent) -> Vec<Action> {
    let mut actions = Vec::new();
    let plan_update = plan.observe(&event);
    actions.extend(app.handle(UiMsg::Loop(event)));
    if let Some(msg) = plan_update {
        actions.extend(app.handle(msg));
    }
    actions
}

/// Turns a terminal event into a UI message, dropping the kinds the UI
/// does not use.
fn terminal_event(event: Event) -> Option<UiMsg> {
    match event {
        Event::Key(key) => Some(UiMsg::Key(key)),
        Event::Mouse(mouse) => Some(UiMsg::Mouse(mouse)),
        Event::Resize(_, _) => Some(UiMsg::Resize),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_watch_rereads_after_an_edit_of_the_plan_file() {
        let dir = tempfile::tempdir().expect("dir");
        let plan_path = dir.path().join("plan.md");
        std::fs::write(&plan_path, "- [ ] a\n").expect("write");
        let mut watch = PlanWatch::new(dir.path().to_path_buf());
        assert!(matches!(
            watch.observe(&LoopEvent::PlanWritten {
                path: plan_path.clone()
            }),
            Some(UiMsg::Plan { .. })
        ));
        let edit = ToolCall {
            id: "e1".to_owned(),
            name: "edit_file".to_owned(),
            arguments_json: "{\"path\": \"plan.md\"}".to_owned(),
        };
        let other = ToolCall {
            id: "e2".to_owned(),
            name: "write_file".to_owned(),
            arguments_json: "{\"path\": \"notes.txt\"}".to_owned(),
        };
        assert!(
            watch
                .observe(&LoopEvent::ToolStarted { call: edit })
                .is_none()
        );
        assert!(
            watch
                .observe(&LoopEvent::ToolStarted { call: other })
                .is_none()
        );
        std::fs::write(&plan_path, "- [x] a\n").expect("write");
        let finished = |id: &str| LoopEvent::ToolFinished {
            call_id: id.to_owned(),
            name: "edit_file".to_owned(),
            outcome: anvil_core::tools::ToolOutcome::Completed,
            preview: String::new(),
            stats: anvil_core::tools::OutputStats {
                bytes: 0,
                truncated: false,
                digest: 0,
            },
        };
        assert!(watch.observe(&finished("e2")).is_none());
        match watch.observe(&finished("e1")) {
            Some(UiMsg::Plan { text, .. }) => assert_eq!(text, "- [x] a\n"),
            other => panic!("unexpected: {other:?}"),
        }
    }
}
