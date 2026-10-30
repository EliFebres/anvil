//! The error type for recording, drafting and submitting.

use std::path::PathBuf;

use forge_types::ValidationError;
use thiserror::Error;

/// Why a telemetry or ticket operation could not finish.
///
/// Every variant names the file, id or command concerned, so the message
/// stands on its own in a log line or a CLI error.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TelemetryError {
    /// A file or directory could not be read or written.
    #[error("cannot access {}: {detail}", path.display())]
    Io {
        /// The path that failed.
        path: PathBuf,
        /// The operating system's message.
        detail: String,
    },

    /// A file is not valid JSONL or TOML of the expected shape.
    #[error("{}:{line}: {detail}", path.display())]
    Parse {
        /// The file.
        path: PathBuf,
        /// The line, starting at 1; 0 when the whole file failed.
        line: usize,
        /// The parser's message.
        detail: String,
    },

    /// A drafted ticket failed the forge/factory data boundary.
    #[error("draft rejected by the ticket validator: {0}")]
    Validation(#[from] ValidationError),

    /// The drafter's reply held no usable JSON draft.
    #[error("the drafter's reply is not a ticket draft: {detail}")]
    BadDraft {
        /// What was wrong.
        detail: String,
    },

    /// The drafter (the summarizer model) failed.
    #[error("drafting failed: {detail}")]
    Drafter {
        /// The provider's message.
        detail: String,
    },

    /// No draft has this id or prefix.
    #[error("no draft ticket matches `{id}`; `forge ticket list` shows them")]
    UnknownTicket {
        /// The id or prefix asked for.
        id: String,
    },

    /// More than one draft matches an id prefix.
    #[error("`{id}` matches more than one draft; give more of the id")]
    AmbiguousTicket {
        /// The prefix asked for.
        id: String,
    },

    /// A git command failed.
    #[error("git {command} failed: {detail}")]
    Git {
        /// The subcommand and its arguments.
        command: String,
        /// Git's stderr, trimmed.
        detail: String,
    },

    /// A `--since` value is not a duration.
    #[error("`{text}` is not a duration; use a number followed by m, h, d or w")]
    BadDuration {
        /// The text as given.
        text: String,
    },

    /// A session id is not a ULID, so it cannot be a ticket evidence ref.
    #[error("session id `{id}` is not a ULID")]
    BadSessionId {
        /// The id as recorded.
        id: String,
    },
}

impl TelemetryError {
    /// Wraps an I/O error with the path it concerned.
    ///
    /// # Arguments
    ///
    /// * `path` - The path that failed.
    /// * `error` - The error.
    ///
    /// # Returns
    ///
    /// The wrapped error.
    pub fn io(path: impl Into<PathBuf>, error: &std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            detail: error.to_string(),
        }
    }
}
