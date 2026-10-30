//! Instruction files forge-core does not load itself, plus `@path` imports.
//!
//! forge-core already loads `~/.forge/FORGE.md`, the repo's `FORGE.md` or
//! `CLAUDE.md`, and nested `CLAUDE.md` files as they are touched. This
//! module adds what Claude Code also honors: `~/.claude/CLAUDE.md`,
//! `.claude/CLAUDE.md`, `CLAUDE.local.md`, `.claude/CLAUDE.local.md`, and
//! `@path` imports inside any of them, one level deep.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use forge_core::agent::Instruction;

/// Extra instruction files, relative to the home directory.
const HOME_FILES: &[&str] = &[".claude/CLAUDE.md"];

/// Extra instruction files, relative to the project root.
const PROJECT_FILES: &[&str] = &[
    ".claude/CLAUDE.md",
    "CLAUDE.local.md",
    ".claude/CLAUDE.local.md",
];

/// Finds the extra instruction files and the imports of every file.
///
/// # Arguments
///
/// * `home` - The home directory, if known.
/// * `cwd` - The project root.
/// * `base` - The instructions forge-core already loaded; their imports
///   are expanded, their text is not repeated.
///
/// # Returns
///
/// New instructions in prompt order: the extra files, then the imports of
/// the base files and of the extra files. A file is never loaded twice.
pub fn discover_extra(home: Option<&Path>, cwd: &Path, base: &[Instruction]) -> Vec<Instruction> {
    let mut loaded: BTreeSet<PathBuf> = base
        .iter()
        .filter_map(|instruction| source_path(&instruction.source, home, cwd))
        .map(|path| key(&path))
        .collect();
    let mut found = Vec::new();
    for (path, source) in extra_files(home, cwd) {
        if !loaded.insert(key(&path)) {
            continue;
        }
        if let Some(text) = read_non_empty(&path) {
            found.push(Instruction { source, text });
        }
    }
    let mut imports = Vec::new();
    for instruction in base.iter().chain(found.iter()) {
        let Some(path) = source_path(&instruction.source, home, cwd) else {
            continue;
        };
        let dir = path.parent().unwrap_or(cwd);
        for import in find_imports(&instruction.text) {
            let resolved = resolve_import(&import, dir, home);
            if !loaded.insert(key(&resolved)) {
                continue;
            }
            match read_non_empty(&resolved) {
                Some(text) => imports.push(Instruction {
                    source: format!("{import} (imported by {})", instruction.source),
                    text,
                }),
                None => tracing::debug!(
                    path = %resolved.display(),
                    from = %instruction.source,
                    "skipping an @import that is missing or empty"
                ),
            }
        }
    }
    found.extend(imports);
    found
}

/// The extra files with their prompt labels, in order.
fn extra_files(home: Option<&Path>, cwd: &Path) -> Vec<(PathBuf, String)> {
    let mut files = Vec::new();
    if let Some(home) = home {
        for name in HOME_FILES {
            files.push((home.join(name), format!("~/{name}")));
        }
    }
    for name in PROJECT_FILES {
        files.push((cwd.join(name), (*name).to_owned()));
    }
    files
}

/// The file behind an instruction's source label, when it names one.
///
/// Labels are `~/...` for home files and project-relative paths otherwise.
/// Import labels (`... (imported by ...)`) are not files and give `None`.
fn source_path(source: &str, home: Option<&Path>, cwd: &Path) -> Option<PathBuf> {
    if source.contains(" (imported by ") {
        return None;
    }
    match source.strip_prefix("~/") {
        Some(rest) => home.map(|home| home.join(rest)),
        None => Some(cwd.join(source)),
    }
}

/// Every `@path` token in the text, outside fenced code blocks.
///
/// A token counts when `@` starts a word, the rest has no spaces, and it
/// is not an e-mail address. Trailing punctuation is dropped.
pub(crate) fn find_imports(text: &str) -> Vec<String> {
    let mut imports = Vec::new();
    let mut in_fence = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        for word in line.split_whitespace() {
            let Some(path) = word.trim_start_matches(['(', '`']).strip_prefix('@') else {
                continue;
            };
            let path = path.trim_end_matches(['.', ',', ';', ':', ')', '`']);
            if path.is_empty() || path.contains('@') {
                continue;
            }
            imports.push(path.to_owned());
        }
    }
    imports
}

