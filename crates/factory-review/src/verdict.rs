//! Reviews, findings and the aggregate verdict.
//!
//! A [`Review`] is what one model wrote. A [`Verdict`] is what the swarm
//! decided, with every raw review kept so a wrong call can be traced to one
//! model and one stance.

use std::fmt;

use anvil_types::Stance;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

/// What a review asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    /// Merge as it is.
    Approve,
    /// Send it back.
    Changes,
}

impl Decision {
    /// The decision as the JSON contract spells it.
    ///
    /// # Returns
    ///
    /// `approve` or `changes`.
    pub fn as_str(self) -> &'static str {
        match self {
            Decision::Approve => "approve",
            Decision::Changes => "changes",
        }
    }
}

impl fmt::Display for Decision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How bad a finding is; `Blocker` is the greatest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Worth a comment, not worth blocking.
    Minor,
    /// Should be fixed before merging; two of these block.
    Major,
    /// Must not merge as it is.
    Blocker,
}

impl Severity {
    /// The severity as the JSON contract spells it.
    ///
    /// # Returns
    ///
    /// `minor`, `major` or `blocker`.
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Minor => "minor",
            Severity::Major => "major",
            Severity::Blocker => "blocker",
        }
    }
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One thing a reviewer found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// How bad it is.
    pub severity: Severity,
    /// Repository-relative path, when the finding has one.
    #[serde(default)]
    pub file: Option<String>,
    /// Line in the new version of the file, when the finding has one.
    #[serde(default, deserialize_with = "lenient_line")]
    pub line: Option<u32>,
    /// What is wrong and why it matters.
    pub message: String,
    /// The change the reviewer would make.
    #[serde(default)]
    pub suggested_fix: Option<String>,
}

impl Finding {
    /// `path:line`, `path` or `-`, for one-line listings.
    ///
    /// # Returns
    ///
    /// The location text.
    pub fn location(&self) -> String {
        match (&self.file, self.line) {
            (Some(file), Some(line)) => format!("{file}:{line}"),
            (Some(file), None) => file.clone(),
            (None, _) => "-".to_owned(),
        }
    }
}

/// Accepts `12`, `"12"` and `null` for a line number; models mix them.
fn lenient_line<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<u32>, D::Error> {
    let value = Option::<Value>::deserialize(deserializer)?;
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => Ok(number.as_u64().and_then(|n| u32::try_from(n).ok())),
        Some(Value::String(text)) => Ok(text.trim().parse().ok()),
        Some(other) => Err(serde::de::Error::custom(format!(
            "line must be a number or null, not {other}"
        ))),
    }
}

/// What one model wrote, as the JSON contract defines it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Review {
    /// The model's own call; the aggregate recomputes it from the findings.
    pub verdict: Decision,
    /// Everything the model found.
    #[serde(default)]
    pub findings: Vec<Finding>,
}

/// Which model reviewed with which stance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewerId {
    /// The provider name from `factory.toml`.
    pub provider: String,
    /// The stance from `factory.toml`.
    pub stance: Stance,
    /// The model the provider is configured with.
    pub model: String,
}

impl fmt::Display for ReviewerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.provider, self.stance)
    }
}

/// What one review cost.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    /// Uncached input tokens.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Input tokens served from the provider's cache.
    pub cached_tokens: u64,
    /// Cost in USD when the provider has a price.
    pub cost_usd: Option<f64>,
}

impl Usage {
    /// Adds another call's usage to this one; the cost stays unknown once any part is.
    ///
    /// # Arguments
    ///
    /// * `other` - The usage to add.
    pub fn add(&mut self, other: Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cached_tokens += other.cached_tokens;
        self.cost_usd = match (self.cost_usd, other.cost_usd) {
            (Some(a), Some(b)) => Some(a + b),
            _ => None,
        };
    }
}

/// What came back from one reviewer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Outcome {
    /// The model delivered a well-formed review.
    Reviewed {
        /// The review.
        review: Review,
    },
    /// The model could not be reached or never returned a review.
    Failed {
        /// The error text.
        error: String,
    },
}

/// One reviewer's result, kept verbatim for tracing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RawVerdict {
    /// Who reviewed.
    pub reviewer: ReviewerId,
    /// What they returned.
    pub outcome: Outcome,
    /// What it cost, including a retry.
    pub usage: Usage,
}

/// A finding with the reviewer it came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttributedFinding {
    /// Who found it.
    pub reviewer: ReviewerId,
    /// The finding.
    pub finding: Finding,
}

