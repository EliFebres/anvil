//! Runs the docstring check script on fixture files, directly and through
//! the seeded `google-docstrings` rule.

use std::path::{Path, PathBuf};
use std::process::Command;

use forge_memory::paths;
use forge_memory::rules::{RuleSet, ensure_check_scripts};
use forge_memory::run_checks;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

fn script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("checks")
        .join("docstrings.py")
}

/// The python program that runs here, or `None` when there is none.
fn python() -> Option<&'static str> {
    ["python", "python3"].into_iter().find(|program| {
        Command::new(program)
            .arg("--version")
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false)
    })
}

fn run_script(python: &str, files: &[PathBuf]) -> (bool, String) {
    let out = Command::new(python)
        .arg(script())
        .args(files)
        .output()
        .expect("python runs");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

#[test]
fn documented_fixtures_pass_in_both_languages() {
    let Some(python) = python() else {
        eprintln!("python not available; skipping");
        return;
    };
    let (passed, stdout) = run_script(
        python,
        &[fixture("documented.py"), fixture("documented.rs")],
    );
    assert!(passed, "{stdout}");
    assert!(stdout.trim().is_empty(), "{stdout}");
}

#[test]
fn undocumented_fixtures_are_reported_with_lines() {
    let Some(python) = python() else {
        eprintln!("python not available; skipping");
        return;
    };
    let (passed, stdout) = run_script(python, &[fixture("undocumented.py")]);
    assert!(!passed);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 3, "{stdout}");
    assert!(
        lines[0].ends_with(":4: public function `add` has no docstring"),
        "{stdout}"
    );
    assert!(
        lines[1].ends_with(":8: public class `Counter` has no docstring"),
        "{stdout}"
    );
    assert!(
        lines[2].ends_with(":9: public function `bump` has no docstring"),
        "{stdout}"
    );
    assert!(!stdout.contains("_hidden"));

    let (passed, stdout) = run_script(python, &[fixture("undocumented.rs")]);
    assert!(!passed);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 3, "{stdout}");
    assert!(
        lines[0].ends_with(":3: public fn `add` has no doc comment"),
        "{stdout}"
    );
    assert!(
        lines[1].ends_with(":8: public struct `Counter` has no doc comment"),
        "{stdout}"
    );
    assert!(
        lines[2].ends_with(":13: public fn `bump` has no doc comment"),
        "{stdout}"
    );
    assert!(!stdout.contains("hidden"));
}

#[test]
fn seeded_rule_runs_the_installed_script_on_changed_files() {
    let Some(_) = python() else {
        eprintln!("python not available; skipping");
        return;
    };
    let home = tempfile::tempdir().expect("home");
    ensure_check_scripts(&paths::checks_dir(home.path())).expect("installs");
    let rules = RuleSet::seed();
    let cwd = fixture("").parent().expect("fixtures dir").to_path_buf();

    let failures = run_checks(
        &rules,
        &cwd,
        Some(home.path()),
        &[fixture("undocumented.py"), fixture("documented.rs")],
    );
    assert_eq!(failures.len(), 1, "{failures:?}");
    assert_eq!(failures[0].rule_id, "google-docstrings");
    assert!(
        failures[0].output.contains("undocumented.py:4"),
        "{}",
        failures[0].output
    );
    assert!(
        !failures[0].output.contains("documented.rs:"),
        "{}",
        failures[0].output
    );

    let failures = run_checks(
        &rules,
        &cwd,
        Some(home.path()),
        &[fixture("documented.py"), fixture("documented.rs")],
    );
    assert!(failures.is_empty(), "{failures:?}");
}

#[test]
fn rules_without_matching_files_are_skipped_without_python() {
    let rules = RuleSet::seed();
    let failures = run_checks(&rules, Path::new("."), None, &[fixture("not-there.toml")]);
    assert!(failures.is_empty(), "{failures:?}");
}
