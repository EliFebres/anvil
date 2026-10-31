//! Rolling records up into the numbers `forge stats` prints.

use std::collections::BTreeMap;
use std::fmt;

use chrono::Duration;
use forge_types::Price;

use crate::error::TelemetryError;
use crate::record::{Record, RecordKind, SessionOutcome, ToolOutcomeKind};

/// How many tools and failure classes the summary lists.
const TOP_N: usize = 10;

/// Parses a `--since` value such as `7d`, `36h`, `2w` or `30m`.
///
/// # Arguments
///
/// * `text` - The value.
///
/// # Returns
///
/// The duration.
///
/// # Errors
///
/// [`TelemetryError::BadDuration`] when the text is not a positive number
/// followed by `m`, `h`, `d` or `w`.
pub fn parse_since(text: &str) -> Result<Duration, TelemetryError> {
    let bad = || TelemetryError::BadDuration {
        text: text.to_owned(),
    };
    let trimmed = text.trim();
    let (number, unit) = trimmed.split_at(trimmed.len().saturating_sub(1));
    let amount: i64 = number.parse().map_err(|_| bad())?;
    if amount <= 0 {
        return Err(bad());
    }
    match unit {
        "m" => Ok(Duration::minutes(amount)),
        "h" => Ok(Duration::hours(amount)),
        "d" => Ok(Duration::days(amount)),
        "w" => Ok(Duration::weeks(amount)),
        _ => Err(bad()),
    }
}

/// Token use and cost of one provider.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ProviderStats {
    /// Completions, compactions included.
    pub turns: usize,
    /// Uncached input tokens.
    pub input: u64,
    /// Output tokens.
    pub output: u64,
    /// Cached input tokens.
    pub cached: u64,
    /// Estimated cost in USD, when the provider has a price.
    pub cost: Option<f64>,
}

/// Calls and failures of one tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolStats {
    /// The tool name.
    pub name: String,
    /// How many times it was called.
    pub calls: usize,
    /// How many calls did not complete.
    pub failures: usize,
}

/// What `forge stats` prints.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Stats {
    /// Sessions seen, chats included.
    pub sessions: usize,
    /// Chat sessions, which have no completion status.
    pub chats: usize,
    /// Runs that reached Done.
    pub done: usize,
    /// Runs that ended any other way.
    pub failed: usize,
    /// Token use per provider.
    pub providers: BTreeMap<String, ProviderStats>,
    /// The most called tools, most calls first.
    pub tools: Vec<ToolStats>,
    /// The most common failure classes, most common first.
    pub failures: Vec<(String, usize)>,
}