/// The swarm's decision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Verdict {
    /// The aggregate decision.
    pub decision: Decision,
    /// Why, in a few words: `1 blocker`, `2 majors`, `a reviewer failed`.
    pub reason: String,
    /// The blocker and major findings, worst first, in reviewer order within a severity.
    pub findings: Vec<AttributedFinding>,
    /// The minor findings, for the caller to post as comments rather than block on.
    pub minors: Vec<AttributedFinding>,
    /// Every reviewer's raw result, in `[[reviewers]]` order.
    pub raw: Vec<RawVerdict>,
}

impl Verdict {
    /// Applies the aggregation rule to the raw reviews.
    ///
    /// Any blocker is `changes`. Two or more majors is `changes`. A reviewer
    /// that failed is `changes`, because a review nobody delivered must not
    /// count as approval. Otherwise `approve`, with the minors set aside. The
    /// models' own `verdict` fields are recorded but not trusted.
    ///
    /// # Arguments
    ///
    /// * `raw` - One result per reviewer.
    ///
    /// # Returns
    ///
    /// The verdict.
    pub fn aggregate(raw: Vec<RawVerdict>) -> Self {
        let mut findings = Vec::new();
        let mut minors = Vec::new();
        let mut failed = Vec::new();
        for result in &raw {
            match &result.outcome {
                Outcome::Reviewed { review } => {
                    for finding in &review.findings {
                        let attributed = AttributedFinding {
                            reviewer: result.reviewer.clone(),
                            finding: finding.clone(),
                        };
                        if finding.severity == Severity::Minor {
                            minors.push(attributed);
                        } else {
                            findings.push(attributed);
                        }
                    }
                }
                Outcome::Failed { .. } => failed.push(result.reviewer.to_string()),
            }
        }
        findings.sort_by_key(|f| std::cmp::Reverse(f.finding.severity));
        let blockers = count(&findings, Severity::Blocker);
        let majors = count(&findings, Severity::Major);
        let (decision, reason) = if raw.is_empty() {
            (Decision::Changes, "no reviewers ran".to_owned())
        } else if !failed.is_empty() {
            (
                Decision::Changes,
                format!("reviewer(s) failed: {}", failed.join(", ")),
            )
        } else if blockers > 0 {
            (Decision::Changes, plural(blockers, "blocker"))
        } else if majors >= 2 {
            (Decision::Changes, plural(majors, "major"))
        } else if majors == 1 {
            (
                Decision::Approve,
                "1 major, under the limit of 2".to_owned(),
            )
        } else {
            (Decision::Approve, "no blocking findings".to_owned())
        };
        Self {
            decision,
            reason,
            findings,
            minors,
            raw,
        }
    }

    /// The usage of every reviewer added up.
    ///
    /// # Returns
    ///
    /// The sum; the cost is unknown when any reviewer's is.
    pub fn usage(&self) -> Usage {
        let mut total = Usage {
            cost_usd: Some(0.0),
            ..Usage::default()
        };
        for result in &self.raw {
            total.add(result.usage);
        }
        total
    }
}

/// How many findings have exactly `severity`.
fn count(findings: &[AttributedFinding], severity: Severity) -> usize {
    findings
        .iter()
        .filter(|f| f.finding.severity == severity)
        .count()
}

/// `1 blocker`, `2 majors`.
fn plural(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "verdict: {} ({})", self.decision, self.reason)?;
        if !self.findings.is_empty() {
            writeln!(f, "findings:")?;
            for finding in &self.findings {
                write_finding(f, finding)?;
            }
        }
        if !self.minors.is_empty() {
            writeln!(f, "minor findings:")?;
            for finding in &self.minors {
                write_finding(f, finding)?;
            }
        }
        writeln!(f, "reviewers:")?;
        for raw in &self.raw {
            let tokens = format!(
                "{} in / {} out",
                thousands(raw.usage.input_tokens + raw.usage.cached_tokens),
                thousands(raw.usage.output_tokens)
            );
            let cost = raw
                .usage
                .cost_usd
                .map_or_else(|| "cost n/a".to_owned(), |c| format!("${c:.4}"));
            match &raw.outcome {
                Outcome::Reviewed { review } => writeln!(
                    f,
                    "  {:<24} {:<16} {:<8} {} finding(s)  {tokens}  {cost}",
                    raw.reviewer.to_string(),
                    raw.reviewer.model,
                    review.verdict,
                    review.findings.len()
                )?,
                Outcome::Failed { error } => writeln!(
                    f,
                    "  {:<24} {:<16} failed: {error}  {tokens}  {cost}",
                    raw.reviewer.to_string(),
                    raw.reviewer.model
                )?,
            }
        }
        Ok(())
    }
}

