//! Every program anvil starts begins from a cleared environment refilled
//! from `anvil_types::child_env::vars()`, so a provider key in the user's
//! shell never reaches it. This test fails when non-test code builds a
//! `Command` without `env_clear` close by.

use std::path::{Path, PathBuf};

/// Text that builds a process, for `std::process` and `tokio::process`.
const SPAWN: &str = "Command::new(";

/// How many lines after `Command::new(` may hold its `env_clear()`: one
/// builder chain, or a `match` whose arms build the command and whose end
/// clears it.
const WINDOW: usize = 12;

/// Reviewed places that build a command without `env_clear` nearby, as
/// (file, trimmed line, reason). Empty today; an entry needs a reason a
/// reviewer can check.
const EXCEPTIONS: [(&str, &str, &str); 0] = [];

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

/// Places in `code` that build a command without clearing its
/// environment within [`WINDOW`] lines, as `file:line: text`.
fn uncleared(relative: &str, code: &str) -> Vec<String> {
    let lines: Vec<&str> = code.lines().collect();
    let mut found = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if !line.contains(SPAWN) || line.trim_start().starts_with("//") {
            continue;
        }
        let end = (index + WINDOW + 1).min(lines.len());
        if lines[index..end]
            .iter()
            .any(|line| line.contains("env_clear()"))
        {
            continue;
        }
        let trimmed = line.trim();
        let excepted = EXCEPTIONS
            .iter()
            .any(|(file, text, _)| *file == relative && *text == trimmed);
        if !excepted {
            found.push(format!("{relative}:{}: {trimmed}", index + 1));
        }
    }
    found
}

#[test]
fn every_non_test_command_starts_from_a_cleared_environment() {
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
        if relative.ends_with("/tests.rs") || relative.ends_with("_tests.rs") {
            continue;
        }
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        // Test modules start programs freely to build fixtures.
        let code = text.split("#[cfg(test)]").next().unwrap_or_default();
        offenders.extend(uncleared(&relative, code));
    }
    assert!(
        offenders.is_empty(),
        "start these with .env_clear().envs(anvil_types::child_env::vars()):\n{}",
        offenders.join("\n")
    );
}

#[test]
fn the_scan_catches_a_command_that_keeps_the_environment() {
    let bad = "fn f() {\n    let out = Command::new(\"icacls\").arg(p).output();\n}\n";
    assert_eq!(uncleared("x.rs", bad).len(), 1);
    let good = "fn f() {\n    let out = Command::new(\"icacls\")\n        .env_clear()\n        .output();\n}\n";
    assert!(uncleared("x.rs", good).is_empty());
}
