//! Language tags by file extension, and a bounded scan of a project for them.

use std::collections::BTreeSet;
use std::path::Path;

use ignore::WalkBuilder;

/// Files a project scan looks at before it stops.
pub const MAX_SCAN_FILES: usize = 5_000;

/// Directory levels a project scan descends.
pub const MAX_SCAN_DEPTH: usize = 8;

/// Language tag and the extensions (lowercase, no dot) that carry it.
const LANGUAGES: &[(&str, &[&str])] = &[
    ("python", &["py", "pyi"]),
    ("rust", &["rs"]),
    ("typescript", &["ts", "tsx"]),
    ("javascript", &["js", "jsx", "mjs", "cjs"]),
    ("go", &["go"]),
    ("java", &["java"]),
    ("ruby", &["rb"]),
    ("shell", &["sh", "bash"]),
    ("toml", &["toml"]),
    ("markdown", &["md"]),
];

/// Directories that are never scanned, even without a `.gitignore`.
const SKIPPED_DIRS: &[&str] = &["node_modules", "target", "dist"];

/// The language tag of a file, by extension.
///
/// # Arguments
///
/// * `path` - The file.
///
/// # Returns
///
/// A tag such as `python` or `rust`, or `None` for an unknown extension.
pub fn language_of(path: &Path) -> Option<&'static str> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    LANGUAGES
        .iter()
        .find(|(_, extensions)| extensions.contains(&extension.as_str()))
        .map(|(tag, _)| *tag)
}

/// Whether a tag is one a scan can ever detect.
///
/// # Arguments
///
/// * `tag` - A language tag from a rule or a template, any case.
///
/// # Returns
///
/// `true` when some extension maps to it.
pub fn is_detectable(tag: &str) -> bool {
    LANGUAGES
        .iter()
        .any(|(known, _)| known.eq_ignore_ascii_case(tag))
}

/// The languages present in a project, from the extensions of its files.
///
/// The walk honors `.gitignore` (also outside a git repository), skips
/// dot-directories, `node_modules`, `target` and `dist`, visits entries in
/// name order and stops after [`MAX_SCAN_FILES`] files or
/// [`MAX_SCAN_DEPTH`] levels, so the answer is cheap and repeatable.
///
/// # Arguments
///
/// * `root` - The project root.
///
/// # Returns
///
/// The tags found; empty for an empty or unreadable directory.
pub fn detect_languages(root: &Path) -> BTreeSet<&'static str> {
    let walker = WalkBuilder::new(root)
        .max_depth(Some(MAX_SCAN_DEPTH))
        .require_git(false)
        .sort_by_file_name(|a, b| a.cmp(b))
        .filter_entry(|entry| {
            entry.depth() == 0
                || !entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| SKIPPED_DIRS.contains(&name))
        })
        .build();
    let mut found = BTreeSet::new();
    let mut seen = 0;
    for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        seen += 1;
        if seen > MAX_SCAN_FILES {
            break;
        }
        if let Some(tag) = language_of(entry.path()) {
            found.insert(tag);
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(root: &Path, relative: &str) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, "x\n").expect("write");
    }

    #[test]
    fn extensions_map_to_tags() {
        assert_eq!(language_of(Path::new("a/b.PY")), Some("python"));
        assert_eq!(language_of(Path::new("lib.rs")), Some("rust"));
        assert_eq!(language_of(Path::new("Makefile")), None);
        assert!(is_detectable("Rust"));
        assert!(!is_detectable("cobol"));
    }

    #[test]
    fn scan_finds_languages_and_honors_gitignore() {
        let dir = tempfile::tempdir().expect("tempdir");
        touch(dir.path(), "src/main.rs");
        touch(dir.path(), "gen/out.py");
        touch(dir.path(), "node_modules/dep/index.js");
        touch(dir.path(), ".hidden/a.go");
        std::fs::write(dir.path().join(".gitignore"), "gen/\n").expect("write");
        let found: Vec<_> = detect_languages(dir.path()).into_iter().collect();
        assert_eq!(found, vec!["rust"]);
    }

    #[test]
    fn scan_is_bounded_by_file_count() {
        let dir = tempfile::tempdir().expect("tempdir");
        for index in 0..=MAX_SCAN_FILES {
            touch(dir.path(), &format!("a/f{index:05}.txt"));
        }
        touch(dir.path(), "z/late.py");
        assert!(detect_languages(dir.path()).is_empty());
    }

    #[test]
    fn an_empty_directory_has_no_languages() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(detect_languages(dir.path()).is_empty());
    }
}
