//! The UI task: owns the terminal, feeds the [`App`] and draws it.
//!
//! This is the only module under `tui` that touches the terminal, the
//! clock or the environment.

use std::io;
use std::sync::Once;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anvil_core::agent::LoopEvent;
use crossterm::event::{DisableMouseCapture, EnableMouseCapture, Event, EventStream};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures_util::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;

use super::app::{Action, App};
use super::{Input, UiExit, UiMsg};

/// How often the UI ticks for spinners and the clock.
pub const TICK: Duration = Duration::from_millis(120);

/// Whether the terminal is in raw mode on the alternate screen.
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Installs the panic hook once.
static PANIC_HOOK: Once = Once::new();

/// Raw mode, the alternate screen and mouse capture, undone on drop, on
/// panic, or by an explicit [`TerminalGuard::restore`].
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
        if let Err(error) = execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture) {
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
            let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
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
    let mut events_open = true;
    terminal.draw(|frame| app.draw(frame))?;
    let exit = loop {
        let mut actions: Vec<Action> = Vec::new();
        tokio::select! {
            event = channels.events.recv(), if events_open => match event {
                Some(event) => actions.extend(app.handle(UiMsg::Loop(event))),
                None => events_open = false,
            },
            msg = channels.msgs.recv() => match msg {
                Some(msg) => actions.extend(app.handle(msg)),
                None => break UiExit::Quit,
            },
            key = keys.next() => match key {
                Some(Ok(event)) => {
                    if let Some(msg) = terminal_event(event) {
                        actions.extend(app.handle(msg));
                    }
                }
                Some(Err(error)) => return Err(error),
                None => break UiExit::Quit,
            },
            _ = ticker.tick() => actions.extend(app.handle(UiMsg::Tick(Instant::now()))),
        }
        while let Ok(event) = channels.events.try_recv() {
            actions.extend(app.handle(UiMsg::Loop(event)));
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
                Action::Quit => quit = true,
            }
        }
        if quit {
            break UiExit::Quit;
        }
        terminal.draw(|frame| app.draw(frame))?;
    };
    let _ = channels.input.send(Input::Quit);
    drop(terminal);
    drop(guard);
    Ok(exit)
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
