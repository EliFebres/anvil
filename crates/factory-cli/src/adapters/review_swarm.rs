//! Gate C: the review swarm over the diff.

use factory_core::{BoxFuture, MinorFinding, ReviewInput, ReviewReport, ReviewSwarm};
use factory_review::{
    AttributedFinding, Decision, DiffLines, Outcome, ReviewRequest, Reviewer, Severity,
    TicketSummary, Verdict,
};

/// The swarm: every reviewer from `factory.toml`, or fakes in tests.
pub struct Swarm {
    reviewers: Vec<Box<dyn Reviewer>>,
}

impl Swarm {
    /// Builds the swarm.
    ///
    /// # Arguments
    ///
    /// * `reviewers` - The reviewers, in `[[reviewers]]` order.
    pub fn new(reviewers: Vec<Box<dyn Reviewer>>) -> Self {
        Self { reviewers }
    }

    /// How many reviewers run.
    pub fn len(&self) -> usize {
        self.reviewers.len()
    }

    /// Whether there are no reviewers.
    pub fn is_empty(&self) -> bool {
        self.reviewers.is_empty()
    }

    /// Turns the swarm's verdict into the report the worker reads.
    ///
    /// Minor findings keep their file and line only when the diff shows that
    /// line, so the worker can post them inline without GitHub refusing.
    ///
    /// # Arguments
    ///
    /// * `verdict` - The aggregate verdict.
    /// * `diff` - The diff that was reviewed.
    ///
    /// # Returns
    ///
    /// The report.
    pub fn report(verdict: &Verdict, diff: &str) -> ReviewReport {
        let lines = DiffLines::parse(diff);
        let minors = verdict
            .minors
            .iter()
            .map(|minor| {
                let anchored = matches!(
                    (&minor.finding.file, minor.finding.line),
                    (Some(file), Some(line)) if lines.contains(file, line)
                );
                MinorFinding {
                    file: minor.finding.file.clone(),
                    line: minor.finding.line,
                    anchored,
                    body: finding_text(minor),
                }
            })
            .collect();
        let verdicts = verdict
            .raw
            .iter()
            .map(|raw| {
                let word = match &raw.outcome {
                    Outcome::Reviewed { review } => review.verdict.as_str().to_owned(),
                    Outcome::Failed { .. } => "failed".to_owned(),
                };
                (raw.reviewer.to_string(), word)
            })
            .collect();
        let findings_markdown = verdict
            .findings
            .iter()
            .map(|finding| {
                format!(
                    "- `{}`: {}",
                    finding.finding.location(),
                    finding_text(finding)
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        ReviewReport {
            approved: verdict.decision == Decision::Approve,
            reason: verdict.reason.clone(),
            findings_markdown,
            verdicts,
            minors,
            cost_usd: verdict.usage().cost_usd,
        }
    }
}

impl ReviewSwarm for Swarm {
    fn review<'a>(
        &'a self,
        input: &'a ReviewInput,
    ) -> BoxFuture<'a, factory_core::Result<ReviewReport>> {
        Box::pin(async move {
            let request = ReviewRequest {
                ticket: TicketSummary::from(&input.ticket),
                diff: input.diff.clone(),
                changed_files: input.changed_files.clone(),
                guidelines: input.guidelines.clone(),
            };
            let verdict = factory_review::review(&request, &self.reviewers).await;
            Ok(Self::report(&verdict, &input.diff))
        })
    }
}

/// `**minor** (openai/style): message` plus the suggested fix when there is one.
///
/// # Arguments
///
/// * `finding` - The finding with its reviewer.
///
/// # Returns
///
/// Markdown.
pub fn finding_text(finding: &AttributedFinding) -> String {
    let mut text = format!(
        "**{}** ({}): {}",
        finding.finding.severity,
        finding.reviewer,
        finding.finding.message.trim()
    );
    if let Some(fix) = &finding.finding.suggested_fix
        && !fix.trim().is_empty()
    {
        text.push_str(&format!("\n\nSuggested fix: {}", fix.trim()));
    }
    text
}

/// Whether a finding blocks.
pub fn blocks(severity: Severity) -> bool {
    severity > Severity::Minor
}

#[cfg(test)]
mod tests {
    use super::*;
    use anvil_types::Stance;
    use factory_review::{Finding, RawVerdict, Review, ReviewerId, Usage};

    fn finding(
        severity: Severity,
        file: Option<&str>,
        line: Option<u32>,
        message: &str,
    ) -> Finding {
        Finding {
            severity,
            file: file.map(str::to_owned),
            line,
            message: message.to_owned(),
            suggested_fix: Some("do x".to_owned()),
        }
    }

    fn raw(stance: Stance, verdict: Decision, findings: Vec<Finding>) -> RawVerdict {
        RawVerdict {
            reviewer: ReviewerId {
                provider: "openai".to_owned(),
                stance,
                model: "m".to_owned(),
            },
            outcome: Outcome::Reviewed {
                review: Review { verdict, findings },
            },
            usage: Usage::default(),
        }
    }

    #[test]
    fn minors_are_anchored_only_on_lines_the_diff_shows() {
        let verdict = Verdict::aggregate(vec![raw(
            Stance::Style,
            Decision::Approve,
            vec![
                finding(Severity::Minor, Some("a.rs"), Some(2), "inline"),
                finding(Severity::Minor, Some("a.rs"), Some(40), "line not in diff"),
                finding(Severity::Minor, None, None, "no location"),
            ],
        )]);
        let diff = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1,2 @@\n x\n+y\n";
        let report = Swarm::report(&verdict, diff);
        assert!(report.approved);
        assert_eq!(report.minors.len(), 3);
        assert!(report.minors[0].anchored);
        assert!(!report.minors[1].anchored, "line 40 is not in the diff");
        assert_eq!(report.minors[1].location(), "a.rs:40");
        assert_eq!(report.minors[2].location(), "-");
        assert_eq!(
            report.minors[0].body,
            "**minor** (openai/style): inline\n\nSuggested fix: do x"
        );
        assert_eq!(
            report.verdicts,
            vec![("openai/style".to_owned(), "approve".to_owned())]
        );
        assert!(report.findings_markdown.is_empty());
    }

    #[test]
    fn blockers_make_a_changes_report_with_findings_markdown() {
        let verdict = Verdict::aggregate(vec![
            raw(
                Stance::Correctness,
                Decision::Changes,
                vec![finding(Severity::Blocker, Some("b.rs"), Some(3), "wrong")],
            ),
            raw(Stance::Safety, Decision::Approve, Vec::new()),
        ]);
        let report = Swarm::report(&verdict, "");
        assert!(!report.approved);
        assert_eq!(report.reason, "1 blocker");
        assert!(
            report
                .findings_markdown
                .contains("- `b.rs:3`: **blocker** (openai/correctness): wrong")
        );
        assert_eq!(report.verdicts.len(), 2);
        assert_eq!(report.verdicts[0].1, "changes");
        assert!(blocks(Severity::Major));
        assert!(!blocks(Severity::Minor));
    }
}
