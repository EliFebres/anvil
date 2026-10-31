//! What a reviewer receives and the trait every reviewer implements.

use std::future::Future;
use std::pin::Pin;

use anvil_types::{Stance, Ticket};

use crate::error::ReviewError;
use crate::verdict::{Decision, Finding, Review, ReviewerId, Usage};

/// The ticket as a reviewer sees it: what was asked and how to tell it is done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TicketSummary {
    /// The ticket or pull request title.
    pub title: String,
    /// Symptom, repro and spec for a ticket; the body for a pull request.
    pub description: String,
    /// The acceptance checks the change must satisfy.
    pub acceptance: Vec<String>,
}

impl From<&Ticket> for TicketSummary {
    fn from(ticket: &Ticket) -> Self {
        let mut description = format!("Symptom:\n{}", ticket.symptom.trim());
        if let Some(repro) = &ticket.repro {
            description.push_str(&format!("\n\nRepro:\n{}", repro.trim()));
        }
        description.push_str(&format!("\n\nSpec:\n{}", ticket.spec.trim()));
        Self {
            title: ticket.title.clone(),
            description,
            acceptance: ticket.acceptance.clone(),
        }
    }
}

impl TicketSummary {
    /// Builds the summary from a pull request title and body.
    ///
    /// The acceptance list is the body's checklist (`- [ ]` and `- [x]`
    /// lines); a body without one gets a single generic check.
    ///
    /// # Arguments
    ///
    /// * `title` - The pull request title.
    /// * `body` - The pull request body, possibly empty.
    ///
    /// # Returns
    ///
    /// The summary.
    pub fn from_pull(title: &str, body: &str) -> Self {
        let acceptance: Vec<String> = body
            .lines()
            .map(str::trim)
            .filter_map(checklist_item)
            .filter(|item| !item.is_empty())
            .map(str::to_owned)
            .collect();
        let acceptance = if acceptance.is_empty() {
            vec!["The change does what the pull request title and description say.".to_owned()]
        } else {
            acceptance
        };
        let body = body.trim();
        Self {
            title: title.trim().to_owned(),
            description: if body.is_empty() {
                "(no description)".to_owned()
            } else {
                body.to_owned()
            },
            acceptance,
        }
    }
}

/// The text after a `- [ ]` or `- [x]` marker, when the line is one.
fn checklist_item(line: &str) -> Option<&str> {
    let rest = line
        .strip_prefix("- ")
        .or_else(|| line.strip_prefix("* "))?;
    let rest = rest
        .strip_prefix("[ ]")
        .or_else(|| rest.strip_prefix("[x]"))
        .or_else(|| rest.strip_prefix("[X]"))?;
    Some(rest.trim())
}

/// Everything a reviewer receives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewRequest {
    /// What was asked.
    pub ticket: TicketSummary,
    /// The full unified diff.
    pub diff: String,
    /// The files the diff touches, repository-relative.
    pub changed_files: Vec<String>,
    /// The repository's `CLAUDE.md`, when it has one.
    pub guidelines: Option<String>,
}

/// A delivered review with what it cost.
#[derive(Debug, Clone, PartialEq)]
pub struct Reviewed {
    /// The review.
    pub review: Review,
    /// Tokens and cost, including any retry.
    pub usage: Usage,
}

/// The future a reviewer returns; boxed so reviewers can be trait objects.
pub type ReviewFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Reviewed, ReviewError>> + Send + 'a>>;

/// One reviewer of the swarm: a model with a stance, or a fake in tests.
pub trait Reviewer: Send + Sync {
    /// Who this reviewer is.
    fn id(&self) -> &ReviewerId;

    /// Reviews the request.
    ///
    /// # Arguments
    ///
    /// * `request` - The ticket, diff and changed files.
    ///
    /// # Returns
    ///
    /// The review and its cost, or why none could be delivered.
    fn review<'a>(&'a self, request: &'a ReviewRequest) -> ReviewFuture<'a>;
}

/// A reviewer that returns a scripted answer; for the factory's tests and this crate's.
#[derive(Debug, Clone)]
pub struct FakeReviewer {
    id: ReviewerId,
    response: Result<Review, String>,
}