/// Resolves an import path against the importing file's directory.
fn resolve_import(import: &str, dir: &Path, home: Option<&Path>) -> PathBuf {
    if let Some(rest) = import.strip_prefix("~/") {
        return match home {
            Some(home) => home.join(rest),
            None => PathBuf::from(import),
        };
    }
    let path = Path::new(import);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        dir.join(path)
    }
}

/// The dedupe key for a path: its canonical form when it exists.
fn key(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// The file's text when it exists and is not blank.
fn read_non_empty(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    (!text.trim().is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        std::fs::write(path, text).expect("write");
    }

    fn base(source: &str, text: &str) -> Instruction {
        Instruction {
            source: source.to_owned(),
            text: text.to_owned(),
        }
    }

    #[test]
    fn loads_local_and_dot_claude_files_in_order() {
        let home = tempfile::tempdir().expect("tempdir");
        let project = tempfile::tempdir().expect("tempdir");
        write(&home.path().join(".claude/CLAUDE.md"), "home rules");
        write(
            &project.path().join(".claude/CLAUDE.md"),
            "dot claude rules",
        );
        write(&project.path().join("CLAUDE.local.md"), "local rules");
        write(&project.path().join(".claude/CLAUDE.local.md"), "   ");

        let extra = discover_extra(Some(home.path()), project.path(), &[]);
        let sources: Vec<&str> = extra.iter().map(|i| i.source.as_str()).collect();
        assert_eq!(
            sources,
            [
                "~/.claude/CLAUDE.md",
                ".claude/CLAUDE.md",
                "CLAUDE.local.md"
            ]
        );
        assert_eq!(extra[2].text, "local rules");
    }

    #[test]
    fn expands_imports_one_level_deep_relative_to_the_importing_file() {
        let home = tempfile::tempdir().expect("tempdir");
        let project = tempfile::tempdir().expect("tempdir");
        write(
            &project.path().join("docs/style.md"),
            "style: @docs/deeper.md",
        );
        write(&project.path().join("docs/deeper.md"), "too deep");
        write(&home.path().join("shared.md"), "shared rules");
        write(
            &project.path().join("crates/a/CLAUDE.md"),
            "See @README.md for the crate.",
        );
        write(&project.path().join("crates/a/README.md"), "crate readme");
        let claude = "Rules.\n\n@docs/style.md\nAlso read @~/shared.md.\n\n```\n@docs/in-fence.md\n```\nmail me@example.com\n@docs/missing.md\n";
        let loaded = [
            base("CLAUDE.md", claude),
            base("crates/a/CLAUDE.md", "See @README.md for the crate."),
        ];

        let extra = discover_extra(Some(home.path()), project.path(), &loaded);
        let sources: Vec<&str> = extra.iter().map(|i| i.source.as_str()).collect();
        assert_eq!(
            sources,
            [
                "docs/style.md (imported by CLAUDE.md)",
                "~/shared.md (imported by CLAUDE.md)",
                "README.md (imported by crates/a/CLAUDE.md)",
            ]
        );
        assert_eq!(extra[2].text, "crate readme");
        assert!(extra.iter().all(|i| i.text != "too deep"));
    }

    #[test]
    fn a_file_is_never_loaded_twice() {
        let project = tempfile::tempdir().expect("tempdir");
        write(&project.path().join("CLAUDE.local.md"), "local rules");
        let loaded = [base("CLAUDE.md", "@CLAUDE.local.md\n@CLAUDE.md")];
        let extra = discover_extra(None, project.path(), &loaded);
        assert_eq!(extra.len(), 1);
        assert_eq!(extra[0].source, "CLAUDE.local.md");
    }

    #[test]
    fn import_tokens_are_found_outside_fences_only() {
        let text = "@a.md, then (@b.md) and `@c.md`.\n~~~\n@no.md\n~~~\n@d/e.md:\nuser@host.md\n";
        assert_eq!(find_imports(text), ["a.md", "b.md", "c.md", "d/e.md"]);
    }
}
