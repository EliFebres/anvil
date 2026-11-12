//! Multi-model review for the factory.
//!
//! Every reviewer in `factory.toml` reads the ticket, the full diff and the
//! changed files with one stance (correctness, safety, style, simplicity)
//! and returns findings as JSON. [`review`] runs them concurrently and
//! aggregates: any blocker or two majors from a blocking reviewer means
//! `changes`. Minors, and every finding of a non-blocking reviewer, come back
//! separately so the caller can post them as comments rather than block.
//! A reviewer that fails is rerun alone, up to [`RERUN_PASSES`] times.
//! Every raw verdict is kept so a wrong review can be traced to one model
//! and one stance.
//!
//! Owns its own model calls and depends on anvil-types only; never on
//! anvil-core or factory-core. The factory calls [`review`] with
//! [`ModelReviewer::from_config`] in production and [`FakeReviewer`]s in tests.

use std::time::Duration;

pub mod diff;
pub mod error;
pub mod model;
pub mod prompt;
pub mod reviewer;
pub mod rules;
pub mod verdict;

pub use diff::DiffLines;
pub use error::ReviewError;
pub use model::{ANTHROPIC_VERSION, MAX_ATTEMPTS, ModelReviewer, ping};
pub use reviewer::{FakeReviewer, ReviewFuture, ReviewRequest, Reviewed, Reviewer, TicketSummary};
pub use rules::{Rule, load_rules};
pub use verdict::{
    AttributedFinding, Decision, Finding, Outcome, RawVerdict, Review, ReviewerId, Severity, Usage,
    Verdict, parse_review,
};

/// How many times a failed reviewer is rerun after the first pass.
pub const RERUN_PASSES: u32 = 2;

/// How long to wait before each rerun pass.
pub const RERUN_DELAY: Duration = Duration::from_secs(30);

/// Runs every reviewer on the request at once and aggregates their verdicts.
///
/// Reviewers that fail are rerun alone, up to [`RERUN_PASSES`] more passes,
/// each after [`RERUN_DELAY`]; delivered reviews are kept.
///
/// # Arguments
///
/// * `request` - The ticket, diff and changed files.
/// * `reviewers` - The swarm, in the order `factory.toml` lists it.
///
/// # Returns
///
/// The verdict, with the raw result of every reviewer in the same order. A
/// reviewer that still fails is recorded as failed, listed in
/// [`Verdict::failed`] and forces `changes`.
pub async fn review(request: &ReviewRequest, reviewers: &[Box<dyn Reviewer>]) -> Verdict {
    review_with_delay(request, reviewers, RERUN_DELAY).await
}

/// [`review`] with the pause before each rerun pass supplied, so tests need not wait.
async fn review_with_delay(
    request: &ReviewRequest,
    reviewers: &[Box<dyn Reviewer>],
    rerun_delay: Duration,
) -> Verdict {
    let mut raw = futures_util::future::join_all(
        reviewers
            .iter()
            .map(|reviewer| run_once(request, reviewer.as_ref())),
    )
    .await;
    for pass in 1..=RERUN_PASSES {
        let failed: Vec<usize> = (0..raw.len())
            .filter(|&i| matches!(raw[i].outcome, Outcome::Failed { .. }))
            .collect();
        if failed.is_empty() {
            break;
        }
        tracing::warn!(pass, reviewers = failed.len(), "rerunning failed reviewers");
        tokio::time::sleep(rerun_delay).await;
        let reruns = futures_util::future::join_all(
            failed
                .iter()
                .map(|&i| run_once(request, reviewers[i].as_ref())),
        )
        .await;
        for (&i, mut rerun) in failed.iter().zip(reruns) {
            let mut usage = raw[i].usage;
            usage.add(rerun.usage);
            rerun.usage = usage;
            raw[i] = rerun;
        }
    }
    Verdict::aggregate(raw)
}

