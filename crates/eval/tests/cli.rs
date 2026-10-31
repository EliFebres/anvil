//! The `eval` binary: `compare` and `gate` over written result files.

use std::path::Path;
use std::process::Command;

use eval::{AttemptRecord, AttemptStatus, RunResult, SCHEMA_VERSION, TaskResult, Totals, Usage};

fn eval_bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_eval"))
}

fn attempt(index: u32, passed: bool) -> AttemptRecord {
    AttemptRecord {
        index,
        passed,
        status: AttemptStatus::Done,
        check_exit: Some(if passed { 0 } else { 1 }),
        session: None,
        provider: Some("fake".to_owned()),
        model: Some("fake-1".to_owned()),
        tokens: Usage {
            input: 10,
            output: 1,
            cached: 0,
        },
        cost_usd: Some(0.001),
        anvil_ms: 1,
        check_ms: 1,
        wall_ms: 2,
        stderr_tail: String::new(),
        check_tail: String::new(),
    }
}

fn result(sha: &str, outcomes: &[(&str, &[bool])]) -> RunResult {
    let tasks: Vec<TaskResult> = outcomes
        .iter()
        .map(|(name, passes)| TaskResult {
            name: (*name).to_owned(),
            tags: vec!["fixture".to_owned()],
            attempts: passes
                .iter()
                .enumerate()
                .map(|(i, &p)| attempt(i as u32 + 1, p))
                .collect(),
        })
        .collect();
    let totals = Totals::compute(&tasks, 3, 100);
    RunResult {
        schema: SCHEMA_VERSION,
        started_at: "2026-10-30T10:00:00Z".parse().expect("time"),
        finished_at: "2026-10-30T10:01:00Z".parse().expect("time"),
        git_sha: sha.to_owned(),
        provider: Some("fake".to_owned()),
        model: Some("fake-1".to_owned()),
        runner: "fake".to_owned(),
        repeat: 3,
        parallel: 1,
        tags: Vec::new(),
        tasks,
        totals,
    }
}

fn write(dir: &Path, result: &RunResult) -> String {
    result
        .write(dir)
        .expect("writes")
        .to_string_lossy()
        .into_owned()
}

const BASE: &[(&str, &[bool])] = &[
    ("always-fails", &[false, false, false]),
    ("greps-marker", &[true, true, true]),
    ("pass-when-done", &[true, true, false]),
];

#[test]
fn gate_passes_at_one_regression_and_fails_at_two() {
    let dir = tempfile::tempdir().expect("tmp");
    let base = write(dir.path(), &result("base000", BASE));
    let one = write(
        dir.path(),
        &result(
            "one0000",
            &[
                ("always-fails", &[false, false, false]),
                ("greps-marker", &[true, true, true]),
                ("pass-when-done", &[false, false, true]),
            ],
        ),
    );
    let two = write(
        dir.path(),
        &result(
            "two0000",
            &[
                ("always-fails", &[true, true, true]),
                ("greps-marker", &[false, true, false]),
                ("pass-when-done", &[false, false, false]),
            ],
        ),
    );

    let out = eval_bin()
        .args(["gate", "--base", &base, "--candidate", &one])
        .output()
        .expect("runs");
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.starts_with("gate: PASS (1 regression(s)"), "{text}");
    assert!(text.contains("regressed: pass-when-done"), "{text}");

    let out = eval_bin()
        .args(["gate", "--base", &base, "--candidate", &two])
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(2));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.starts_with("gate: FAIL (2 regression(s)"), "{text}");
    assert!(
        text.contains("regressed: greps-marker, pass-when-done"),
        "{text}"
    );
    assert!(text.contains("fixed:     always-fails"), "{text}");
}

#[test]
fn compare_prints_the_table_and_flips() {
    let dir = tempfile::tempdir().expect("tmp");
    let a = write(dir.path(), &result("aaaaaaa", BASE));
    let b = write(
        dir.path(),
        &result(
            "bbbbbbb",
            &[
                ("always-fails", &[true, true, true]),
                ("greps-marker", &[true, true, true]),
                ("pass-when-done", &[false, false, false]),
            ],
        ),
    );
    let out = eval_bin().args(["compare", &a, &b]).output().expect("runs");
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("aaaaaaa fake-1"), "{text}");
    let fixed_row = text
        .lines()
        .find(|line| line.starts_with("always-fails"))
        .expect("a row for always-fails");
    assert!(fixed_row.contains("FAIL 0/3"), "{fixed_row}");
    assert!(fixed_row.contains("PASS 3/3"), "{fixed_row}");
    assert!(fixed_row.ends_with("fixed"), "{fixed_row}");
    assert!(text.contains("flipped: 1 fixed, 1 regressed"), "{text}");
    assert!(text.contains("cost per passed task"), "{text}");
}

#[test]
fn bad_inputs_exit_one_with_an_error_line() {
    let dir = tempfile::tempdir().expect("tmp");
    let bad = dir.path().join("bad.json");
    std::fs::write(&bad, "{").expect("write");
    let out = eval_bin()
        .args(["gate", "--base"])
        .arg(&bad)
        .arg("--candidate")
        .arg(&bad)
        .output()
        .expect("runs");
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.starts_with("error: parsing"), "{err}");

    let out = eval_bin().arg("--version").output().expect("runs");
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("eval "));
}
