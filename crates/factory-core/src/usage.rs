//! Token usage from anvil's telemetry files, priced with the providers.
//!
//! anvil prints no usage; it appends one JSON object per line to
//! `<worktree>/.anvil/telemetry/<date>.jsonl`. The `turn` and `compacted`
//! records carry `tokens`. This mirrors `eval::telemetry`, which this crate
//! cannot depend on.

use std::collections::BTreeMap;
use std::path::Path;

use anvil_types::ProviderConfig;
use serde::Deserialize;

use crate::ports::Tokens;

/// The telemetry folder inside a worktree.
pub const TELEMETRY_DIR: &str = ".anvil/telemetry";

/// The file anvil writes signals to; it holds no usage.
const SIGNALS_FILE: &str = "signals.jsonl";

/// What one run used.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Usage {
    /// Tokens over every provider.
    pub tokens: Tokens,
    /// Cost in USD; `None` when a provider used has no price.
    pub cost_usd: Option<f64>,
}

/// The fields of a telemetry record this module reads.
#[derive(Debug, Deserialize)]
struct Record {
    event: String,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    tokens: Option<RecordTokens>,
}

#[derive(Debug, Deserialize, Clone, Copy)]
struct RecordTokens {
    #[serde(default)]
    input: u64,
    #[serde(default)]
    output: u64,
    #[serde(default)]
    cached: u64,
}

/// Sums and prices the usage recorded under a worktree.
///
/// # Arguments
///
/// * `worktree` - The worktree anvil ran in.
/// * `providers` - `[providers]` from the anvil configuration, for prices.
///
/// # Returns
///
/// The usage; zero when there is no telemetry folder.
pub fn read(worktree: &Path, providers: &BTreeMap<String, ProviderConfig>) -> Usage {
    let dir = worktree.join(TELEMETRY_DIR);
    let mut per_provider: BTreeMap<String, Tokens> = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Usage::default();
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_log = path.extension().is_some_and(|ext| ext == "jsonl")
            && path.file_name().is_some_and(|name| name != SIGNALS_FILE);
        if !is_log {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in text.lines().filter(|line| !line.trim().is_empty()) {
            let Ok(record) = serde_json::from_str::<Record>(line) else {
                continue;
            };
            if record.event != "turn" && record.event != "compacted" {
                continue;
            }
            let Some(tokens) = record.tokens else {
                continue;
            };
            let provider = record.provider.unwrap_or_else(|| "unknown".to_owned());
            per_provider.entry(provider).or_default().add(Tokens {
                input: tokens.input,
                output: tokens.output,
                cached: tokens.cached,
            });
        }
    }
    let mut usage = Usage {
        tokens: Tokens::default(),
        cost_usd: Some(0.0),
    };
    for (provider, tokens) in &per_provider {
        usage.tokens.add(*tokens);
        let price = providers.get(provider).and_then(|p| p.price_per_mtoken);
        usage.cost_usd = match (usage.cost_usd, price) {
            (Some(total), Some(price)) => {
                Some(total + price.cost(tokens.input, tokens.output, tokens.cached))
            }
            _ => None,
        };
    }
    usage
}

/// Removes the telemetry folder, so the next read covers one run only.
///
/// # Arguments
///
/// * `worktree` - The worktree.
pub fn clear(worktree: &Path) {
    let _ = std::fs::remove_dir_all(worktree.join(TELEMETRY_DIR));
}

#[cfg(test)]
mod tests {
    use super::*;
    use anvil_types::config::SETUP_PROVIDERS;

    #[test]
    fn sums_turn_and_compacted_records_and_prices_them() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().join(TELEMETRY_DIR);
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join("2026-10-31.jsonl"),
            concat!(
                r#"{"event":"session_start","session":"s"}"#,
                "\n",
                r#"{"event":"turn","session":"s","provider":"anthropic","model":"m","tokens":{"input":1000000,"output":0,"cached":0}}"#,
                "\n",
                r#"{"event":"compacted","session":"s","provider":"anthropic","tokens":{"input":0,"output":1000000,"cached":0}}"#,
                "\n",
                r#"{"event":"tool","session":"s","tokens":{"input":5,"output":5,"cached":5}}"#,
                "\n",
                "not json\n",
            ),
        )
        .expect("write");
        std::fs::write(
            dir.join(SIGNALS_FILE),
            r#"{"event":"turn","tokens":{"input":9}}"#,
        )
        .expect("write");
        let config: anvil_types::AnvilConfig = toml::from_str(SETUP_PROVIDERS).expect("parses");
        let usage = read(temp.path(), &config.providers);
        assert_eq!(
            usage.tokens,
            Tokens {
                input: 1_000_000,
                output: 1_000_000,
                cached: 0
            }
        );
        assert!(
            (usage.cost_usd.expect("priced") - 24.0).abs() < 1e-9,
            "{usage:?}"
        );

        let unpriced = read(temp.path(), &BTreeMap::new());
        assert_eq!(unpriced.tokens.total(), 2_000_000);
        assert_eq!(unpriced.cost_usd, None);

        clear(temp.path());
        assert_eq!(read(temp.path(), &config.providers), Usage::default());
        assert!(!dir.exists());
    }
}
