//! Detects how a project runs its checks, and the hook for checks that
//! come from outside the loop.

use std::path::{Path, PathBuf};

/// What a check supplied from outside the loop reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeCheckFailure {
    /// Which check failed, as the model should cite it.
    pub name: String,
    /// What it reported.
    pub output: String,
}

/// Checks the Verify state runs on the files the session changed, before
/// the project's own check command.
///
/// The CLI supplies them; anvil-core only knows that a failure sends the
/// loop back to Build with the output as feedback.
pub trait ChangeChecks: Send + Sync {
    /// Runs every check on the changed files.
    ///
    /// # Arguments
    ///
    /// * `cwd` - The project root.
    /// * `changed` - Absolute paths of the files the session changed.
    ///
    /// # Returns
    ///
    /// One entry per failed check; empty when everything passed.
    fn run(&self, cwd: &Path, changed: &[PathBuf]) -> Vec<ChangeCheckFailure>;
}

/// How the Verify state runs the project's checks.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum CheckCommand {
    /// Look at the project files, in the order documented on [`detect`].
    #[default]
    Detect,
    /// Run exactly this command line.
    Command(String),
    /// Nothing to run; the model writes one targeted test instead.
    None,
}

impl CheckCommand {
    /// Resolves to a concrete command line, if any.
    ///
    /// # Arguments
    ///
    /// * `cwd` - The project root.
    ///
    /// # Returns
    ///
    /// The command, or `None` when the model must verify by hand.
    pub fn resolve(&self, cwd: &Path) -> Option<String> {
        match self {
            CheckCommand::Detect => detect(cwd),
            CheckCommand::Command(command) => Some(command.clone()),
            CheckCommand::None => None,
        }
    }
}

/// Picks the check command from the project files.
///
/// Order: a `justfile` with a `check` recipe, then `Cargo.toml`
/// (`cargo test`), `pyproject.toml` (`pytest`), `package.json` (`npm test`).
///
/// # Arguments
///
/// * `cwd` - The project root.
///
/// # Returns
///
/// The command line, or `None` when nothing is recognized.
pub fn detect(cwd: &Path) -> Option<String> {
    if justfile_has_check(cwd) {
        return Some("just check".to_owned());
    }
    let candidates = [
        ("Cargo.toml", "cargo test"),
        ("pyproject.toml", "pytest"),
        ("package.json", "npm test"),
    ];
    candidates
        .into_iter()
        .find(|(file, _)| cwd.join(file).is_file())
        .map(|(_, command)| command.to_owned())
}

/// Whether a `justfile` or `Justfile` defines a `check` recipe.
fn justfile_has_check(cwd: &Path) -> bool {
    ["justfile", "Justfile"]
        .iter()
        .filter_map(|name| std::fs::read_to_string(cwd.join(name)).ok())
        .any(|text| {
            text.lines().any(|line| {
                let line = line.trim_end();
                line.starts_with("check:") || line.starts_with("check ")
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_in_documented_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(detect(dir.path()), None);
        std::fs::write(dir.path().join("package.json"), "{}").expect("write");
        assert_eq!(detect(dir.path()).as_deref(), Some("npm test"));
        std::fs::write(dir.path().join("pyproject.toml"), "").expect("write");
        assert_eq!(detect(dir.path()).as_deref(), Some("pytest"));
        std::fs::write(dir.path().join("Cargo.toml"), "").expect("write");
        assert_eq!(detect(dir.path()).as_deref(), Some("cargo test"));
        std::fs::write(dir.path().join("justfile"), "fmt:\n    cargo fmt\n").expect("write");
        assert_eq!(
            detect(dir.path()).as_deref(),
            Some("cargo test"),
            "no check recipe"
        );
        std::fs::write(dir.path().join("justfile"), "check:\n    cargo test\n").expect("write");
        assert_eq!(detect(dir.path()).as_deref(), Some("just check"));
    }

    #[test]
    fn resolve_honors_overrides() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("Cargo.toml"), "").expect("write");
        assert_eq!(
            CheckCommand::Detect.resolve(dir.path()).as_deref(),
            Some("cargo test")
        );
        assert_eq!(
            CheckCommand::Command("make".to_owned())
                .resolve(dir.path())
                .as_deref(),
            Some("make")
        );
        assert_eq!(CheckCommand::None.resolve(dir.path()), None);
    }
}
