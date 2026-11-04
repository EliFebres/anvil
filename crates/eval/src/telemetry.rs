//! Recovers token usage from anvil's telemetry files and prices it.
//!
//! anvil prints no usage; it appends one JSON object per line to
//! `<cwd>/.anvil/telemetry/<UTC date>.jsonl`. The harness reads the `turn`
//! and `compacted` records of one session and sums their `tokens`, counts
//! its `tool_call` records, and keeps what the `review`, `check`,
//! `memory_skipped` and `session_ended` records say about loop overheads.

use std::collections::BTreeMap;
use std::path::Path;

use anvil_types::ProviderConfig;
use serde::Deserialize;

use crate::error::EvalError;
use crate::results::{CheckSource, Overheads, StateTurns, Usage};

/// The file anvil writes signals to; it holds no usage.
const SIGNALS_FILE: &str = "signals.jsonl";

/// Tokens one provider used.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderUsage {
    /// The model telemetry named for the provider.
    pub model: String,
    /// The tokens.
    pub tokens: Usage,
}

/// Everything one session used.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionUsage {
    /// Tokens over every provider.
    pub total: Usage,
    /// Tokens per provider name.
    pub by_provider: BTreeMap<String, ProviderUsage>,
    /// The provider of the first turn: the coder.
    pub coder_provider: Option<String>,
    /// The model of the first turn.
    pub coder_model: Option<String>,
    /// How many `turn` records were counted.
    pub turns: u32,
    /// How many `tool_call` records were counted.
    pub tool_calls: u32,
    /// Every turn in order, with the tool calls that followed it.
    pub turn_log: Vec<TurnTrace>,
    /// How many `review` records were counted.
    pub reviews: u32,
    /// Whether the session ended at the review round cap.
    pub review_cap_hit: bool,
    /// Whether a detected check command ran.
    pub check_detected: bool,
    /// Whether a plan-declared check command ran.
    pub check_plan: bool,
    /// Why the memory summarizer was skipped, when a record said so.
    pub memory_skip_reason: Option<String>,
}

/// One model turn as telemetry recorded it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnTrace {
    /// The loop state it ran in; `unknown` when the record had none.
    pub state: String,
    /// The tool calls it made.
    pub calls: u32,
    /// Prompt tokens it read.
    pub prompt_tokens: u64,
}

impl SessionUsage {
    /// Turns per loop state, split by what each turn did.
    ///
    /// # Returns
    ///
    /// One entry per state that took a turn.
    pub fn turns_by_state(&self) -> BTreeMap<String, StateTurns> {
        let mut states: BTreeMap<String, StateTurns> = BTreeMap::new();
        for turn in &self.turn_log {
            states.entry(turn.state.clone()).or_default().count(
                &turn.state,
                turn.calls,
                turn.prompt_tokens,
            );
        }
        states
    }

    /// The loop overheads telemetry and the transcript recorded.
    ///
    /// # Arguments
    ///
    /// * `lone_tick_turns` - The count from the session transcript.
    ///
    /// # Returns
    ///
    /// The overheads of the session.
    pub fn overheads(&self, lone_tick_turns: u32) -> Overheads {
        let turns_in = |state: &str| self.turn_log.iter().filter(|t| t.state == state).count();
        Overheads {
            lone_tick_turns,
            review_rounds: self.reviews,
            review_cap_hit: self.review_cap_hit,
            verify_model_turns: turns_in("verify") as u32,
            check_source: match (self.check_detected, self.check_plan) {
                (true, _) => CheckSource::Detected,
                (false, true) => CheckSource::Plan,
                (false, false) => CheckSource::None,
            },
            memory_ran: turns_in(MEMORY_STATE) > 0,
            memory_skip_reason: self.memory_skip_reason.clone(),
        }
    }
}

/// The state telemetry gives the memory summarizer's turn.
const MEMORY_STATE: &str = "memory";

/// The `program` of a `check` record that ran the standing-rule checks.
const STANDING_PROGRAM: &str = "standing";

/// The reason tag of a session stopped by the review round cap.
const REVIEW_CAP_REASON: &str = "review_rounds_exhausted";

/// The fields of a telemetry record the harness reads.
#[derive(Debug, Deserialize)]
struct UsageRecord {
    event: String,
    session: String,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    tokens: Option<Usage>,
    #[serde(default)]
    state: Option<String>,
    /// `check`: `detected`, `plan` or `standing`.
    #[serde(default)]
    source: Option<String>,
    /// `check`: the program that ran.
    #[serde(default)]
    program: Option<String>,
    /// `session_ended` and `memory_skipped`: a fixed tag.
    #[serde(default)]
    reason: Option<String>,
}

