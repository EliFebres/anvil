//! Migration from the old name of this program.
//!
//! The agent used to be called `forge`, with `~/.forge/`, project `.forge/`
//! directories and `FORGE_*` variables. This is the only module allowed to
//! spell that name; `just no-old-name` excludes it and its test from the guard.

use std::fs;
use std::path::Path;

use anyhow::Context;

/// The old per-user and per-project directory name.
const OLD_DIR: &str = ".forge";
/// The current directory name.
const NEW_DIR: &str = ".anvil";
/// The prefix of the old environment variables.
const OLD_ENV_PREFIX: &str = "FORGE_";
/// The old home path as the seeded rules file spells it.
const OLD_HOME_TEXT: &str = "~/.forge/";
/// The current home path in the same spelling.
const NEW_HOME_TEXT: &str = "~/.anvil/";
/// The seeded rules file; its docstring check names the home path.
const RULES_FILE: &str = "rules.toml";

/// Moves `<parent>/.forge` to `<parent>/.anvil` when only the old one exists.
///
/// When both exist nothing moves. After a move, `rules.toml` inside the new
/// directory has its old home path rewritten, so the seeded docstring check
/// keeps working.
///
/// # Arguments
///
/// * `parent` - The home or the project directory.
/// * `prefix` - How `parent` is shown to the user: `~/` for the home, empty
///   for the project.
///
/// # Returns
///
/// One line to show the user, or `None` when there was nothing to do.
///
/// # Errors
///
/// When the rename or the rules rewrite fails. Going on would create an
/// empty new directory and silently hide the old configuration.
pub fn migrate(parent: &Path, prefix: &str) -> anyhow::Result<Option<String>> {
    let old = parent.join(OLD_DIR);
    let new = parent.join(NEW_DIR);
    let old_shown = format!("{prefix}{OLD_DIR}");
    let new_shown = format!("{prefix}{NEW_DIR}");
    if !old.is_dir() {
        return Ok(None);
    }
    if new.exists() {
        return Ok(Some(format!(
            "warning: both {old_shown} and {new_shown} exist; nothing was moved. \
             Merge what you need from {old_shown} into {new_shown}, then delete it."
        )));
    }
    fs::rename(&old, &new)
        .with_context(|| format!("could not move {} to {}", old.display(), new.display()))?;
    rewrite_rules(&new.join(RULES_FILE))?;
    Ok(Some(format!("moved {old_shown} to {new_shown}")))
}

/// Replaces the old home path inside a rules file, when the file exists.
fn rewrite_rules(path: &Path) -> anyhow::Result<()> {
    let Ok(text) = fs::read_to_string(path) else {
        return Ok(());
    };
    if !text.contains(OLD_HOME_TEXT) {
        return Ok(());
    }
    fs::write(path, text.replace(OLD_HOME_TEXT, NEW_HOME_TEXT))
        .with_context(|| format!("could not rewrite {}", path.display()))
}

/// What still carries the old name: directories and environment variables.
///
/// # Arguments
///
/// * `home` - The home directory, if known.
/// * `cwd` - The project directory.
///
/// # Returns
///
/// `~/.forge`, `.forge` and every `FORGE_*` variable that is present, in
/// that order with the variables sorted. Empty when nothing is left.
pub fn leftovers(home: Option<&Path>, cwd: &Path) -> Vec<String> {
    let mut found = Vec::new();
    if home.is_some_and(|home| home.join(OLD_DIR).is_dir()) {
        found.push(format!("~/{OLD_DIR}"));
    }
    if cwd.join(OLD_DIR).is_dir() {
        found.push(OLD_DIR.to_owned());
    }
    let names = std::env::vars_os().filter_map(|(name, _)| name.into_string().ok());
    found.extend(old_variables(names));
    found
}

/// The names with the old prefix, sorted.
fn old_variables(names: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut old: Vec<String> = names
        .into_iter()
        .filter(|name| name.starts_with(OLD_ENV_PREFIX))
        .collect();
    old.sort();
    old
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(path, text).expect("write");
    }

    #[test]
    fn an_old_directory_alone_is_moved_and_its_rules_rewritten() {
        let dir = tempfile::tempdir().expect("tempdir");
        let old = dir.path().join(OLD_DIR);
        write(&old.join("config.toml"), "# config\n");
        write(
            &old.join(RULES_FILE),
            "check = { command = \"python ~/.forge/checks/docstrings.py\" }\n",
        );

        let line = migrate(dir.path(), "~/").expect("migrates");

        assert_eq!(line.as_deref(), Some("moved ~/.forge to ~/.anvil"));
        assert!(!old.exists());
        let new = dir.path().join(NEW_DIR);
        assert!(new.join("config.toml").is_file());
        let rules = fs::read_to_string(new.join(RULES_FILE)).expect("rules");
        assert_eq!(
            rules,
            "check = { command = \"python ~/.anvil/checks/docstrings.py\" }\n"
        );
    }

    #[test]
    fn both_directories_present_moves_nothing_and_warns_with_both_paths() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(&dir.path().join(OLD_DIR).join("a"), "old");
        write(&dir.path().join(NEW_DIR).join("b"), "new");

        let line = migrate(dir.path(), "").expect("migrates");

        let line = line.expect("a warning");
        assert!(
            line.starts_with("warning: both .forge and .anvil exist"),
            "{line}"
        );
        assert!(dir.path().join(OLD_DIR).join("a").is_file());
        assert!(dir.path().join(NEW_DIR).join("b").is_file());
        assert!(!dir.path().join(NEW_DIR).join("a").exists());
    }

    #[test]
    fn nothing_to_migrate_is_silent() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(migrate(dir.path(), "~/").expect("migrates"), None);
        write(&dir.path().join(NEW_DIR).join("config.toml"), "");
        assert_eq!(migrate(dir.path(), "~/").expect("migrates"), None);
    }

    #[test]
    fn leftovers_name_the_old_directories() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join("home");
        let project = dir.path().join("project");
        fs::create_dir_all(home.join(OLD_DIR)).expect("mkdir");
        fs::create_dir_all(project.join(OLD_DIR)).expect("mkdir");

        let found = leftovers(Some(&home), &project);

        assert!(
            found.starts_with(&["~/.forge".to_owned(), ".forge".to_owned()]),
            "{found:?}"
        );
        assert!(
            leftovers(None, dir.path())
                .iter()
                .all(|name| name.starts_with(OLD_ENV_PREFIX))
        );
    }

    #[test]
    fn old_variables_keep_only_the_old_prefix_sorted() {
        let names = ["PATH", "FORGE_LOG", "ANVIL_HOME", "FORGE_HOME"].map(str::to_owned);
        assert_eq!(old_variables(names), ["FORGE_HOME", "FORGE_LOG"]);
    }
}
