//! Finds the instruction files that belong in the system prompt.
//!
//! Order: `~/.forge/FORGE.md`, then the repo's `FORGE.md` (or `CLAUDE.md`
//! when there is no `FORGE.md`), then any `CLAUDE.md` in a directory
//! between the repo root and a file touched this session, shallowest first.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::agent::Instruction;

/// The global instruction file, relative to the home directory.
pub const GLOBAL_FILE: &str = ".forge/FORGE.md";

/// Discovers instruction files.
///
/// # Arguments
///
/// * `home` - The user's home directory, if known.
/// * `cwd` - The project root.
/// * `touched` - Files read or written this session, absolute or relative to `cwd`.
///
/// # Returns
///
/// The instructions in prompt order. Unreadable files are skipped.
pub fn discover_instructions(
    home: Option<&Path>,
    cwd: &Path,
    touched: &BTreeSet<PathBuf>,
) -> Vec<Instruction> {
    let mut found = Vec::new();
    if let Some(home) = home {
        push_if_present(&mut found, &home.join(GLOBAL_FILE), "~/.forge/FORGE.md");
    }
    if cwd.join("FORGE.md").is_file() {
        push_if_present(&mut found, &cwd.join("FORGE.md"), "FORGE.md");
    } else {
        push_if_present(&mut found, &cwd.join("CLAUDE.md"), "CLAUDE.md");
    }
    for dir in parent_dirs(cwd, touched) {
        let path = dir.join("CLAUDE.md");
        let source = relative(&path, cwd);
        push_if_present(&mut found, &path, &source);
    }
    found
}

/// Directories strictly between `cwd` and each touched file, deduplicated
/// and ordered by depth then path.
fn parent_dirs(cwd: &Path, touched: &BTreeSet<PathBuf>) -> Vec<PathBuf> {
    let mut dirs = BTreeSet::new();
    for file in touched {
        let absolute = if file.is_absolute() {
            file.clone()
        } else {
            cwd.join(file)
        };
        let Ok(relative) = absolute.strip_prefix(cwd) else {
            continue;
        };
        let mut dir = cwd.to_path_buf();
        let components: Vec<_> = relative.components().collect();
        for component in components.iter().take(components.len().saturating_sub(1)) {
            dir.push(component);
            dirs.insert((dir.components().count(), dir.clone()));
        }
    }
    dirs.into_iter().map(|(_, dir)| dir).collect()
}

/// Reads the file into the list when it exists.
fn push_if_present(found: &mut Vec<Instruction>, path: &Path, source: &str) {
    if !path.is_file() {
        return;
    }
    match std::fs::read_to_string(path) {
        Ok(text) if !text.trim().is_empty() => found.push(Instruction {
            source: source.to_owned(),
            text,
        }),
        Ok(_) => {}
        Err(error) => tracing::debug!(path = %path.display(), %error, "skipping instruction file"),
    }
}

/// The path relative to `cwd` with forward slashes.
fn relative(path: &Path, cwd: &Path) -> String {
    path.strip_prefix(cwd)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, text: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, text).expect("write");
    }

    #[test]
    fn discovers_in_prompt_order_and_dedupes() {
        let home = tempfile::tempdir().expect("home");
        let repo = tempfile::tempdir().expect("repo");
        write(home.path(), ".forge/FORGE.md", "global rules");
        write(repo.path(), "CLAUDE.md", "repo rules");
        write(repo.path(), "crates/CLAUDE.md", "crates rules");
        write(repo.path(), "crates/a/CLAUDE.md", "a rules");
        write(repo.path(), "crates/a/empty.md", "");
        let touched: BTreeSet<PathBuf> = [
            PathBuf::from("crates/a/src/lib.rs"),
            repo.path().join("crates/a/src/main.rs"),
            PathBuf::from("crates/b/x.rs"),
            PathBuf::from("/elsewhere/z.rs"),
        ]
        .into_iter()
        .collect();
        let found = discover_instructions(Some(home.path()), repo.path(), &touched);
        let sources: Vec<&str> = found.iter().map(|i| i.source.as_str()).collect();
        assert_eq!(
            sources,
            vec![
                "~/.forge/FORGE.md",
                "CLAUDE.md",
                "crates/CLAUDE.md",
                "crates/a/CLAUDE.md"
            ]
        );
        assert_eq!(found[3].text, "a rules");
    }

    #[test]
    fn forge_md_replaces_claude_md_and_empty_files_are_skipped() {
        let repo = tempfile::tempdir().expect("repo");
        write(repo.path(), "CLAUDE.md", "claude");
        write(repo.path(), "FORGE.md", "forge");
        let found = discover_instructions(None, repo.path(), &BTreeSet::new());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].source, "FORGE.md");

        write(repo.path(), "FORGE.md", "   \n");
        let found = discover_instructions(None, repo.path(), &BTreeSet::new());
        assert!(
            found.is_empty(),
            "an empty FORGE.md adds nothing and hides CLAUDE.md"
        );
    }

    #[test]
    fn nothing_found_is_fine() {
        let repo = tempfile::tempdir().expect("repo");
        assert!(discover_instructions(None, repo.path(), &BTreeSet::new()).is_empty());
    }
}
