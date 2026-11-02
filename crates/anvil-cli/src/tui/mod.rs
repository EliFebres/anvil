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
pub mod modal;
pub mod runner;
pub mod status;
pub mod theme;
pub mod transcript;
pub mod welcome;
pub mod wrap;

use std::path::PathBuf;
use std::time::Instant;

use anvil_core::agent::{LoopEvent, PlanDecision, State};
use anvil_core::tools::{PermissionRequest, PromptAnswer};
use anvil_types::Price;
use crossterm::event::{KeyEvent, MouseEvent};
use tokio::sync::{mpsc, oneshot};

pub use app::App;
pub use bridge::{ResultTee, TuiGate, TuiPrompt, UiHandle, block_on_reply, wait_cancel};
pub use theme::Theme;

/// The permission mode shown in the status bar and cycled with Shift+Tab.
///
/// Only the label exists so far; the registry still runs in `ask` mode.
// TODO(permission-modes): feed the chosen mode into anvil-core's permissions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionMode {
    /// Ask before every call that is not allowed by a rule.
    Ask,
    /// Allow file edits without asking.
    AcceptEdits,
    /// Read-only tools only.
    Plan,
    /// Allow everything, as `--yes` does.
    Bypass,
}

impl PermissionMode {
    /// Every mode, in Shift+Tab order.
    pub const ALL: [PermissionMode; 4] = [
        PermissionMode::Ask,
        PermissionMode::AcceptEdits,
        PermissionMode::Plan,
        PermissionMode::Bypass,
    ];

    /// The mode's name as `/mode` takes it.
    ///
    /// # Returns
    ///
    /// `ask`, `accept-edits`, `plan` or `bypass`.
    pub fn label(self) -> &'static str {
        match self {
            PermissionMode::Ask => "ask",
            PermissionMode::AcceptEdits => "accept-edits",
            PermissionMode::Plan => "plan",
            PermissionMode::Bypass => "bypass",
        }
    }

    /// The mode after this one, wrapping around.
    ///
    /// # Returns
    ///
    /// The next mode.
    pub fn next(self) -> Self {
        let index = Self::ALL.iter().position(|mode| *mode == self).unwrap_or(0);
        Self::ALL[(index + 1) % Self::ALL.len()]
    }

    /// Parses a mode name.
    ///
    /// # Arguments
    ///
    /// * `name` - The name, as `label` gives it.
    ///
    /// # Returns
    ///
    /// The mode, or `None` for any other text.
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| mode.label() == name)
    }
}

/// What a session tells the UI about itself when it starts.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    /// Whether the input takes free text (chat) or only slash commands (run).
    pub kind: SessionKind,
    /// The project root: paths under it are shown relative to it, and the
    /// paths tools write resolve against it.
    pub cwd: PathBuf,
    /// The coder's model id, shown in the status line.
    pub model: String,
    /// The coder's context window, for the context percentage.
    pub context_window: u32,
    /// The coder's price, for the cost estimate; `None` hides it.
    pub price: Option<Price>,
    /// The permission mode shown in the status line.
    pub permission_mode: PermissionMode,
    /// The state shown in the prompt before the first event.
    pub initial_state: State,
    /// Every slash command Tab may complete, each starting with `/`.
    pub slash_commands: Vec<String>,
    /// The anvil version, for the startup screen.
    pub version: String,
    /// The coder as `name (model)`.
    pub coder: String,
    /// The reviewer as `name (model)`, if one is configured.
    pub reviewer: Option<String>,
    /// The standing rules as `(id, text)`.
    pub rules: Vec<(String, String)>,
    /// How many skills are loaded.
    pub skills: usize,
    /// How many MCP servers are configured.
    pub mcp_servers: usize,
    /// The newest saved sessions, newest first.
    pub recent_sessions: Vec<RecentSession>,
    /// Whether the logo draws in over time; off when stdout is not a
    /// terminal or `ANVIL_NO_ANIMATION` is set.
    pub animate: bool,
}

/// A saved session as the startup screen lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentSession {
    /// The session id.
    pub id: String,
    /// How it ended, or where it stopped.
    pub status: String,
    /// Whether it ended well; `None` when it did not end.
    pub ok: Option<bool>,
    /// The task.
    pub task: String,
    /// When it started, in local time.
    pub when: String,
}

/// Whether the startup logo should animate.
///
/// # Arguments
///
/// * `is_tty` - Whether stdout is a terminal.
/// * `no_animation` - Whether `ANVIL_NO_ANIMATION` is set.
///
/// # Returns
///
/// True only on a terminal without the opt-out.
pub fn animate_logo(is_tty: bool, no_animation: bool) -> bool {
    is_tty && !no_animation
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

/// What the user said about a proposed memory fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryAnswer {
    /// Save it.
    Keep,
    /// Forget it.
    Drop,
}

/// A question the session asks the user; the UI answers on the channel it
/// carries. Dropping the sender gives the asker its safe answer.
#[derive(Debug)]
pub enum UiRequest {
    /// May this tool call run?
    Permission {
        /// What is about to run.
        request: PermissionRequest,
        /// Where the answer goes.
        reply: oneshot::Sender<PromptAnswer>,
    },
    /// Should this plan be built?
    Plan {
        /// The plan text.
        plan: String,
        /// Where the decision goes.
        reply: oneshot::Sender<PlanDecision>,
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
}

/// Everything the UI consumes, from the session and from the terminal.
#[derive(Debug)]
pub enum UiMsg {
    /// An event from the agent loop or the chat turn.
    Loop(LoopEvent),
    /// A question for the user, shown as a modal.
    Request(UiRequest),
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
    /// The permission mode changed (`/mode`).
    PermissionMode(PermissionMode),
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
    /// The user cycled the permission mode with Shift+Tab.
    Mode(PermissionMode),
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
