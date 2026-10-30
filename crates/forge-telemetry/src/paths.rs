//! Where telemetry and tickets live on disk.

use std::path::{Path, PathBuf};

use chrono::NaiveDate;

/// Project-local telemetry folder, relative to the project root.
pub const TELEMETRY_DIR: &str = ".forge/telemetry";

/// The signals file inside a telemetry folder.
pub const SIGNALS_FILE: &str = "signals.jsonl";

/// Drafted tickets, relative to the repository root. Per machine, not committed.
pub const DRAFT_DIR: &str = "tickets/draft";

/// Submitted tickets, relative to the repository root. Committed.
pub const OPEN_DIR: &str = "tickets/open";

/// The telemetry folder of a project.
///
/// # Arguments
///
/// * `project_root` - The project directory.
///
/// # Returns
///
/// `<project_root>/.forge/telemetry`.
pub fn project_telemetry_dir(project_root: &Path) -> PathBuf {
    project_root.join(TELEMETRY_DIR)
}

/// The cross-project telemetry folder under the forge home.
///
/// # Arguments
///
/// * `home` - The user's home directory.
///
/// # Returns
///
/// `<home>/.forge/telemetry`.
pub fn home_telemetry_dir(home: &Path) -> PathBuf {
    home.join(TELEMETRY_DIR)
}

/// The JSONL file that holds one day's records.
///
/// # Arguments
///
/// * `dir` - A telemetry folder.
/// * `date` - The day.
///
/// # Returns
///
/// `<dir>/<YYYY-MM-DD>.jsonl`.
pub fn day_file(dir: &Path, date: NaiveDate) -> PathBuf {
    dir.join(format!("{}.jsonl", date.format("%Y-%m-%d")))
}

/// The signals file of a telemetry folder.
///
/// # Arguments
///
/// * `dir` - A telemetry folder.
///
/// # Returns
///
/// `<dir>/signals.jsonl`.
pub fn signals_file(dir: &Path) -> PathBuf {
    dir.join(SIGNALS_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_hang_off_the_forge_dir() {
        let root = Path::new("proj");
        assert_eq!(
            project_telemetry_dir(root),
            Path::new("proj/.forge/telemetry")
        );
        assert_eq!(
            home_telemetry_dir(Path::new("home")),
            Path::new("home/.forge/telemetry")
        );
        let date = NaiveDate::from_ymd_opt(2026, 10, 30).expect("date");
        assert_eq!(
            day_file(&project_telemetry_dir(root), date),
            Path::new("proj/.forge/telemetry/2026-10-30.jsonl")
        );
        assert_eq!(signals_file(Path::new("t")), Path::new("t/signals.jsonl"));
    }
}
