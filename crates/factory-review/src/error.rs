//! The errors of the review swarm.

use std::path::PathBuf;

use thiserror::Error;

/// Why a reviewer could not be built or could not deliver a review.
#[derive(Debug, Error)]
pub enum ReviewError {
    /// A `[[reviewers]]` entry names a provider that is not in `[providers]`.
    #[error("reviewer provider `{provider}` is not in ~/.anvil/config.toml")]
    UnknownProvider {
        /// The provider name as written in `factory.toml`.
        provider: String,
    },

    /// The environment variable that holds a provider's key is not set.
    #[error("provider `{provider}` needs the API key in ${env}, which is not set")]
    MissingApiKey {
        /// The provider name.
        provider: String,
        /// The environment variable the provider names.
        env: String,
    },

    /// The HTTP client could not be built.
    #[error("building the HTTP client: {0}")]
    Client(#[source] reqwest::Error),

    /// The request never produced a response.
    #[error("provider `{provider}`: {source}")]
    Transport {
        /// The provider name.
        provider: String,
        /// The reqwest error.
        #[source]
        source: reqwest::Error,
    },

    /// The provider answered with an error status.
    #[error("provider `{provider}` returned HTTP {status}: {body}")]
    Http {
        /// The provider name.
        provider: String,
        /// The status code.
        status: u16,
        /// The first part of the body.
        body: String,
    },

    /// The response was not the JSON shape the protocol promises.
    #[error("provider `{provider}` response has an unexpected shape: {detail}")]
    Shape {
        /// The provider name.
        provider: String,
        /// What was missing or wrong.
        detail: String,
    },

    /// The model's text was not a review, even after the retry.
    #[error(
        "provider `{provider}` did not return a well-formed review in {attempts} attempts: {detail}"
    )]
    Malformed {
        /// The provider name.
        provider: String,
        /// How many replies were tried.
        attempts: u32,
        /// Why the last reply was rejected.
        detail: String,
    },

    /// A fake reviewer was told to fail; tests only.
    #[error("{message}")]
    Scripted {
        /// The scripted error text.
        message: String,
    },

    /// `rules.toml` exists but could not be read.
    #[error("reading {path}: {source}")]
    RulesIo {
        /// The file.
        path: PathBuf,
        /// The I/O error.
        #[source]
        source: std::io::Error,
    },

    /// `rules.toml` is not valid TOML of the expected shape.
    #[error("parsing {path}: {source}")]
    RulesParse {
        /// The file.
        path: PathBuf,
        /// The TOML error.
        #[source]
        source: toml::de::Error,
    },
}
