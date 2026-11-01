//! The full-screen terminal UI: a transcript, a status line and a
//! multi-line input, drawn with ratatui over crossterm.
//!
//! Everything here is pure except [`runner`], which owns the terminal.
//! [`App`] consumes [`UiMsg`]s and draws into a frame, so tests drive it
//! with messages and a `TestBackend`. The module knows the core's events
//! and prompt types and nothing about how a session is wired: it never
//! imports `Wiring`, the memory crate, the compat crate or telemetry.

pub mod app;
pub mod bridge;
pub mod editor;
pub mod keys;
pub mod markdown;
pub mod runner;
pub mod status;
pub mod theme;
pub mod transcript;
pub mod wrap;

use std::path::PathBuf;
use std::time::Instant;

use anvil_core::agent::{LoopEvent, State};
use anvil_types::Price;
use crossterm::event::{KeyEvent, MouseEvent};
use tokio::sync::mpsc;

pub use app::App;
pub use bridge::{ResultTee, UiHandle};
pub use theme::Theme;

/// What a session tells the UI about itself when it starts.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    /// Whether the input takes free text (chat) or only slash commands (run).
    pub kind: SessionKind,
    /// The project root: paths under it are shown relative to it, and the
    /// paths tools write resolve against it.
    pub cwd: PathBuf,
    /// The coder's name and model, shown in the status line.
    pub model: String,
    /// The coder's context window, for the context percentage.
    pub context_window: u32,
    /// The coder's price, for the cost estimate; `None` hides it.
    pub price: Option<Price>,
    /// The permission mode label shown in the status line.
    pub permission_mode: String,
    /// The state shown in the prompt before the first event.
    pub initial_state: State,
    /// Every slash command Tab may complete, each starting with `/`.
    pub slash_commands: Vec<String>,
}

/// Which kind of session the UI fronts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionKind {
    /// `anvil chat`: free text and slash commands.
    Chat,
    /// `anvil run` or `anvil resume`: slash commands and keys only.
    Run,
}

/// How loud a notice is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// Routine progress, drawn dimmed.
    Info,
    /// Something to notice, drawn plainly.
    Warn,
    /// A failure, drawn with the failure accent.
    Error,
}

/// Everything the UI consumes, from the session and from the terminal.
#[derive(Debug)]
pub enum UiMsg {
    /// An event from the agent loop or the chat turn.
    Loop(LoopEvent),
    /// A tool call's full result, from the tool hook.
    ToolResult {
        /// The call.
        call_id: String,
        /// The result as the model saw it, already capped.
        text: String,
        /// Whether the tool failed.
        is_error: bool,
    },
    /// The plan file was written or edited.
    Plan {
        /// The file.
        path: PathBuf,
        /// Its contents.
        text: String,
    },
    /// The chat mode changed (`/plan`, `/build`, `/review`).
    Mode(State),
    /// The coder changed (`/model`).
    Model(String),
    /// Whether a turn is running.
    Busy(bool),
    /// A line the session wants shown.
    Notice {
        /// The text.
        text: String,
        /// How loud.
        level: Level,
    },
    /// The session is over; the UI may close.
    Shutdown {
        /// Whether to wait for a key before closing, so the last screen stays readable.
        wait_for_key: bool,
    },
    /// The clock advanced; the only source of time inside the UI.
    Tick(Instant),
    /// A key from the terminal.
    Key(KeyEvent),
    /// A mouse event from the terminal.
    Mouse(MouseEvent),
    /// The terminal was resized; the next draw re-lays everything out.
    Resize,
}

/// What the UI hands back to the session driver.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    /// A submitted line, trimmed, never empty.
    Line(String),
    /// The user left (`/quit`, Ctrl+C twice, or the UI closed).
    Quit,
}

/// How the UI task ended.
#[derive(Debug)]
pub enum UiExit {
    /// The terminal is restored. The receiver still holds any events the
    /// UI did not show, and more may arrive while the session winds down.
    Quit {
        /// The loop events the UI was consuming.
        events: mpsc::UnboundedReceiver<LoopEvent>,
    },
}
