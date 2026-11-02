//! `factory watch`: a full-screen dashboard of what the factory is doing.
//!
//! It reads `~/.factory/status.json`, which the service writes, and never
//! controls the service except through the pause flag. Everything here is
//! pure except [`runner`], which owns the terminal and runs the commands
//! the keys ask for: [`App`] consumes [`WatchMsg`]s and draws into a frame,
//! so tests drive it with messages and a `TestBackend`.

pub mod app;
mod fmt;
mod idle;
mod parts;
pub mod runner;
mod working;

use chrono::{DateTime, Utc};
use factory_core::Status;
use ratatui::crossterm::event::KeyEvent;

pub use app::App;
pub use runner::{WatchOptions, watch};

/// The reason `p` writes into the pause flag.
pub const PAUSE_REASON: &str = "paused from factory watch";

/// How often the dashboard rereads the status file.
pub const REFRESH_SECS: u64 = 2;

/// What the dashboard reacts to.
#[derive(Debug, Clone)]
pub enum WatchMsg {
    /// A fresh read of the status file and the pause flag.
    Status {
        /// The last good snapshot, if any was ever read.
        status: Option<Box<Status>>,
        /// The pause flag's reason when the flag is set.
        pause_flag: Option<String>,
        /// Why the latest read failed, when it did.
        read_error: Option<String>,
    },
    /// A key press.
    Key(KeyEvent),
    /// The clock moved; countdowns and elapsed times redraw.
    Tick(DateTime<Utc>),
    /// A one-line message for the footer, such as a URL that could not open.
    Notice(String),
}

/// What a key asks the runner to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Leave the dashboard.
    Quit,
    /// Write the pause flag.
    Pause,
    /// Remove the pause flag.
    Resume,
    /// Show the service's journal in a pager.
    OpenLog,
    /// Show a ticket file in a pager.
    OpenTicket(String),
    /// Open a pull request in the browser.
    OpenPr(String),
    /// Run `anvil doctor`.
    Doctor,
}
