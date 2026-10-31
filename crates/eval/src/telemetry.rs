//! Recovers token usage from anvil's telemetry files and prices it.
//!
//! anvil prints no usage; it appends one JSON object per line to
//! `<cwd>/.anvil/telemetry/<UTC date>.jsonl`. The harness reads the `turn`
//! and `compacted` records of one session and sums their `tokens`.

use std::collections::BTreeMap;
use std::path::Path;

use anvil_types::ProviderConfig;
use serde::Deserialize;

use crate::error::EvalError;
use crate::results::Usage;

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
}

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
    let entries = std::fs::read_dir(telemetry_dir)
        .map_err(|source| EvalError::io("reading", telemetry_dir, source))?;
    for entry in entries {
        let path = entry
            .map_err(|source| EvalError::io("reading", telemetry_dir, source))?
            .path();
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
        let is_usage = record.event == "turn" || record.event == "compacted";
        let Some(tokens) = record.tokens.filter(|_| is_usage) else {
            return;
        };
        let provider = record.provider.unwrap_or_else(|| "unknown".to_owned());
        let model = record.model.unwrap_or_default();
        if record.event == "turn" {
            self.turns += 1;
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
