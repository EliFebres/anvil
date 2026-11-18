//! Where the stores live under the home directory.

use std::path::{Path, PathBuf};

/// The anvil directory under the home directory.
pub const ANVIL_DIR: &str = ".anvil";

/// The rules file inside [`ANVIL_DIR`].
pub const RULES_FILE: &str = "rules.toml";

/// The facts directory inside [`ANVIL_DIR`].
pub const MEMORY_DIR: &str = "memory";

/// The directory for check scripts inside [`ANVIL_DIR`].
pub const CHECKS_DIR: &str = "checks";

/// The proposals file inside [`MEMORY_DIR`].
pub const PROPOSED_FILE: &str = "proposed.md";

/// The templates directory inside [`ANVIL_DIR`].
pub const TEMPLATES_DIR: &str = "templates";

/// The directory of correction records inside [`MEMORY_DIR`].
pub const CORRECTIONS_DIR: &str = "corrections";

/// The name used for a project whose directory has no file name.
const FALLBACK_REPO_NAME: &str = "project";

/// `<home>/.anvil/rules.toml`.
///
/// # Arguments
///
/// * `home` - The home directory.
///
/// # Returns
///
/// The path.
pub fn rules_path(home: &Path) -> PathBuf {
    home.join(ANVIL_DIR).join(RULES_FILE)
}

/// `<home>/.anvil/memory`.
///
/// # Arguments
///
/// * `home` - The home directory.
///
/// # Returns
///
/// The path.
pub fn memory_dir(home: &Path) -> PathBuf {
    home.join(ANVIL_DIR).join(MEMORY_DIR)
}

/// `<home>/.anvil/checks`.
///
/// # Arguments
///
/// * `home` - The home directory.
///
/// # Returns
///
/// The path.
pub fn checks_dir(home: &Path) -> PathBuf {
    home.join(ANVIL_DIR).join(CHECKS_DIR)
}

/// `<home>/.anvil/templates`.
///
/// # Arguments
///
/// * `home` - The home directory.
///
/// # Returns
///
/// The path.
pub fn templates_dir(home: &Path) -> PathBuf {
    home.join(ANVIL_DIR).join(TEMPLATES_DIR)
}

/// `<home>/.anvil/memory/corrections/<repo>.toml`: the files anvil left in
/// a project, for spotting what the owner edits afterwards.
///
/// # Arguments
///
/// * `home` - The home directory.
/// * `cwd` - The project root, which names the record.
///
/// # Returns
///
/// The path.
pub fn corrections_path(home: &Path, cwd: &Path) -> PathBuf {
    memory_dir(home)
        .join(CORRECTIONS_DIR)
        .join(format!("{}.toml", repo_name(cwd)))
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
    fn paths_hang_off_the_anvil_dir() {
        let home = Path::new("/h");
        assert_eq!(rules_path(home), PathBuf::from("/h/.anvil/rules.toml"));
        assert_eq!(memory_dir(home), PathBuf::from("/h/.anvil/memory"));
        assert_eq!(checks_dir(home), PathBuf::from("/h/.anvil/checks"));
        assert_eq!(templates_dir(home), PathBuf::from("/h/.anvil/templates"));
        assert_eq!(
            corrections_path(home, Path::new("/work/anvil")),
            PathBuf::from("/h/.anvil/memory/corrections/anvil.toml")
        );
        assert_eq!(repo_name(Path::new("/work/anvil")), "anvil");
        assert_eq!(repo_name(Path::new("/")), "project");
    }
}
