//! Why `factory.toml` failed validation.

use thiserror::Error;

/// Why `factory.toml` failed validation.
///
/// Each variant names the offending field or entry, so the message stands on
/// its own in a log line or a CLI error.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ValidationError {
    /// A required text field is empty or only whitespace.
    #[error("field `{field}` must not be empty")]
    EmptyField {
        /// Name of the empty field.
        field: &'static str,
    },

    /// A role names a provider that is not declared in `[providers]`.
    #[error("role `{role}` names unknown provider `{provider}`")]
    UnknownProvider {
        /// The role being assigned.
        role: &'static str,
        /// The provider name that has no entry.
        provider: String,
    },

    /// A `github.repo` value is not `owner/repo`.
    #[error("github.repo `{repo}` must be `owner/repo`")]
    InvalidRepo {
        /// The value as written.
        repo: String,
    },

    /// A reviewer runs the same model as the coder, so a model would review its own work.
    #[error(
        "reviewer `{reviewer}` runs {model}, the same as coder `{coder}`; a reviewer needs a different model"
    )]
    ReviewerIsCoderModel {
        /// The reviewer's provider name.
        reviewer: String,
        /// The coder's provider name.
        coder: String,
        /// The shared model, as `model at base_url`.
        model: String,
    },

    /// The blocking reviewers do not span two models, so one model's blind spots decide every review.
    #[error("blocking reviewers use {found} distinct model(s); at least 2 are needed")]
    TooFewReviewerModels {
        /// How many distinct (base URL, model) pairs the blocking reviewers use.
        found: usize,
    },

    /// Every reviewer is non-blocking, so no review could ever send a change back.
    #[error("at least one reviewer must be blocking")]
    NoBlockingReviewer,

    /// A numeric field that must be positive is zero.
    #[error("field `{field}` must be at least 1")]
    ZeroField {
        /// Name of the field.
        field: &'static str,
    },

    /// A glob in a list of rules does not compile.
    #[error("field `{field}` has an invalid glob `{glob}`: {detail}")]
    InvalidGlob {
        /// Name of the field holding the list.
        field: &'static str,
        /// The glob as written.
        glob: String,
        /// Why it does not compile.
        detail: String,
    },

    /// A time of day is not `HH:MM`.
    #[error("field `{field}` must be a time of day as HH:MM, not `{value}`")]
    InvalidTime {
        /// Name of the field.
        field: &'static str,
        /// The value as written.
        value: String,
    },
}
