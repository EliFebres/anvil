//! Rolling records up into the numbers `anvil stats` prints.

use std::collections::BTreeMap;
use std::fmt;

use anvil_types::Price;
use chrono::Duration;

use crate::error::TelemetryError;
use crate::record::{Record, RecordKind, SessionOutcome, Tokens, ToolOutcomeKind};

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

impl ProviderStats {
    /// Prompt tokens (uncached plus cached input) per turn: how much context
    /// each completion re-sent.
    ///
    /// # Returns
    ///
    /// The mean, or `None` without turns.
    pub fn context_per_turn(&self) -> Option<u64> {
        (self.turns > 0).then(|| (self.input + self.cached) / self.turns as u64)
    }
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

/// What `anvil stats` prints.
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
    /// Completions over every provider, compactions included.
    pub turns: usize,
    /// Token use per provider.
    pub providers: BTreeMap<String, ProviderStats>,
    /// Token use and cost per role, each turn priced by the provider that
    /// served it. Empty for records written before roles were recorded.
    pub roles: BTreeMap<String, ProviderStats>,
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
        let mut role_costs: BTreeMap<String, Option<f64>> = BTreeMap::new();
        for record in records {
            let session = sessions.entry(&record.session).or_default();
            match &record.kind {
                RecordKind::SessionEnded { outcome } => *session = Some(outcome),
                RecordKind::Turn {
                    provider,
                    tokens,
                    role,
                    ..
                } => {
                    let entry = stats.providers.entry(provider.clone()).or_default();
                    entry.turns += 1;
                    entry.input += tokens.input;
                    entry.output += tokens.output;
                    entry.cached += tokens.cached;
                    if let Some(role) = role {
                        add_to_role(
                            &mut stats.roles,
                            &mut role_costs,
                            prices,
                            role,
                            provider,
                            tokens,
                        );
                    }
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
                    add_to_role(
                        &mut stats.roles,
                        &mut role_costs,
                        prices,
                        "summarizer",
                        provider,
                        tokens,
                    );
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
        stats.turns = stats.providers.values().map(|p| p.turns).sum();
        for (name, entry) in &mut stats.providers {
            entry.cost = prices
                .get(name)
                .copied()
                .flatten()
                .map(|price| price.cost(entry.input, entry.output, entry.cached));
        }
        for (role, cost) in role_costs {
            if let Some(entry) = stats.roles.get_mut(&role) {
                entry.cost = cost;
            }
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

/// Adds one completion to a role's totals, priced with the price of the
/// provider that served it; a provider without a price makes the role's
/// cost unknown.
fn add_to_role(
    roles: &mut BTreeMap<String, ProviderStats>,
    costs: &mut BTreeMap<String, Option<f64>>,
    prices: &BTreeMap<String, Option<Price>>,
    role: &str,
    provider: &str,
    tokens: &Tokens,
) {
    let entry = roles.entry(role.to_owned()).or_default();
    entry.turns += 1;
    entry.input += tokens.input;
    entry.output += tokens.output;
    entry.cached += tokens.cached;
    let cost = prices
        .get(provider)
        .copied()
        .flatten()
        .map(|price| price.cost(tokens.input, tokens.output, tokens.cached));
    let total = costs.entry(role.to_owned()).or_insert(Some(0.0));
    *total = match (*total, cost) {
        (Some(sum), Some(cost)) => Some(sum + cost),
        _ => None,
    };
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
        if self.sessions > 0 {
            writeln!(
                f,
                "turns per session: {:.1}",
                self.turns as f64 / self.sessions as f64
            )?;
        }
        writeln!(
            f,
            "\nprovider        turns   input      output     cached     ctx/turn   cost"
        )?;
        for (name, p) in &self.providers {
            let cost = p
                .cost
                .map(|cost| format!("${cost:.2}"))
                .unwrap_or_else(|| "n/a".to_owned());
            let context = p
                .context_per_turn()
                .map_or_else(|| "n/a".to_owned(), |c| c.to_string());
            writeln!(
                f,
                "{name:<15} {:<7} {:<10} {:<10} {:<10} {context:<10} {cost}",
                p.turns, p.input, p.output, p.cached
            )?;
        }
        if self.providers.is_empty() {
            writeln!(f, "(no completions)")?;
        }
        if self.roles.contains_key("planner") {
            writeln!(
                f,
                "
role            turns   input      output     cached     cost"
            )?;
            for (name, p) in &self.roles {
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
                role: None,
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

    fn role_turn(role: &str, provider: &str, input: u64) -> Record {
        let mut record = turn("a", provider, input, 0, 0);
        if let RecordKind::Turn { role: slot, .. } = &mut record.kind {
            *slot = Some(role.to_owned());
        }
        record
    }

    #[test]
    fn each_role_is_priced_by_the_provider_that_served_it() {
        let prices = BTreeMap::from([
            (
                "dear".to_owned(),
                Some(Price {
                    input: 15.0,
                    output: 75.0,
                    cached: None,
                }),
            ),
            (
                "cheap".to_owned(),
                Some(Price {
                    input: 1.0,
                    output: 5.0,
                    cached: None,
                }),
            ),
        ]);
        let records = vec![
            role_turn("planner", "dear", 1_000_000),
            role_turn("coder", "cheap", 1_000_000),
            role_turn("coder", "cheap", 1_000_000),
        ];
        let stats = Stats::compute(&records, &prices);
        assert_eq!(stats.roles["planner"].turns, 1);
        assert_eq!(stats.roles["planner"].cost, Some(15.0));
        assert_eq!(stats.roles["coder"].turns, 2);
        assert_eq!(stats.roles["coder"].cost, Some(2.0));
        let text = stats.to_string();
        assert!(text.contains("role            turns"), "{text}");
        assert!(text.contains("planner"), "{text}");

        let without_planner = Stats::compute(&records[1..], &prices).to_string();
        assert!(!without_planner.contains("role            turns"));
    }

    #[test]
    fn records_without_roles_print_no_role_table() {
        let records = vec![turn("a", "m", 100, 10, 0)];
        let stats = Stats::compute(&records, &BTreeMap::new());
        assert!(stats.roles.is_empty());
        assert!(!stats.to_string().contains("role            turns"));
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
        assert_eq!(stats.turns, 3);
        assert_eq!(
            slow.context_per_turn(),
            Some((slow.input + slow.cached) / 2)
        );
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
        assert!(text.contains("turns per session: 0.8"), "{text}");
        assert!(text.contains("ctx/turn"));
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
