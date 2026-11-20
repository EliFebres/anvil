//! Where usage records live on disk.

use std::path::{Path, PathBuf};

use chrono::NaiveDate;

/// Project-local usage folder, relative to the project root.
pub const USAGE_DIR: &str = ".anvil/usage";

/// The usage folder of a project.
///
/// # Arguments
///
/// * `project_root` - The project directory.
///
/// # Returns
///
/// `<project_root>/.anvil/usage`.
pub fn project_usage_dir(project_root: &Path) -> PathBuf {
    project_root.join(USAGE_DIR)
}

/// The cross-project usage folder under the anvil home.
///
/// # Arguments
///
/// * `home` - The user's home directory.
///
/// # Returns
///
/// `<home>/.anvil/usage`.
pub fn home_usage_dir(home: &Path) -> PathBuf {
    home.join(USAGE_DIR)
}

/// The JSONL file that holds one day's records.
///
/// # Arguments
///
/// * `dir` - A usage folder.
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
        assert_eq!(project_usage_dir(root), Path::new("proj/.anvil/usage"));
        assert_eq!(
            home_usage_dir(Path::new("home")),
            Path::new("home/.anvil/usage")
        );
        let date = NaiveDate::from_ymd_opt(2026, 10, 30).expect("date");
        assert_eq!(
            day_file(&project_usage_dir(root), date),
            Path::new("proj/.anvil/usage/2026-10-30.jsonl")
        );
    }
}
