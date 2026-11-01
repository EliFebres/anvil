//! Tracing setup: a JSON file under `.anvil/logs/` and a human console sink.

use std::path::Path;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, fmt};

/// Folder for log files, relative to the project root.
pub const LOG_DIR: &str = ".anvil/logs";

/// Environment variable that sets the file filter, such as `anvil_core=debug`.
pub const FILTER_ENV: &str = "ANVIL_LOG";

/// Installs the global subscriber.
///
/// The console shows warnings and above; the file gets everything the
/// filter allows (default `info`). Logging never fails the command: when
/// the log folder cannot be created, only the console sink is installed.
///
/// # Arguments
///
/// * `cwd` - The project root.
///
/// # Returns
///
/// A guard that flushes the file on drop; keep it alive for the process.
pub fn init(cwd: &Path) -> Option<WorkerGuard> {
    let filter = EnvFilter::try_from_env(FILTER_ENV).unwrap_or_else(|_| EnvFilter::new("info"));
    let log_dir = cwd.join(LOG_DIR);
    if std::fs::create_dir_all(&log_dir).is_err() {
        let console = fmt::layer()
            .with_writer(std::io::stderr)
            .with_target(false)
            .without_time()
            .with_filter(LevelFilter::WARN);
        let _ = tracing_subscriber::registry()
            .with(filter)
            .with(console)
            .try_init();
        return None;
    }
    let file = tracing_appender::rolling::never(&log_dir, "anvil.jsonl");
    let (writer, guard) = tracing_appender::non_blocking(file);
    let json = fmt::layer().json().with_writer(writer);
    let console = fmt::layer()
        .with_writer(std::io::stderr)
        .with_target(false)
        .without_time()
        .with_filter(LevelFilter::WARN);
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(json)
        .with(console)
        .try_init();
    Some(guard)
}
