//! Every git command anvil runs in a project goes through
//! `anvil_core::git::command`, which stops the repository from choosing a
//! program for git to run. This test fails when non-test code starts git
//! any other way.

use std::path::{Path, PathBuf};

/// The one file allowed to start git directly: the safe entry point itself.
const ALLOWED: [&str; 1] = ["crates/anvil-core/src/git.rs"];

/// Text that starts git directly.
const FORBIDDEN: &str = "Command::new(\"git\")";

fn rust_files(dir: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            if path
                .file_name()
                .is_some_and(|name| name != "target" && name != "tests")
            {
                rust_files(&path, found);
            }
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            found.push(path);
        }
    }
}

#[test]
fn only_the_git_module_starts_git_in_a_project() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = Vec::new();
    rust_files(&root.join("crates"), &mut files);
    assert!(files.len() > 50, "found only {} Rust files", files.len());
    let mut offenders = Vec::new();
    for file in files {
        let relative = file
            .strip_prefix(&root)
            .unwrap_or(&file)
            .to_string_lossy()
            .replace('\\', "/");
        let test_file = relative.ends_with("/tests.rs") || relative.ends_with("_tests.rs");
        if test_file || ALLOWED.contains(&relative.as_str()) {
            continue;
        }
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        // Test modules start git freely to build fixtures.
        let code = text.split("#[cfg(test)]").next().unwrap_or_default();
        if code.contains(FORBIDDEN) {
            offenders.push(relative);
        }
    }
    assert!(
        offenders.is_empty(),
        "start git through anvil_core::git::command instead:\n{}",
        offenders.join("\n")
    );
}
