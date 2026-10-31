//! Exponential backoff for the request phase of a completion.
//!
//! Nothing here touches the network; the caller supplies an `attempt` future
//! and reports whether its outcome is retryable.

use std::future::Future;
use std::time::Duration;

use forge_types::LimitsConfig;

use super::error::ProviderError;

/// How many times to retry and how long to wait between attempts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Retries after the first attempt. Zero means one attempt only.
    pub max_retries: u32,
    /// Delay before the first retry; doubles on each later retry.
    pub base_delay: Duration,
    /// Upper bound on any single delay.
    pub max_delay: Duration,
}

impl RetryPolicy {
    /// Builds the policy from config, with 500 ms base and 30 s cap.
    ///
    /// # Arguments
    ///
    /// * `limits` - The `[limits]` table.
    ///
    /// # Returns
    ///
    /// The policy.
    pub fn from_limits(limits: &LimitsConfig) -> Self {
        Self {
            max_retries: limits.max_retries,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(30),
        }
    }
}

/// The outcome of one attempt.
#[derive(Debug)]
pub enum Attempt<T> {
    /// The attempt succeeded.
    Ok(T),
    /// The attempt failed in a way that may succeed if repeated.
    Retry {
        /// The failure.
        error: ProviderError,
        /// A server-requested delay, if it sent one.
        retry_after: Option<Duration>,
    },
    /// The attempt failed and repeating it would not help.
    Fatal(ProviderError),
}

/// Whether an HTTP status is worth retrying.
///
/// # Arguments
///
/// * `status` - The HTTP status code.
///
/// # Returns
///
/// `true` for 408, 429 and every 5xx.
pub fn is_retryable_status(status: u16) -> bool {
    status == 408 || status == 429 || (500..=599).contains(&status)
}

/// Parses a `Retry-After` header value.
///
/// Only the integer-seconds form is understood; the HTTP-date form is
/// ignored so the policy's backoff applies instead.
///
/// # Arguments
///
/// * `header` - The header value, if present.
///
/// # Returns
///
/// The delay, when the value is a non-negative integer.
pub fn parse_retry_after(header: Option<&str>) -> Option<Duration> {
    header
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

/// Computes the delay before retry number `attempt` (zero-based).
///
/// # Arguments
///
/// * `policy` - The policy.
/// * `attempt` - Index of the attempt that just failed, starting at zero.
/// * `retry_after` - A server-requested delay, which wins when present.
///
/// # Returns
///
/// `retry_after`, or `base_delay * 2^attempt`, capped at `max_delay`.
pub fn backoff_delay(
    policy: &RetryPolicy,
    attempt: u32,
    retry_after: Option<Duration>,
) -> Duration {
    let exponential = policy
        .base_delay
        .saturating_mul(2u32.saturating_pow(attempt.min(31)));
    retry_after.unwrap_or(exponential).min(policy.max_delay)
}

/// Runs `attempt` until it succeeds, fails fatally, or the retries run out.
///
/// # Arguments
///
/// * `policy` - How many retries and how long to wait.
/// * `attempt` - Called with the zero-based attempt index; returns the outcome.
///
/// # Returns
///
/// The successful value.
///
/// # Errors
///
/// The [`Attempt::Fatal`] error as-is, or [`ProviderError::RetriesExhausted`]
/// wrapping the last retryable error once `max_retries` is spent.
pub async fn retry<T, F, Fut>(policy: &RetryPolicy, mut attempt: F) -> Result<T, ProviderError>
where
    F: FnMut(u32) -> Fut,
    Fut: Future<Output = Attempt<T>>,
{
    let mut index = 0;
    loop {
        match attempt(index).await {
            Attempt::Ok(value) => return Ok(value),
            Attempt::Fatal(error) => return Err(error),
            Attempt::Retry { error, retry_after } => {
                if index >= policy.max_retries {
                    return Err(ProviderError::RetriesExhausted {
                        attempts: index + 1,
                        last: Box::new(error),
                    });
                }
                let delay = backoff_delay(policy, index, retry_after);
                tracing::debug!(attempt = index, ?delay, %error, "retrying provider request");
                tokio::time::sleep(delay).await;
                index += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    fn policy(max_retries: u32) -> RetryPolicy {
        RetryPolicy {
            max_retries,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(30),
        }
    }

    fn http(status: u16) -> ProviderError {
        ProviderError::Http {
            status,
            body: String::new(),
        }
    }

    #[test]
    fn retryable_statuses() {
        for status in [408, 429, 500, 502, 503, 599] {
            assert!(is_retryable_status(status), "{status}");
        }
        for status in [200, 400, 401, 404, 422] {
            assert!(!is_retryable_status(status), "{status}");
        }
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let policy = policy(10);
        let seconds: Vec<f64> = (0..7)
            .map(|attempt| backoff_delay(&policy, attempt, None).as_secs_f64())
            .collect();
        assert_eq!(seconds, vec![0.5, 1.0, 2.0, 4.0, 8.0, 16.0, 30.0]);
        assert_eq!(
            backoff_delay(&policy, 40, None),
            Duration::from_secs(30),
            "huge attempt counts must not overflow"
        );
    }

    #[test]
    fn retry_after_overrides_backoff() {
        let policy = policy(10);
        assert_eq!(
            backoff_delay(&policy, 0, Some(Duration::from_secs(7))),
            Duration::from_secs(7)
        );
        assert_eq!(
            backoff_delay(&policy, 0, Some(Duration::from_secs(90))),
            Duration::from_secs(30),
            "the cap still applies"
        );
    }

    #[test]
    fn parse_retry_after_seconds_only() {
        assert_eq!(parse_retry_after(Some("3")), Some(Duration::from_secs(3)));
        assert_eq!(
            parse_retry_after(Some(" 12 ")),
            Some(Duration::from_secs(12))
        );
        assert_eq!(
            parse_retry_after(Some("Wed, 21 Oct 2015 07:28:00 GMT")),
            None
        );
        assert_eq!(parse_retry_after(None), None);
    }

    #[tokio::test(start_paused = true)]
    async fn retry_succeeds_after_two_retries() {
        let calls = Arc::new(AtomicU32::new(0));
        let seen = Arc::clone(&calls);
        let result = retry(&policy(4), |index| {
            let seen = Arc::clone(&seen);
            async move {
                seen.fetch_add(1, Ordering::SeqCst);
                if index < 2 {
                    Attempt::Retry {
                        error: http(503),
                        retry_after: None,
                    }
                } else {
                    Attempt::Ok("done")
                }
            }
        })
        .await;
        assert_eq!(result, Ok("done"));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn retry_exhausts_into_retries_exhausted() {
        let result: Result<(), _> = retry(&policy(2), |_| async {
            Attempt::Retry {
                error: http(503),
                retry_after: None,
            }
        })
        .await;
        assert_eq!(
            result,
            Err(ProviderError::RetriesExhausted {
                attempts: 3,
                last: Box::new(http(503)),
            })
        );
    }

    #[tokio::test(start_paused = true)]
    async fn retry_fatal_stops_immediately() {
        let calls = Arc::new(AtomicU32::new(0));
        let seen = Arc::clone(&calls);
        let started = tokio::time::Instant::now();
        let result: Result<(), _> = retry(&policy(4), |_| {
            let seen = Arc::clone(&seen);
            async move {
                seen.fetch_add(1, Ordering::SeqCst);
                Attempt::Fatal(http(400))
            }
        })
        .await;
        assert_eq!(result, Err(http(400)));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            started.elapsed(),
            Duration::ZERO,
            "no sleep on a fatal error"
        );
    }
}
