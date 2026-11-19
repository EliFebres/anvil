//! Where telemetry lives on disk.

use std::path::{Path, PathBuf};

use chrono::NaiveDate;

/// Project-local telemetry folder, relative to the project root.
pub const TELEMETRY_DIR: &str = ".anvil/telemetry";

/// The telemetry folder of a project.
///
/// # Arguments
///
/// * `project_root` - The project directory.
///
/// # Returns
///
/// `<project_root>/.anvil/telemetry`.
pub fn project_telemetry_dir(project_root: &Path) -> PathBuf {
    project_root.join(TELEMETRY_DIR)
}

/// The cross-project telemetry folder under the anvil home.
///
/// # Arguments
///
/// * `home` - The user's home directory.
///
/// # Returns
///
/// `<home>/.anvil/telemetry`.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_hang_off_the_anvil_dir() {
        let root = Path::new("proj");
        assert_eq!(
            project_telemetry_dir(root),
            Path::new("proj/.anvil/telemetry")
        );
        assert_eq!(
            home_telemetry_dir(Path::new("home")),
            Path::new("home/.anvil/telemetry")
        );
        let date = NaiveDate::from_ymd_opt(2026, 10, 30).expect("date");
        assert_eq!(
            day_file(&project_telemetry_dir(root), date),
            Path::new("proj/.anvil/telemetry/2026-10-30.jsonl")
        );
    }
}
