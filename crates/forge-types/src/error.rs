//! The error type returned by every validator in this crate.

use thiserror::Error;

/// Why a shared type failed validation.
///
/// Each variant names the offending field or entry, so the message stands on
/// its own in a log line or a CLI error.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ValidationError {
    /// A ticket title exceeds the character limit.
    #[error("ticket title is {len} characters; the limit is {max}")]
    TitleTooLong {
        /// Length of the offending title in characters.
        len: usize,
        /// The limit that was exceeded.
        max: usize,
    },

    /// A required text field is empty or only whitespace.
    #[error("field `{field}` must not be empty")]
    EmptyField {
        /// Name of the empty field.
        field: &'static str,
    },

    /// A ticket has no acceptance checks.
    #[error("ticket needs at least one acceptance check")]
    EmptyAcceptance,

    /// A ticket text field references a path outside the monorepo.
    #[error("field `{field}` references a path outside the monorepo: `{path}`")]
    OutsidePath {
        /// Name of the field that holds the path.
        field: &'static str,
        /// The offending token as written.
        path: String,
    },

    /// A ticket text field contains more code than the boundary allows.
    #[error("field `{field}` contains {lines} lines of code; the limit is {max}")]
    TooMuchCode {
        /// Name of the field that holds the code.
        field: &'static str,
        /// Number of code lines counted.
        lines: usize,
        /// The limit that was exceeded.
        max: usize,
    },

    /// A role names a provider that is not declared in `[providers]`.
    #[error("role `{role}` names unknown provider `{provider}`")]
    UnknownProvider {
        /// The role being assigned.
        role: &'static str,
        /// The provider name that has no entry.
        provider: String,
    },

    /// A provider base URL is not an http or https URL.
    #[error("provider `{name}` has base_url `{base_url}`; it must start with http:// or https://")]
    InvalidBaseUrl {
        /// The provider entry name.
        name: String,
        /// The URL as written.
        base_url: String,
    },

    /// A provider declares a context window of zero tokens.
    #[error("provider `{name}` has context_window 0; it must be positive")]
    ZeroContextWindow {
        /// The provider entry name.
        name: String,
    },

    /// A provider has an empty required string field.
    #[error("provider `{name}` field `{field}` must not be empty")]
    EmptyProviderField {
        /// The provider entry name.
        name: String,
        /// Name of the empty field.
        field: &'static str,
    },

    /// A skill name is not kebab-case or is too long.
    #[error(
        "skill name `{name}` must be kebab-case (a-z, 0-9, single hyphens) and at most {max} characters"
    )]
    InvalidSkillName {
        /// The name as written.
        name: String,
        /// The length limit in characters.
        max: usize,
    },

    /// A skill description exceeds the character limit.
    #[error("skill description is {len} characters; the limit is {max}")]
    DescriptionTooLong {
        /// Length of the description in characters.
        len: usize,
        /// The limit that was exceeded.
        max: usize,
    },
}