/// One finding as two lines: the finding, then its fix when there is one.
fn write_finding(f: &mut fmt::Formatter<'_>, finding: &AttributedFinding) -> fmt::Result {
    let inner = &finding.finding;
    writeln!(
        f,
        "  {:<8} {:<24} {}  {}",
        inner.severity,
        finding.reviewer.to_string(),
        inner.location(),
        inner.message.trim()
    )?;
    if let Some(fix) = &inner.suggested_fix
        && !fix.trim().is_empty()
    {
        writeln!(f, "           fix: {}", fix.trim())?;
    }
    Ok(())
}

/// `1.2k` above a thousand, the number below.
fn thousands(n: u64) -> String {
    if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

/// Reads a model's reply as a [`Review`].
///
/// Code fences are stripped and the text is narrowed to its outer braces,
/// since models wrap JSON in prose despite being told not to.
///
/// # Arguments
///
/// * `text` - The reply.
///
/// # Returns
///
/// The review.
///
/// # Errors
///
/// A description of what was wrong, meant to go back to the model.
pub fn parse_review(text: &str) -> Result<Review, String> {
    let (Some(start), Some(end)) = (text.find('{'), text.rfind('}')) else {
        return Err("the reply contains no JSON object".to_owned());
    };
    if end < start {
        return Err("the reply contains no JSON object".to_owned());
    }
    serde_json::from_str(&text[start..=end])
        .map_err(|error| format!("invalid review JSON: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(provider: &str, stance: Stance) -> ReviewerId {
        ReviewerId {
            provider: provider.to_owned(),
            stance,
            model: format!("{provider}-model"),
        }
    }

    fn finding(severity: Severity, file: &str, line: u32, message: &str) -> Finding {
        Finding {
            severity,
            file: Some(file.to_owned()),
            line: Some(line),
            message: message.to_owned(),
            suggested_fix: None,
        }
    }

    fn reviewed(reviewer: ReviewerId, verdict: Decision, findings: Vec<Finding>) -> RawVerdict {
        RawVerdict {
            reviewer,
            outcome: Outcome::Reviewed {
                review: Review { verdict, findings },
            },
            usage: Usage {
                input_tokens: 1200,
                output_tokens: 300,
                cached_tokens: 0,
                cost_usd: Some(0.0123),
            },
        }
    }

    #[test]
    fn parse_tolerates_fences_prose_and_string_lines() {
        let text = "Sure, here is the review:\n```json\n{\"verdict\": \"changes\", \"findings\": [{\"severity\": \"major\", \"file\": \"a.rs\", \"line\": \"12\", \"message\": \"m\"}]}\n```\nDone.";
        let review = parse_review(text).expect("parses");
        assert_eq!(review.verdict, Decision::Changes);
        assert_eq!(review.findings[0].line, Some(12));
        assert_eq!(review.findings[0].suggested_fix, None);

        let null_line = "{\"verdict\":\"approve\",\"findings\":[{\"severity\":\"minor\",\"line\":null,\"message\":\"m\"}]}";
        let review = parse_review(null_line).expect("parses");
        assert_eq!(review.findings[0].file, None);
        assert_eq!(review.findings[0].line, None);
    }

    #[test]
    fn parse_rejects_prose_bad_severity_and_missing_verdict() {
        assert!(parse_review("I approve.").is_err());
        assert!(parse_review("{\"verdict\":\"maybe\",\"findings\":[]}").is_err());
        assert!(parse_review("{\"findings\":[]}").is_err());
        let bad_severity =
            "{\"verdict\":\"approve\",\"findings\":[{\"severity\":\"huge\",\"message\":\"m\"}]}";
        assert!(parse_review(bad_severity).is_err());
        assert!(parse_review("}{").is_err());
    }

    #[test]
    fn a_blocker_means_changes() {
        let raw = vec![
            reviewed(
                id("openai", Stance::Correctness),
                Decision::Approve,
                vec![finding(Severity::Blocker, "a.rs", 1, "wrong")],
            ),
            reviewed(id("google", Stance::Safety), Decision::Approve, vec![]),
        ];
        let verdict = Verdict::aggregate(raw);
        assert_eq!(verdict.decision, Decision::Changes);
        assert_eq!(verdict.reason, "1 blocker");
        assert_eq!(verdict.findings.len(), 1);
        assert!(verdict.minors.is_empty());
    }

    #[test]
    fn two_majors_mean_changes() {
        let raw = vec![
            reviewed(
                id("openai", Stance::Correctness),
                Decision::Approve,
                vec![finding(Severity::Major, "a.rs", 1, "one")],
            ),
            reviewed(
                id("google", Stance::Simplicity),
                Decision::Approve,
                vec![finding(Severity::Major, "b.rs", 2, "two")],
            ),
        ];
        let verdict = Verdict::aggregate(raw);
        assert_eq!(verdict.decision, Decision::Changes);
        assert_eq!(verdict.reason, "2 majors");
        assert_eq!(verdict.findings.len(), 2);
    }

    #[test]
    fn one_major_plus_minors_approves_with_minors_separate() {
        let raw = vec![
            reviewed(
                id("openai", Stance::Correctness),
                Decision::Changes,
                vec![
                    finding(Severity::Minor, "a.rs", 3, "nit"),
                    finding(Severity::Major, "a.rs", 1, "one"),
                ],
            ),
            reviewed(
                id("google", Stance::Style),
                Decision::Approve,
                vec![finding(Severity::Minor, "b.rs", 2, "rule x")],
            ),
        ];
        let verdict = Verdict::aggregate(raw);
        assert_eq!(
            verdict.decision,
            Decision::Approve,
            "the model's own `changes` is not trusted"
        );
        assert_eq!(verdict.findings.len(), 1);
        assert_eq!(verdict.findings[0].finding.severity, Severity::Major);
        assert_eq!(verdict.minors.len(), 2);
        assert_eq!(verdict.minors[1].reviewer.provider, "google");
    }

    #[test]
    fn a_failed_reviewer_or_no_reviewers_means_changes() {
        let raw = vec![
            reviewed(id("openai", Stance::Correctness), Decision::Approve, vec![]),
            RawVerdict {
                reviewer: id("google", Stance::Safety),
                outcome: Outcome::Failed {
                    error: "HTTP 500".to_owned(),
                },
                usage: Usage::default(),
            },
        ];
        let verdict = Verdict::aggregate(raw);
        assert_eq!(verdict.decision, Decision::Changes);
        assert_eq!(verdict.reason, "reviewer(s) failed: google/safety");
        assert_eq!(verdict.usage().cost_usd, None);

        let empty = Verdict::aggregate(Vec::new());
        assert_eq!(empty.decision, Decision::Changes);
    }

    #[test]
    fn findings_are_sorted_worst_first_and_usage_sums() {
        let raw = vec![
            reviewed(
                id("openai", Stance::Correctness),
                Decision::Changes,
                vec![finding(Severity::Major, "a.rs", 1, "major first in input")],
            ),
            reviewed(
                id("google", Stance::Safety),
                Decision::Changes,
                vec![finding(
                    Severity::Blocker,
                    "b.rs",
                    9,
                    "blocker second in input",
                )],
            ),
        ];
        let verdict = Verdict::aggregate(raw);
        assert_eq!(verdict.findings[0].finding.severity, Severity::Blocker);
        assert_eq!(verdict.findings[1].finding.severity, Severity::Major);
        let usage = verdict.usage();
        assert_eq!(usage.input_tokens, 2400);
        assert_eq!(usage.output_tokens, 600);
        assert!((usage.cost_usd.expect("priced") - 0.0246).abs() < 1e-9);
    }

    #[test]
    fn display_is_stable() {
        let mut with_fix = finding(Severity::Blocker, "src/lib.rs", 12, "returns Ok on failure");
        with_fix.suggested_fix = Some("propagate the error with ?".to_owned());
        let raw = vec![
            reviewed(
                id("openai", Stance::Correctness),
                Decision::Changes,
                vec![
                    with_fix,
                    finding(Severity::Minor, "src/lib.rs", 4, "rename x to count"),
                ],
            ),
            reviewed(
                id("google", Stance::Style),
                Decision::Approve,
                vec![Finding {
                    severity: Severity::Minor,
                    file: None,
                    line: None,
                    message: "rule google-docstrings: add # Errors".to_owned(),
                    suggested_fix: None,
                }],
            ),
            RawVerdict {
                reviewer: id("google", Stance::Safety),
                outcome: Outcome::Failed {
                    error: "provider `google` returned HTTP 500: boom".to_owned(),
                },
                usage: Usage::default(),
            },
        ];
        insta::assert_snapshot!(Verdict::aggregate(raw).to_string());
    }

    #[test]
    fn verdict_json_shape_is_stable() {
        let raw = vec![reviewed(
            id("openai", Stance::Correctness),
            Decision::Approve,
            vec![finding(Severity::Minor, "a.rs", 3, "nit")],
        )];
        insta::assert_json_snapshot!(Verdict::aggregate(raw));
    }
}
