//! The error type of the factory.

use std::path::PathBuf;

use thiserror::Error;

/// Why a factory operation failed.
#[derive(Debug, Error)]
pub enum FactoryError {
    /// A git command exited non-zero.
    #[error("git {command}: {stderr}")]
    Git {
        /// The arguments, space separated.
        command: String,
        /// What git said.
        stderr: String,
    },

    /// A program could not be started.
    #[error("cannot run {program}: {source}")]
    Spawn {
        /// The program.
        program: String,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },

    /// A filesystem operation failed.
    #[error("{action} {path}: {source}")]
    Io {
        /// What was attempted, such as `reading`.
        action: &'static str,
        /// The path involved.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },

    /// A ticket file does not parse or does not validate.
    #[error("ticket {path}: {detail}")]
    Ticket {
        /// The file.
        path: PathBuf,
        /// What is wrong with it.
        detail: String,
    },

    /// A JSON file does not parse or cannot be written.
    #[error("{path}: {source}")]
    Json {
        /// The file.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: serde_json::Error,
    },

    /// The configuration cannot be used as it stands.
    #[error("configuration: {0}")]
    Config(String),

    /// A ticket in `tickets/merged/` was about to be edited.
    #[error("ticket {id} is merged and is never edited again")]
    Immutable {
        /// The ticket id.
        id: String,
    },

    /// No ticket with this id exists in any state folder.
    #[error("no ticket {id} under tickets/")]
    UnknownTicket {
        /// The ticket id.
        id: String,
    },

    /// `main` has no eval result to gate against.
    #[error("no eval result under eval/results/ on main; run `eval baseline` and merge it first")]
    NoBaseline,

    /// The code host refused or failed a request.
    #[error("GitHub: HTTP {status}: {message}")]
    Host {
        /// The HTTP status, 0 when the request never completed.
        status: u16,
        /// GitHub's message or the transport error.
        message: String,
    },

    /// A notification could not be delivered.
    #[error("notification: {0}")]
    Notify(String),

    /// The sandbox could not run the implementer.
    #[error("sandbox: {0}")]
    Sandbox(String),
}

impl FactoryError {
    /// An I/O error with the path and action that failed.
    ///
    /// # Arguments
    ///
    /// * `action` - What was attempted, such as `reading`.
    /// * `path` - The path involved.
    /// * `source` - The underlying error.
    ///
    /// # Returns
    ///
    /// The error.
    pub fn io(action: &'static str, path: &std::path::Path, source: std::io::Error) -> Self {
        FactoryError::Io {
            action,
            path: path.to_path_buf(),
            source,
        }
    }
}

/// The result type of this crate.
pub type Result<T> = std::result::Result<T, FactoryError>;