/// Sums the usage of one session from a telemetry folder.
///
/// # Arguments
///
/// * `telemetry_dir` - `<cwd>/.anvil/telemetry` of the fixture copy.
/// * `session` - The session id to keep; `None` keeps every record, which is
///   right when the folder belongs to a single run.
///
/// # Returns
///
/// The usage; empty when the folder does not exist.
///
/// # Errors
///
/// When a file cannot be read. Lines that do not parse are skipped with a
/// warning.
pub fn read_session_usage(
    telemetry_dir: &Path,
    session: Option<&str>,
) -> Result<SessionUsage, EvalError> {
    let mut usage = SessionUsage::default();
    if !telemetry_dir.is_dir() {
        return Ok(usage);
    }
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(telemetry_dir)
        .map_err(|source| EvalError::io("reading", telemetry_dir, source))?
    {
        let entry = entry.map_err(|source| EvalError::io("reading", telemetry_dir, source))?;
        paths.push(entry.path());
    }
    // Files are named by date, so name order is time order, which a tool call
    // needs to find the turn it belongs to.
    paths.sort();
    for path in paths {
        let is_log = path.extension().is_some_and(|ext| ext == "jsonl")
            && path.file_name().is_some_and(|name| name != SIGNALS_FILE);
        if !is_log {
            continue;
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|source| EvalError::io("reading", &path, source))?;
        for line in text.lines().filter(|line| !line.trim().is_empty()) {
            match serde_json::from_str::<UsageRecord>(line) {
                Ok(record) => usage.absorb(record, session),
                Err(error) => {
                    tracing::warn!(%error, path = %path.display(), "skipping telemetry line")
                }
            }
        }
    }
    Ok(usage)
}

impl SessionUsage {
    /// Adds one record if it belongs to the session and carries tokens.
    fn absorb(&mut self, record: UsageRecord, session: Option<&str>) {
        if session.is_some_and(|wanted| record.session != wanted) {
            return;
        }
        if record.event == "tool_call" {
            self.tool_calls += 1;
            if let Some(turn) = self.turn_log.last_mut() {
                turn.calls += 1;
            }
            return;
        }
        if self.absorb_overhead(&record) {
            return;
        }
        let is_usage = record.event == "turn" || record.event == "compacted";
        let Some(tokens) = record.tokens.filter(|_| is_usage) else {
            return;
        };
        let provider = record.provider.unwrap_or_else(|| "unknown".to_owned());
        let model = record.model.unwrap_or_default();
        if record.event == "turn" {
            self.turns += 1;
            self.turn_log.push(TurnTrace {
                state: record.state.unwrap_or_else(|| "unknown".to_owned()),
                calls: 0,
                prompt_tokens: tokens.prompt(),
            });
            if self.coder_provider.is_none() {
                self.coder_provider = Some(provider.clone());
                self.coder_model = Some(model.clone());
            }
        }
        self.total.add(tokens);
        let entry = self.by_provider.entry(provider).or_default();
        if entry.model.is_empty() {
            entry.model = model;
        }
        entry.tokens.add(tokens);
    }
}

impl SessionUsage {
    /// Keeps what a `review`, `check`, `memory_skipped` or `session_ended`
    /// record says; returns whether the record was one of those.
    fn absorb_overhead(&mut self, record: &UsageRecord) -> bool {
        match record.event.as_str() {
            "review" => self.reviews += 1,
            "check" => match record.source.as_deref() {
                Some("detected") => self.check_detected = true,
                Some("plan") => self.check_plan = true,
                Some(_) => {}
                // Telemetry written before checks named their source: every
                // check that was not the standing-rule one was detected.
                None => {
                    self.check_detected |= record.program.as_deref() != Some(STANDING_PROGRAM);
                }
            },
            "memory_skipped" => self.memory_skip_reason = record.reason.clone(),
            // The reason tag survives a `denied` outcome, which would
            // otherwise hide that the cap was what stopped the session.
            "session_ended" => {
                self.review_cap_hit |= record.reason.as_deref() == Some(REVIEW_CAP_REASON);
            }
            _ => return false,
        }
        true
    }
}

