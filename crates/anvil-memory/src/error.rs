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
    #[error("no rule with id `{id}`; `anvil rules list` shows them")]
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

    /// A template name is empty or holds characters other than a-z, 0-9, `-`, `_`.
    #[error("template name `{name}` must use only a-z, 0-9, `-` and `_`")]
    InvalidTemplateName {
        /// The name as written.
        name: String,
    },

    /// A `template.toml` is unusable.
    #[error("template `{name}`: {detail}")]
    InvalidTemplate {
        /// The template's directory name.
        name: String,
        /// What is wrong.
        detail: String,
    },

    /// No usable template has this name.
    #[error("no template named `{name}`; available: {available}")]
    UnknownTemplate {
        /// The name asked for.
        name: String,
        /// The usable names, comma-separated, or `none`.
        available: String,
    },

    /// A template of this name is already saved.
    #[error("template `{name}` already exists; pass --force to replace it")]
    TemplateExists {
        /// The name.
        name: String,
    },

    /// A template would be copied into a directory that holds files.
    #[error("{} is not empty; a project is only created in an empty directory", path.display())]
    DestinationNotEmpty {
        /// The directory.
        path: PathBuf,
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