impl Stats {
    /// Rolls records up.
    ///
    /// # Arguments
    ///
    /// * `records` - Records from [`crate::load_records`].
    /// * `prices` - Each provider's price, when known.
    ///
    /// # Returns
    ///
    /// The summary.
    pub fn compute(records: &[Record], prices: &BTreeMap<String, Option<Price>>) -> Self {
        let mut stats = Self::default();
        let mut sessions: BTreeMap<&str, Option<&SessionOutcome>> = BTreeMap::new();
        let mut tools: BTreeMap<&str, ToolStats> = BTreeMap::new();
        let mut failures: BTreeMap<String, usize> = BTreeMap::new();
        for record in records {
            let session = sessions.entry(&record.session).or_default();
            match &record.kind {
                RecordKind::SessionEnded { outcome } => *session = Some(outcome),
                RecordKind::Turn {
                    provider, tokens, ..
                } => {
                    let entry = stats.providers.entry(provider.clone()).or_default();
                    entry.turns += 1;
                    entry.input += tokens.input;
                    entry.output += tokens.output;
                    entry.cached += tokens.cached;
                }
                RecordKind::Compacted {
                    provider,
                    tokens: Some(tokens),
                    ..
                } => {
                    let entry = stats.providers.entry(provider.clone()).or_default();
                    entry.turns += 1;
                    entry.input += tokens.input;
                    entry.output += tokens.output;
                    entry.cached += tokens.cached;
                }
                RecordKind::ToolResult { name, outcome, .. } => {
                    let entry = tools.entry(name).or_insert_with(|| ToolStats {
                        name: name.clone(),
                        calls: 0,
                        failures: 0,
                    });
                    entry.calls += 1;
                    if *outcome != ToolOutcomeKind::Completed {
                        entry.failures += 1;
                    }
                }
                RecordKind::ProviderError { kind, .. } => {
                    *failures
                        .entry(format!("provider_error:{kind}"))
                        .or_default() += 1;
                }
                _ => {}
            }
        }
        stats.sessions = sessions.len();
        for outcome in sessions.into_values() {
            match outcome {
                Some(SessionOutcome::Done) => stats.done += 1,
                Some(SessionOutcome::Chat) => stats.chats += 1,
                Some(other) => {
                    stats.failed += 1;
                    *failures.entry(other.class()).or_default() += 1;
                }
                None => stats.failed += 1,
            }
        }
        for (name, entry) in &mut stats.providers {
            entry.cost = prices
                .get(name)
                .copied()
                .flatten()
                .map(|price| price.cost(entry.input, entry.output, entry.cached));
        }
        stats.tools = tools.into_values().collect();
        stats
            .tools
            .sort_by(|a, b| b.calls.cmp(&a.calls).then(a.name.cmp(&b.name)));
        stats.tools.truncate(TOP_N);
        stats.failures = failures.into_iter().collect();
        stats
            .failures
            .sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        stats.failures.truncate(TOP_N);
        stats
    }

    /// Runs that reached Done as a share of runs, or `None` without runs.
    ///
    /// # Returns
    ///
    /// The rate from 0 to 1.
    pub fn completion_rate(&self) -> Option<f64> {
        let runs = self.done + self.failed;
        (runs > 0).then(|| self.done as f64 / runs as f64)
    }
}

