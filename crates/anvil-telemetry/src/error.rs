//! The error type for recording and reading usage records.

use std::path::PathBuf;

use thiserror::Error;

/// Why a telemetry operation could not finish.
///
/// Every variant names the file or value concerned, so the message
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

    /// A `--since` value is not a duration.
    #[error("`{text}` is not a duration; use a number followed by m, h, d or w")]
    BadDuration {
        /// The text as given.
        text: String,
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
