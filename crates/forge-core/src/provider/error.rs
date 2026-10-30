//! Errors a provider can return.

use thiserror::Error;

/// Why a completion failed.
///
/// Every variant holds plain data so the error is `Clone` and `PartialEq`,
/// which lets tests script and compare it.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ProviderError {
    /// The configured API key environment variable is unset or empty.
    #[error("API key environment variable `{env}` is not set")]
    MissingApiKey {
        /// Name of the environment variable.
        env: String,
    },

    /// The server answered with a non-success status.
    #[error("provider returned HTTP {status}: {body}")]
    Http {
        /// HTTP status code.
        status: u16,
        /// Response body, possibly truncated.
        body: String,
    },

    /// The request never produced a response, or the stream broke.
    #[error("transport error: {detail}")]
    Transport {
        /// What went wrong.
        detail: String,
    },

    /// Every retry failed.
    #[error("gave up after {attempts} attempts; last error: {last}")]
    RetriesExhausted {
        /// Number of attempts made, including the first.
        attempts: u32,
        /// The error from the final attempt.
        last: Box<ProviderError>,
    },

    /// The completion took longer than the configured timeout.
    #[error("request exceeded the {after_s}s timeout")]
    Timeout {
        /// The timeout that was exceeded, in seconds.
        after_s: u64,
    },

    /// The session has used its token budget.
    #[error(
        "session token budget exceeded: {used} of {limit} tokens used; raise [limits].session_tokens or start a new session"
    )]
    BudgetExceeded {
        /// Tokens used so far.
        used: u64,
        /// The budget.
        limit: u64,
    },

    /// The provider sent an error event inside an otherwise successful stream.
    #[error("provider returned an error event ({kind}): {message}")]
    Api {
        /// The provider's error type.
        kind: String,
        /// The provider's message.
        message: String,
    },

    /// The response could not be understood.
    #[error("malformed provider response: {detail}")]
    Malformed {
        /// What was wrong.
        detail: String,
    },
}

impl ProviderError {
    /// Builds a [`ProviderError::Malformed`].
    ///
    /// # Arguments
    ///
    /// * `detail` - What was wrong.
    ///
    /// # Returns
    ///
    /// The error.
    pub fn malformed(detail: impl Into<String>) -> Self {
        Self::Malformed {
            detail: detail.into(),
        }
    }

    /// Builds a [`ProviderError::Transport`].
    ///
    /// # Arguments
    ///
    /// * `detail` - What went wrong.
    ///
    /// # Returns
    ///
    /// The error.
    pub fn transport(detail: impl Into<String>) -> Self {
        Self::Transport {
            detail: detail.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_message_names_the_config_key() {
        let error = ProviderError::BudgetExceeded {
            used: 12,
            limit: 10,
        };
        let text = error.to_string();
        assert!(text.contains("12 of 10"));
        assert!(text.contains("[limits].session_tokens"));
    }

    #[test]
    fn retries_exhausted_shows_last_error() {
        let error = ProviderError::RetriesExhausted {
            attempts: 3,
            last: Box::new(ProviderError::Http {
                status: 503,
                body: "busy".to_owned(),
            }),
        };
        assert_eq!(
            error.to_string(),
            "gave up after 3 attempts; last error: provider returned HTTP 503: busy"
        );
    }
}