/// Asks one reviewer once and records what came back.
async fn run_once(request: &ReviewRequest, reviewer: &dyn Reviewer) -> RawVerdict {
    let id = reviewer.id().clone();
    match reviewer.review(request).await {
        Ok(reviewed) => {
            tracing::info!(reviewer = %id, verdict = %reviewed.review.verdict, findings = reviewed.review.findings.len(), "review delivered");
            RawVerdict {
                reviewer: id,
                outcome: Outcome::Reviewed {
                    review: reviewed.review,
                },
                usage: reviewed.usage,
            }
        }
        Err(error) => {
            tracing::warn!(reviewer = %id, error = %error, "review failed");
            RawVerdict {
                reviewer: id,
                outcome: Outcome::Failed {
                    error: error.to_string(),
                },
                usage: Usage::default(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anvil_types::Stance;
    use std::sync::atomic::Ordering;

    fn request() -> ReviewRequest {
        ReviewRequest {
            ticket: TicketSummary::from_pull("t", ""),
            diff: String::new(),
            changed_files: Vec::new(),
            guidelines: None,
            earlier_findings: Vec::new(),
        }
    }

    fn finding(severity: Severity, message: &str) -> Finding {
        Finding {
            severity,
            file: Some("a.rs".to_owned()),
            line: Some(1),
            message: message.to_owned(),
            suggested_fix: None,
        }
    }

    fn swarm(reviewers: Vec<FakeReviewer>) -> Vec<Box<dyn Reviewer>> {
        reviewers
            .into_iter()
            .map(|r| Box::new(r) as Box<dyn Reviewer>)
            .collect()
    }

    #[tokio::test]
    async fn a_blocker_means_changes() {
        let verdict = review(
            &request(),
            &swarm(vec![
                FakeReviewer::with_findings(
                    "a",
                    Stance::Correctness,
                    vec![finding(Severity::Blocker, "wrong")],
                ),
                FakeReviewer::approving("b", Stance::Safety),
            ]),
        )
        .await;
        assert_eq!(verdict.decision, Decision::Changes);
        assert_eq!(verdict.findings.len(), 1);
        assert_eq!(verdict.raw.len(), 2);
    }

    #[tokio::test]
    async fn two_majors_from_blocking_reviewers_mean_changes() {
        let verdict = review(
            &request(),
            &swarm(vec![
                FakeReviewer::with_findings(
                    "a",
                    Stance::Correctness,
                    vec![finding(Severity::Major, "one")],
                ),
                FakeReviewer::with_findings(
                    "b",
                    Stance::Safety,
                    vec![finding(Severity::Major, "two")],
                ),
            ]),
        )
        .await;
        assert_eq!(verdict.decision, Decision::Changes);
        assert_eq!(verdict.reason, "2 majors");
    }

    #[tokio::test]
    async fn a_non_blocking_major_and_blocker_only_comment() {
        let verdict = review(
            &request(),
            &swarm(vec![
                FakeReviewer::with_findings(
                    "a",
                    Stance::Correctness,
                    vec![finding(Severity::Major, "one")],
                ),
                FakeReviewer::with_findings(
                    "b",
                    Stance::Simplicity,
                    vec![finding(Severity::Major, "two")],
                ),
                FakeReviewer::with_findings(
                    "c",
                    Stance::Style,
                    vec![finding(Severity::Blocker, "rule x")],
                ),
            ]),
        )
        .await;
        assert_eq!(verdict.decision, Decision::Approve);
        assert_eq!(verdict.findings.len(), 1);
        assert_eq!(verdict.minors.len(), 2);
    }

    #[tokio::test]
    async fn a_style_reviewer_made_blocking_can_block() {
        let verdict = review(
            &request(),
            &swarm(vec![
                FakeReviewer::with_findings(
                    "a",
                    Stance::Style,
                    vec![finding(Severity::Blocker, "rule x")],
                )
                .blocking(true),
            ]),
        )
        .await;
        assert_eq!(verdict.decision, Decision::Changes);
        assert_eq!(verdict.reason, "1 blocker");
    }

    #[tokio::test]
    async fn one_major_plus_minors_approves_with_minors_aside() {
        let verdict = review(
            &request(),
            &swarm(vec![
                FakeReviewer::with_findings(
                    "a",
                    Stance::Correctness,
                    vec![
                        finding(Severity::Major, "one"),
                        finding(Severity::Minor, "nit"),
                    ],
                ),
                FakeReviewer::with_findings(
                    "b",
                    Stance::Style,
                    vec![finding(Severity::Minor, "rule x")],
                ),
            ]),
        )
        .await;
        assert_eq!(verdict.decision, Decision::Approve);
        assert_eq!(verdict.findings.len(), 1);
        assert_eq!(verdict.minors.len(), 2);
    }

    #[tokio::test]
    async fn a_reviewer_that_fails_once_is_rerun_alone() {
        let flaky = FakeReviewer::failing_then_approving("b", Stance::Safety, 1);
        let flaky_calls = flaky.call_counter();
        let a = FakeReviewer::approving("a", Stance::Correctness);
        let c = FakeReviewer::approving("c", Stance::Style);
        let other_calls = [a.call_counter(), c.call_counter()];
        let verdict =
            review_with_delay(&request(), &swarm(vec![a, flaky, c]), Duration::ZERO).await;
        assert_eq!(verdict.decision, Decision::Approve);
        assert!(verdict.failed.is_empty());
        assert_eq!(flaky_calls.load(Ordering::SeqCst), 2);
        for calls in other_calls {
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        }
    }

    #[tokio::test]
    async fn a_reviewer_that_always_fails_is_called_three_times_and_listed() {
        let broken = FakeReviewer::failing("b", Stance::Safety, "HTTP 500");
        let calls = broken.call_counter();
        let verdict = review_with_delay(
            &request(),
            &swarm(vec![
                FakeReviewer::approving("a", Stance::Correctness),
                broken,
                FakeReviewer::approving("c", Stance::Style),
            ]),
            Duration::ZERO,
        )
        .await;
        assert_eq!(calls.load(Ordering::SeqCst), 1 + RERUN_PASSES as usize);
        assert_eq!(verdict.failed, vec!["b/safety"]);
        assert_eq!(verdict.decision, Decision::Changes);
        assert_eq!(verdict.reason, "reviewer(s) failed: b/safety");
        let order: Vec<String> = verdict.raw.iter().map(|r| r.reviewer.to_string()).collect();
        assert_eq!(order, vec!["a/correctness", "b/safety", "c/style"]);
        assert!(
            matches!(&verdict.raw[1].outcome, Outcome::Failed { error } if error == "HTTP 500")
        );
    }

    #[tokio::test]
    async fn no_reviewers_means_changes() {
        let verdict = review(&request(), &[]).await;
        assert_eq!(verdict.decision, Decision::Changes);
        assert_eq!(verdict.reason, "no reviewers ran");
    }
}
