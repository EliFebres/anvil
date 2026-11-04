//! `anvil stats`: a summary of this project's telemetry.

use std::collections::BTreeMap;
use std::path::Path;

use anvil_telemetry::paths::project_telemetry_dir;
use anvil_telemetry::{Stats, load_records, parse_since};
use anvil_types::AnvilConfig;
use chrono::Utc;

/// Prints the summary for the records newer than `since`.
///
/// # Arguments
///
/// * `config` - The configuration, for provider prices; `None` when there
///   is none, in which case costs print as unknown.
/// * `cwd` - The project root.
/// * `since` - How far back to look, such as `7d`.
///
/// # Returns
///
/// Zero.
///
/// # Errors
///
/// When `since` is not a duration or the telemetry folder cannot be read.
pub fn run(config: Option<&AnvilConfig>, cwd: &Path, since: &str) -> anyhow::Result<i32> {
    let window = parse_since(since)?;
    let records = load_records(&project_telemetry_dir(cwd), Some(Utc::now() - window))?;
    let prices: BTreeMap<String, Option<anvil_types::Price>> = config
        .map(|config| {
            config
                .providers
                .iter()
                .map(|(name, provider)| (name.clone(), provider.price_per_mtoken))
                .collect()
        })
        .unwrap_or_default();
    let stats = Stats::compute(&records, &prices);
    println!("anvil stats, last {since}\n");
    print!("{stats}");
    Ok(0)
}
