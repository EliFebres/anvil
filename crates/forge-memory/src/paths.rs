//! Where the stores live under the home directory.

use std::path::{Path, PathBuf};

/// The forge directory under the home directory.
pub const FORGE_DIR: &str = ".forge";

/// The rules file inside [`FORGE_DIR`].
pub const RULES_FILE: &str = "rules.toml";

/// The facts directory inside [`FORGE_DIR`].
pub const MEMORY_DIR: &str = "memory";

/// The directory for check scripts inside [`FORGE_DIR`].
pub const CHECKS_DIR: &str = "checks";

/// The proposals file inside [`MEMORY_DIR`].
pub const PROPOSED_FILE: &str = "proposed.md";

/// The name used for a project whose directory has no file name.
const FALLBACK_REPO_NAME: &str = "project";

/// `<home>/.forge/rules.toml`.
///
/// # Arguments
///
/// * `home` - The home directory.
///
/// # Returns
///
/// The path.
pub fn rules_path(home: &Path) -> PathBuf {
    home.join(FORGE_DIR).join(RULES_FILE)
}

/// `<home>/.forge/memory`.
///
/// # Arguments
///
/// * `home` - The home directory.
///
/// # Returns
///
/// The path.
pub fn memory_dir(home: &Path) -> PathBuf {
    home.join(FORGE_DIR).join(MEMORY_DIR)
}

/// `<home>/.forge/checks`.
///
/// # Arguments
///
/// * `home` - The home directory.
///
/// # Returns
///
/// The path.
pub fn checks_dir(home: &Path) -> PathBuf {
    home.join(FORGE_DIR).join(CHECKS_DIR)
}

/// The name a repository's facts are filed under: its directory name.
///
/// # Arguments
///
/// * `cwd` - The project root.
///
/// # Returns
///
/// The last path component, or `project` when there is none.
pub fn repo_name(cwd: &Path) -> String {
    cwd.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| FALLBACK_REPO_NAME.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_hang_off_the_forge_dir() {
        let home = Path::new("/h");
        assert_eq!(rules_path(home), PathBuf::from("/h/.forge/rules.toml"));
        assert_eq!(memory_dir(home), PathBuf::from("/h/.forge/memory"));
        assert_eq!(checks_dir(home), PathBuf::from("/h/.forge/checks"));
        assert_eq!(repo_name(Path::new("/work/anvil")), "anvil");
        assert_eq!(repo_name(Path::new("/")), "project");
    }
}