/// Prices a session's usage.
///
/// # Arguments
///
/// * `providers` - The `[providers]` table of the anvil configuration.
/// * `usage` - The session's usage.
///
/// # Returns
///
/// The cost in USD; `Some(0.0)` when nothing was used, `None` when a provider
/// that was used has no `price_per_mtoken`. A provider name missing from the
/// table falls back to the provider whose `model` matches.
pub fn cost(providers: &BTreeMap<String, ProviderConfig>, usage: &SessionUsage) -> Option<f64> {
    let mut total = 0.0;
    for (name, used) in &usage.by_provider {
        let config = providers
            .get(name)
            .or_else(|| providers.values().find(|p| p.model == used.model))?;
        let price = config.price_per_mtoken.as_ref()?;
        total += price.cost(used.tokens.input, used.tokens.output, used.tokens.cached);
    }
    Some(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anvil_types::{Price, Protocol};

    const SESSION: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

    fn line(event: &str, session: &str, provider: &str, model: &str, tokens: &str) -> String {
        format!(
            "{{\"ts\":\"2026-10-30T10:00:00Z\",\"session\":\"{session}\",\"project\":\"p\",\"event\":\"{event}\",\"state\":\"build\",\"provider\":\"{provider}\",\"model\":\"{model}\",\"tokens\":{tokens},\"latency_ms\":5,\"retries\":0,\"stop_reason\":\"end_turn\"}}"
        )
    }

    fn provider(model: &str, price: Option<Price>) -> ProviderConfig {
        ProviderConfig {
            protocol: Protocol::Openai,
            base_url: "https://example.invalid/v1".to_owned(),
            api_key_env: "KEY".to_owned(),
            model: model.to_owned(),
            context_window: 1000,
            supports_tools: true,
            supports_reasoning: false,
            max_output_tokens: None,
            price_per_mtoken: price,
            script: None,
        }
    }

    #[test]
    fn sums_turns_and_compactions_of_one_session_across_files() {
        let dir = tempfile::tempdir().expect("tmp");
        let day_one = [
            line("session_started", SESSION, "", "", "null"),
            line(
                "turn",
                SESSION,
                "fast",
                "m1",
                "{\"input\":1000,\"output\":100,\"cached\":10}",
            ),
            line(
                "turn",
                "OTHER",
                "fast",
                "m1",
                "{\"input\":5,\"output\":5,\"cached\":5}",
            ),
            "not json".to_owned(),
        ]
        .join("\n");
        let day_two = [
            line(
                "compacted",
                SESSION,
                "summ",
                "m2",
                "{\"input\":200,\"output\":20,\"cached\":0}",
            ),
            line("compacted", SESSION, "summ", "m2", "null"),
            line("tool_call", SESSION, "", "", "null"),
        ]
        .join("\n");
        std::fs::write(dir.path().join("2026-10-30.jsonl"), day_one).expect("write");
        std::fs::write(dir.path().join("2026-10-31.jsonl"), day_two).expect("write");
        std::fs::write(dir.path().join("signals.jsonl"), "{}").expect("write");

        let usage = read_session_usage(dir.path(), Some(SESSION)).expect("reads");
        assert_eq!(
            usage.total,
            Usage {
                input: 1200,
                output: 120,
                cached: 10
            }
        );
        assert_eq!(usage.turns, 1);
        assert_eq!(usage.tool_calls, 1);
        assert_eq!(usage.coder_provider.as_deref(), Some("fast"));
        assert_eq!(usage.coder_model.as_deref(), Some("m1"));
        assert_eq!(usage.by_provider["summ"].tokens.input, 200);
        assert_eq!(usage.by_provider["summ"].model, "m2");

        let all = read_session_usage(dir.path(), None).expect("reads");
        assert_eq!(all.total.input, 1205);
        let missing = read_session_usage(&dir.path().join("nope"), None).expect("reads");
        assert_eq!(missing, SessionUsage::default());
    }

    #[test]
    fn tool_calls_belong_to_the_turn_before_them() {
        let dir = tempfile::tempdir().expect("tmp");
        let turn = |state: &str| {
            line(
                "turn",
                SESSION,
                "fast",
                "m1",
                "{\"input\":10,\"output\":1,\"cached\":5}",
            )
            .replace("\"state\":\"build\"", &format!("\"state\":\"{state}\""))
        };
        let call = line("tool_call", SESSION, "", "", "null");
        let log = [
            turn("plan"),
            call.clone(),
            call.clone(),
            turn("plan"),
            turn("build"),
            call.clone(),
            turn("review"),
            turn("verify"),
            call,
        ]
        .join("\n");
        std::fs::write(dir.path().join("2026-10-30.jsonl"), log).expect("write");

        let usage = read_session_usage(dir.path(), Some(SESSION)).expect("reads");
        let states = usage.turns_by_state();
        let plan = states["plan"];
        assert_eq!((plan.multi_call, plan.text, plan.total()), (1, 1, 2));
        assert_eq!(plan.prompt_tokens, 30);
        assert_eq!(states["build"].one_call, 1);
        assert_eq!(states["review"].verdict, 1);
        assert_eq!(states["verify"].one_call, 1);
        assert_eq!(usage.tool_calls, 4);
    }

    #[test]
    fn overheads_come_from_review_check_memory_and_end_records() {
        let record = |fields: &str| {
            format!("{{\"ts\":\"t\",\"session\":\"{SESSION}\",\"project\":\"p\",{fields}}}")
        };
        let turn = |state: &str| {
            line(
                "turn",
                SESSION,
                "fast",
                "m1",
                "{\"input\":1,\"output\":1,\"cached\":0}",
            )
            .replace("\"state\":\"build\"", &format!("\"state\":\"{state}\""))
        };
        let read = |lines: &[String]| {
            let dir = tempfile::tempdir().expect("tmp");
            std::fs::write(dir.path().join("2026-11-04.jsonl"), lines.join("\n")).expect("write");
            read_session_usage(dir.path(), Some(SESSION)).expect("reads")
        };

        let capped = read(&[
            turn("build"),
            record("\"event\":\"review\",\"approved\":false,\"items\":1,\"digests\":[]"),
            record("\"event\":\"review\",\"approved\":false,\"items\":1,\"digests\":[]"),
            record(
                "\"event\":\"check\",\"program\":\"standing\",\"source\":\"standing\",\"passed\":true",
            ),
            record("\"event\":\"check\",\"program\":\"bash\",\"source\":\"plan\",\"passed\":true"),
            turn("verify"),
            turn("verify"),
            record("\"event\":\"memory_skipped\",\"reason\":\"uneventful_small_diff\""),
            record(
                "\"event\":\"session_ended\",\"outcome\":\"denied\",\"reason\":\"review_rounds_exhausted\"",
            ),
        ]);
        assert_eq!(
            capped.overheads(2),
            Overheads {
                lone_tick_turns: 2,
                review_rounds: 2,
                review_cap_hit: true,
                verify_model_turns: 2,
                check_source: CheckSource::Plan,
                memory_ran: false,
                memory_skip_reason: Some("uneventful_small_diff".to_owned()),
            }
        );
        assert_eq!(capped.turns, 3, "overhead records are not turns");

        let older = read(&[
            record("\"event\":\"check\",\"program\":\"standing\",\"passed\":true"),
            record("\"event\":\"check\",\"program\":\"cargo\",\"passed\":true"),
            turn("memory"),
            record(
                "\"event\":\"session_ended\",\"outcome\":\"incomplete\",\"reason\":\"review_rounds_exhausted\"",
            ),
        ]);
        let overheads = older.overheads(0);
        assert_eq!(overheads.check_source, CheckSource::Detected);
        assert!(overheads.review_cap_hit && overheads.memory_ran);

        let plain = read(&[
            record("\"event\":\"check\",\"program\":\"standing\",\"passed\":true"),
            record("\"event\":\"session_ended\",\"outcome\":\"done\""),
        ]);
        assert_eq!(plain.overheads(0), Overheads::default());
    }

    #[test]
    fn cost_prices_each_provider_and_falls_back_by_model() {
        let mut usage = SessionUsage::default();
        usage.by_provider.insert(
            "fast".to_owned(),
            ProviderUsage {
                model: "m1".to_owned(),
                tokens: Usage {
                    input: 1_000_000,
                    output: 0,
                    cached: 0,
                },
            },
        );
        usage.by_provider.insert(
            "renamed".to_owned(),
            ProviderUsage {
                model: "m2".to_owned(),
                tokens: Usage {
                    input: 0,
                    output: 1_000_000,
                    cached: 0,
                },
            },
        );
        let mut providers = BTreeMap::new();
        providers.insert(
            "fast".to_owned(),
            provider(
                "m1",
                Some(Price {
                    input: 1.0,
                    output: 2.0,
                    cached: None,
                }),
            ),
        );
        providers.insert(
            "other".to_owned(),
            provider(
                "m2",
                Some(Price {
                    input: 3.0,
                    output: 4.0,
                    cached: None,
                }),
            ),
        );
        let total = cost(&providers, &usage).expect("priced");
        assert!((total - 5.0).abs() < 1e-9);

        providers.get_mut("other").expect("other").price_per_mtoken = None;
        assert_eq!(cost(&providers, &usage), None);
        assert_eq!(cost(&providers, &SessionUsage::default()), Some(0.0));
    }
}