impl fmt::Display for Stats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "sessions: {} ({} runs, {} chats)",
            self.sessions,
            self.done + self.failed,
            self.chats
        )?;
        match self.completion_rate() {
            Some(rate) => writeln!(
                f,
                "completion rate: {:.0}% ({} done, {} incomplete)",
                rate * 100.0,
                self.done,
                self.failed
            )?,
            None => writeln!(f, "completion rate: n/a")?,
        }
        writeln!(
            f,
            "\nprovider        turns   input      output     cached     cost"
        )?;
        for (name, p) in &self.providers {
            let cost = p
                .cost
                .map(|cost| format!("${cost:.2}"))
                .unwrap_or_else(|| "n/a".to_owned());
            writeln!(
                f,
                "{name:<15} {:<7} {:<10} {:<10} {:<10} {cost}",
                p.turns, p.input, p.output, p.cached
            )?;
        }
        if self.providers.is_empty() {
            writeln!(f, "(no completions)")?;
        }
        writeln!(f, "\ntop tools")?;
        for tool in &self.tools {
            writeln!(
                f,
                "  {:<15} {} calls, {} failed",
                tool.name, tool.calls, tool.failures
            )?;
        }
        if self.tools.is_empty() {
            writeln!(f, "  (none)")?;
        }
        writeln!(f, "\ntop failure classes")?;
        for (class, count) in &self.failures {
            writeln!(f, "  {class:<40} {count}")?;
        }
        if self.failures.is_empty() {
            writeln!(f, "  (none)")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::record::{SessionMode, StopKind, Tokens};

    fn record(session: &str, kind: RecordKind) -> Record {
        Record {
            ts: Utc::now(),
            session: session.to_owned(),
            project: "p".to_owned(),
            kind,
        }
    }

    fn turn(session: &str, provider: &str, input: u64, output: u64, cached: u64) -> Record {
        record(
            session,
            RecordKind::Turn {
                state: "build".to_owned(),
                provider: provider.to_owned(),
                model: "m".to_owned(),
                tokens: Tokens {
                    input,
                    output,
                    cached,
                },
                latency_ms: 10,
                retries: 0,
                stop_reason: StopKind::EndTurn,
            },
        )
    }

    #[test]
    fn parse_since_accepts_units_and_rejects_junk() {
        assert_eq!(parse_since("7d").expect("ok"), Duration::days(7));
        assert_eq!(parse_since(" 36h ").expect("ok"), Duration::hours(36));
        assert_eq!(parse_since("2w").expect("ok"), Duration::weeks(2));
        assert_eq!(parse_since("30m").expect("ok"), Duration::minutes(30));
        for bad in ["", "d", "7", "0d", "-1d", "7y", "x7d"] {
            assert!(parse_since(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn compute_counts_sessions_tokens_cost_tools_and_failures() {
        let records = vec![
            record(
                "a",
                RecordKind::SessionStarted {
                    mode: SessionMode::Run,
                },
            ),
            turn("a", "fast", 1_000_000, 500_000, 2_000_000),
            record(
                "a",
                RecordKind::tool_result("shell", ToolOutcomeKind::Failed, "boom", 4, false, 1),
            ),
            record(
                "a",
                RecordKind::tool_result("glob", ToolOutcomeKind::Completed, "", 4, false, 1),
            ),
            record(
                "a",
                RecordKind::tool_result("glob", ToolOutcomeKind::Completed, "", 4, false, 1),
            ),
            record(
                "a",
                RecordKind::SessionEnded {
                    outcome: SessionOutcome::Done,
                },
            ),
            turn("b", "slow", 10, 10, 0),
            record(
                "b",
                RecordKind::Compacted {
                    dropped: 3,
                    provider: "slow".to_owned(),
                    model: "m".to_owned(),
                    tokens: Some(Tokens {
                        input: 5,
                        output: 5,
                        cached: 0,
                    }),
                },
            ),
            record(
                "b",
                RecordKind::ProviderError {
                    provider: "slow".to_owned(),
                    kind: "http".to_owned(),
                    http_status: Some(503),
                },
            ),
            record(
                "b",
                RecordKind::SessionEnded {
                    outcome: SessionOutcome::ProviderError {
                        kind: "http".to_owned(),
                    },
                },
            ),
            record(
                "c",
                RecordKind::SessionEnded {
                    outcome: SessionOutcome::Chat,
                },
            ),
            record(
                "d",
                RecordKind::SessionStarted {
                    mode: SessionMode::Run,
                },
            ),
        ];
        let mut prices = BTreeMap::new();
        prices.insert(
            "fast".to_owned(),
            Some(Price {
                input: 1.0,
                output: 2.0,
                cached: Some(0.5),
            }),
        );
        prices.insert("slow".to_owned(), None);
        let stats = Stats::compute(&records, &prices);
        assert_eq!(stats.sessions, 4);
        assert_eq!(stats.chats, 1);
        assert_eq!(stats.done, 1);
        assert_eq!(
            stats.failed, 2,
            "a crashed session with no end counts as failed"
        );
        assert!((stats.completion_rate().expect("rate") - 1.0 / 3.0).abs() < 1e-9);
        let fast = &stats.providers["fast"];
        assert_eq!(fast.turns, 1);
        assert!((fast.cost.expect("cost") - 3.0).abs() < 1e-9);
        let slow = &stats.providers["slow"];
        assert_eq!(slow.turns, 2);
        assert_eq!(slow.input, 15);
        assert_eq!(slow.cost, None);
        assert_eq!(stats.tools[0].name, "glob");
        assert_eq!(stats.tools[0].calls, 2);
        assert_eq!(stats.tools[1].failures, 1);
        assert_eq!(
            stats.failures,
            vec![("provider_error:http".to_owned(), 2)],
            "the session outcome and the provider error share a class"
        );
        let text = stats.to_string();
        assert!(text.contains("completion rate: 33%"));
        assert!(text.contains("$3.00"));
        assert!(text.contains("provider_error:http"));
    }

    #[test]
    fn empty_stats_print_placeholders() {
        let text = Stats::compute(&[], &BTreeMap::new()).to_string();
        assert!(text.contains("sessions: 0"));
        assert!(text.contains("completion rate: n/a"));
        assert!(text.contains("(no completions)"));
    }
}
