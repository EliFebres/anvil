//! Multi-model review for the factory.
//!
//! Every reviewer in `factory.toml` reads the ticket, the full diff and the
//! changed files with one stance (correctness, safety, style, simplicity)
//! and returns findings as JSON. [`review`] runs them concurrently and
//! aggregates: any blocker or two majors means `changes`; minors come back
//! separately so the caller can post them as comments rather than block.
//! Every raw verdict is kept so a wrong review can be traced to one model
//! and one stance.
//!
//! Owns its own model calls and depends on anvil-types only; never on
//! anvil-core or factory-core. The factory calls [`review`] with
//! [`ModelReviewer::from_config`] in production and [`FakeReviewer`]s in tests.

pub mod diff;
pub mod error;
pub mod model;
pub mod prompt;
pub mod reviewer;
pub mod rules;
pub mod verdict;

pub use diff::DiffLines;
pub use error::ReviewError;
pub use model::{ANTHROPIC_VERSION, MAX_ATTEMPTS, ModelReviewer};
pub use reviewer::{FakeReviewer, ReviewFuture, ReviewRequest, Reviewed, Reviewer, TicketSummary};
pub use rules::{Rule, load_rules};
pub use verdict::{
    AttributedFinding, Decision, Finding, Outcome, RawVerdict, Review, ReviewerId, Severity, Usage,
    Verdict, parse_review,
};

/// Runs every reviewer on the request at once and aggregates their verdicts.
///
/// # Arguments
///
/// * `request` - The ticket, diff and changed files.
/// * `reviewers` - The swarm, in the order `factory.toml` lists it.
///
/// # Returns
///
/// The verdict, with the raw result of every reviewer in the same order. A
/// reviewer that fails is recorded as failed and forces `changes`.
pub async fn review(request: &ReviewRequest, reviewers: &[Box<dyn Reviewer>]) -> Verdict {
    let runs = reviewers.iter().map(|reviewer| async move {
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
    });
    let raw = futures_util::future::join_all(runs).await;
    Verdict::aggregate(raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anvil_types::Stance;

    fn request() -> ReviewRequest {
        ReviewRequest {
            ticket: TicketSummary::from_pull("t", ""),
            diff: String::new(),
            changed_files: Vec::new(),
            guidelines: None,
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
    async fn two_majors_mean_changes() {
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
            ]),
        )
        .await;
        assert_eq!(verdict.decision, Decision::Changes);
        assert_eq!(verdict.reason, "2 majors");
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
    async fn a_failed_reviewer_means_changes_and_keeps_order() {
        let verdict = review(
            &request(),
            &swarm(vec![
                FakeReviewer::approving("a", Stance::Correctness),
                FakeReviewer::failing("b", Stance::Safety, "HTTP 500"),
                FakeReviewer::approving("c", Stance::Style),
            ]),
        )
        .await;
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
