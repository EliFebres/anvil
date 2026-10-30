//! The error type for both stores.

use std::path::PathBuf;

use thiserror::Error;

/// Why a rule or fact file could not be used.
///
/// Every variant names the file or the rule, so the message stands on its
/// own in a log line or a CLI error.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MemoryError {
    /// A file or directory could not be read or written.
    #[error("cannot access {}: {detail}", path.display())]
    Io {
        /// The path that failed.
        path: PathBuf,
        /// The operating system's message.
        detail: String,
    },

    /// A rules file is not valid TOML of the expected shape.
    #[error("{}: {detail}", path.display())]
    Parse {
        /// The file.
        path: PathBuf,
        /// The parser's message.
        detail: String,
    },

    /// A rule id is not kebab-case.
    #[error(
        "rule id `{id}` must be kebab-case (a-z, 0-9, single hyphens) and at most {max} characters"
    )]
    InvalidRuleId {
        /// The id as written.
        id: String,
        /// The length limit.
        max: usize,
    },

    /// Two rules share an id.
    #[error("rule id `{id}` is already defined")]
    DuplicateRule {
        /// The id.
        id: String,
    },

    /// No rule has this id.
    #[error("no rule with id `{id}`; `forge rules list` shows them")]
    UnknownRule {
        /// The id asked for.
        id: String,
    },

    /// A rule has no text.
    #[error("rule `{id}` has no text")]
    EmptyRule {
        /// The id.
        id: String,
    },

    /// A rule lists no languages.
    #[error("rule `{id}` lists no languages; use [\"*\"] for all of them")]
    NoLanguages {
        /// The id.
        id: String,
    },

    /// A rule's regex check does not compile.
    #[error("rule `{id}` has an invalid regex check: {detail}")]
    BadRegex {
        /// The id.
        id: String,
        /// The regex engine's message.
        detail: String,
    },

    /// A rule's command check is empty.
    #[error("rule `{id}` has an empty command check")]
    BadCommand {
        /// The id.
        id: String,
    },
}

impl MemoryError {
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