impl FakeReviewer {
    /// A reviewer that approves with no findings.
    ///
    /// # Arguments
    ///
    /// * `provider` - The provider name to report.
    /// * `stance` - The stance to report.
    ///
    /// # Returns
    ///
    /// The fake.
    pub fn approving(provider: &str, stance: Stance) -> Self {
        Self::with_findings(provider, stance, Vec::new())
    }

    /// A reviewer that returns exactly these findings.
    ///
    /// Its own verdict is `changes` when any finding is worse than minor.
    ///
    /// # Arguments
    ///
    /// * `provider` - The provider name to report.
    /// * `stance` - The stance to report.
    /// * `findings` - What it finds.
    ///
    /// # Returns
    ///
    /// The fake.
    pub fn with_findings(provider: &str, stance: Stance, findings: Vec<Finding>) -> Self {
        let verdict = if findings
            .iter()
            .any(|f| f.severity > crate::verdict::Severity::Minor)
        {
            Decision::Changes
        } else {
            Decision::Approve
        };
        Self {
            id: Self::id_for(provider, stance),
            response: Ok(Review { verdict, findings }),
        }
    }

    /// A reviewer that fails with this message.
    ///
    /// # Arguments
    ///
    /// * `provider` - The provider name to report.
    /// * `stance` - The stance to report.
    /// * `message` - The error text.
    ///
    /// # Returns
    ///
    /// The fake.
    pub fn failing(provider: &str, stance: Stance, message: &str) -> Self {
        Self {
            id: Self::id_for(provider, stance),
            response: Err(message.to_owned()),
        }
    }

    fn id_for(provider: &str, stance: Stance) -> ReviewerId {
        ReviewerId {
            provider: provider.to_owned(),
            stance,
            model: format!("fake-{provider}"),
        }
    }
}

impl Reviewer for FakeReviewer {
    fn id(&self) -> &ReviewerId {
        &self.id
    }

    fn review<'a>(&'a self, _request: &'a ReviewRequest) -> ReviewFuture<'a> {
        Box::pin(async move {
            match &self.response {
                Ok(review) => Ok(Reviewed {
                    review: review.clone(),
                    usage: Usage {
                        input_tokens: 10,
                        output_tokens: 5,
                        cached_tokens: 0,
                        cost_usd: Some(0.0),
                    },
                }),
                Err(message) => Err(ReviewError::Scripted {
                    message: message.clone(),
                }),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pull_checklist_becomes_the_acceptance_list() {
        let body = "Fixes #3.\n\n- [ ] tests pass\n* [x] docs updated\n- not a check\n- [ ]   \n";
        let summary = TicketSummary::from_pull("  Add thing  ", body);
        assert_eq!(summary.title, "Add thing");
        assert_eq!(summary.acceptance, vec!["tests pass", "docs updated"]);
        assert_eq!(summary.description, body.trim());
    }

    #[test]
    fn pull_without_checklist_gets_a_generic_check_and_empty_body_is_named() {
        let summary = TicketSummary::from_pull("Title", "   ");
        assert_eq!(summary.acceptance.len(), 1);
        assert!(summary.acceptance[0].contains("title and description"));
        assert_eq!(summary.description, "(no description)");
    }

    #[test]
    fn ticket_summary_carries_symptom_repro_and_spec() {
        let ticket: Ticket = toml::from_str(
            "id = \"01J9Z6Q4K8XW2R3T5V7Y9B1D3F\"
created_at = \"2026-10-30T10:00:00Z\"
kind = \"bug\"
title = \"Title\"
symptom = \"It breaks.\"
repro = \"run x\"
spec = \"Do it right.\"
acceptance = [\"works\"]
priority = \"p1\"
status = \"open\"
origin = \"cli\"
",
        )
        .expect("parses");
        let summary = TicketSummary::from(&ticket);
        assert_eq!(summary.title, "Title");
        assert_eq!(
            summary.description,
            "Symptom:\nIt breaks.\n\nRepro:\nrun x\n\nSpec:\nDo it right."
        );
        assert_eq!(summary.acceptance, vec!["works"]);
    }
}
