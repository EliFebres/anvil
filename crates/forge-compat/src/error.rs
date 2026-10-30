//! The error type for everything this crate loads.

use std::path::PathBuf;

use forge_types::ValidationError;
use thiserror::Error;

/// Why a Claude Code-format file or source could not be used.
///
/// Every variant names the file, so the message stands on its own in a log
/// line, a `forge doctor` report or a `forge compat check` row.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CompatError {
    /// A file or directory could not be read or written.
    #[error("cannot access {}: {detail}", path.display())]
    Io {
        /// The path that failed.
        path: PathBuf,
        /// The operating system's message.
        detail: String,
    },

    /// A Markdown file has no `---` frontmatter block at the top.
    #[error("{}: no frontmatter block (expected `---` ... `---` at the top)", path.display())]
    NoFrontmatter {
        /// The file.
        path: PathBuf,
    },

    /// The frontmatter is not valid YAML, or not a mapping.
    #[error("{}: frontmatter is not valid YAML: {detail}", path.display())]
    Frontmatter {
        /// The file.
        path: PathBuf,
        /// The parser's message.
        detail: String,
    },

    /// A required frontmatter key is absent or not a string.
    #[error("{}: frontmatter is missing `{field}`", path.display())]
    MissingField {
        /// The file.
        path: PathBuf,
        /// The key that must be present.
        field: &'static str,
    },

    /// The frontmatter parsed but failed the shared validator.
    #[error("{}: {source}", path.display())]
    Validation {
        /// The file.
        path: PathBuf,
        /// What the validator rejected.
        #[source]
        source: ValidationError,
    },

    /// `forge skill add` could not fetch, find or copy the skill.
    #[error("{detail}")]
    Install {
        /// What went wrong, with any hint about how to fix it.
        detail: String,
    },
}

/// A file that looked like one of ours but did not load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadError {
    /// The file that failed.
    pub path: PathBuf,
    /// Why.
    pub error: CompatError,
}

impl CompatError {
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
