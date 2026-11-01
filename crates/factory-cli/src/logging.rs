//! Tracing setup: a JSON file under `~/.factory/logs/` rolled daily, and a
//! human console sink on stderr, which journald captures under systemd.

use std::path::Path;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, fmt};

/// Environment variable that sets the filter, such as `factory_core=debug`.
pub const FILTER_ENV: &str = "FACTORY_LOG";

/// The log file name; the appender adds the date.
pub const LOG_FILE: &str = "factory.jsonl";

/// Installs the global subscriber.
///
/// The console shows info and above without timestamps (journald adds its
/// own); the file gets everything the filter allows (default `info`).
/// Logging never fails the command: when the log folder cannot be created,
/// only the console sink is installed.
///
/// # Arguments
///
/// * `logs_dir` - `~/.factory/logs`.
///
/// # Returns
///
/// A guard that flushes the file on drop; keep it alive for the process.
pub fn init(logs_dir: &Path) -> Option<WorkerGuard> {
    let filter = EnvFilter::try_from_env(FILTER_ENV).unwrap_or_else(|_| EnvFilter::new("info"));
    if std::fs::create_dir_all(logs_dir).is_err() {
        let console = fmt::layer()
            .with_writer(std::io::stderr)
            .with_target(false)
            .without_time()
            .with_filter(LevelFilter::INFO);
        let _ = tracing_subscriber::registry()
            .with(filter)
            .with(console)
            .try_init();
        return None;
    }
    let file = tracing_appender::rolling::daily(logs_dir, LOG_FILE);
    let (writer, guard) = tracing_appender::non_blocking(file);
    let json = fmt::layer().json().with_writer(writer);
    let console = fmt::layer()
        .with_writer(std::io::stderr)
        .with_target(false)
        .without_time()
        .with_filter(LevelFilter::INFO);
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(json)
        .with(console)
        .try_init();
    Some(guard)
}
