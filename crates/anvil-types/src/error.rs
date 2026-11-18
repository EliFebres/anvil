//! The error types of this crate: validation, config loading and `.env` loading.

use std::path::PathBuf;

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

    /// A session `status.json` breaks a rule of the format.
    #[error("session status: {reason}")]
    Status {
        /// What is wrong, naming the field.
        reason: String,
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

    /// A ticket text field references a file path outside the project.
    #[error("field `{field}` references a path outside the project: `{path}`")]
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

    /// A provider price is negative.
    #[error("provider `{name}` has a negative price_per_mtoken")]
    NegativePrice {
        /// The provider entry name.
        name: String,
    },

    /// A provider base URL is not an http or https URL.
    #[error("provider `{name}` has base_url `{base_url}`; it must start with http:// or https://")]
    InvalidBaseUrl {
        /// The provider entry name.
        name: String,
        /// The URL as written.
        base_url: String,
    },

    /// A fake provider lacks a `script`, or a real one has one.
    #[error(
        "provider `{name}`: `script` is required with protocol = \"fake\" and allowed with no other protocol"
    )]
    FakeScript {
        /// The provider entry name.
        name: String,
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

    /// A ticket in some status lacks a field that status implies.
    #[error("a ticket with status `{status:?}` needs `{field}`")]
    MissingForStatus {
        /// Name of the missing field.
        field: &'static str,
        /// The status that requires it.
        status: crate::ticket::TicketStatus,
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

/// Why the anvil configuration could not be loaded.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// The user has no `~/.anvil/config.toml`.
    #[error(
        "no configuration found in ~/.anvil/config.toml; run `anvil setup` (with the standard providers) or `anvil config init --global` (a template)"
    )]
    NotFound,

    /// A file exists but cannot be read.
    #[error("reading {path}: {source}")]
    Read {
        /// The file.
        path: PathBuf,
        /// The I/O error.
        #[source]
        source: std::io::Error,
    },

    /// A repository's own rules are malformed.
    #[error("{path}: {detail}")]
    Project {
        /// The repository file.
        path: PathBuf,
        /// What is wrong.
        detail: String,
    },

    /// A configuration file is not valid TOML.
    #[error("parsing {path}: {source}")]
    Parse {
        /// The file that failed to parse.
        path: PathBuf,
        /// The TOML error.
        #[source]
        source: toml::de::Error,
    },

    /// The merged TOML does not have the shape of [`crate::AnvilConfig`].
    #[error("reading {sources}: {source}")]
    Shape {
        /// The files that contributed, joined with ` + `.
        sources: String,
        /// The TOML error.
        #[source]
        source: toml::de::Error,
    },

    /// A file still sets a section that anvil no longer has.
    #[error("{path}: the [{section}] section was removed: {reason}")]
    RemovedSection {
        /// The file that sets it.
        path: PathBuf,
        /// The section name.
        section: &'static str,
        /// What replaced it, in one sentence.
        reason: &'static str,
    },

    /// The profile named on the command line has no file.
    #[error("profile not found: {path} does not exist")]
    ProfileNotFound {
        /// Where the profile was looked for.
        path: PathBuf,
    },

    /// The merged configuration failed validation.
    #[error("validating {sources}: {source}")]
    Invalid {
        /// The files that contributed, joined with ` + `.
        sources: String,
        /// The validation error.
        #[source]
        source: ValidationError,
    },
}

/// Why a `.env` file could not be loaded.
#[derive(Debug, Error)]
pub enum DotenvError {
    /// The file exists but could not be read or parsed.
    #[error("loading {path}: {source}")]
    Load {
        /// The offending file.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: dotenvy::Error,
    },
}
