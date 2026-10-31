//! The committed task set under `eval/tasks/`.
//!
//! The fast test checks every task loads and the language mix is what the
//! README promises. The ignored test proves each check both ways: it fails
//! on the untouched fixture and passes once the task's `solution/` overlay
//! is applied. Run it with `just eval-tasks`; the Rust tasks compile.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use eval::check::{find_bash, find_python, forward_slashes, run_check};
use eval::{Task, load_tasks};

/// `eval/tasks` at the repository root.
fn tasks_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("eval")
        .join("tasks")
}

#[test]
fn twenty_tasks_load_with_the_promised_language_mix() {
    let tasks = load_tasks(&tasks_dir(), &[]).expect("every task loads");
    assert_eq!(tasks.len(), 20, "{:?}", names(&tasks));
    let mut by_language: BTreeMap<&str, usize> = BTreeMap::new();
    for task in &tasks {
        let language = task
            .meta
            .tags
            .first()
            .expect("the first tag is the language");
        *by_language.entry(language.as_str()).or_default() += 1;
        let prefix = match language.as_str() {
            "python" => "py-",
            "rust" => "rs-",
            "shell" => "sh-",
            "markdown" => "md-",
            other => panic!("{}: unknown language tag {other}", task.name),
        };
        assert!(
            task.name.starts_with(prefix),
            "{} should start with {prefix}",
            task.name
        );
        assert!(
            task.dir.join("solution").is_dir(),
            "{} has no solution/ overlay",
            task.name
        );
    }
    assert_eq!(by_language["python"], 10);
    assert_eq!(by_language["rust"], 7);
    assert_eq!(by_language["shell"], 2);
    assert_eq!(by_language["markdown"], 1);
}

#[tokio::test]
#[ignore = "runs every check.sh twice, compiling the Rust fixtures; `just eval-tasks`"]
async fn every_check_fails_on_the_fixture_and_passes_on_the_solution() {
    let tasks = load_tasks(&tasks_dir(), &[]).expect("every task loads");
    let bash = find_bash().expect("bash");
    let python = find_python().expect("python");
    let root = tempfile::tempdir().expect("tmp");
    let mut failures = Vec::new();
    for task in &tasks {
        // Each copy gets its own target folder, as the harness does: cargo
        // would otherwise reuse the raw copy's binary for the solved copy.
        let env = |copy: &str| {
            vec![
                ("EVAL_TASK_DIR".to_owned(), forward_slashes(&task.dir)),
                ("EVAL_PYTHON".to_owned(), forward_slashes(&python)),
                (
                    "CARGO_TARGET_DIR".to_owned(),
                    forward_slashes(&root.path().join("cargo-target").join(copy)),
                ),
            ]
        };
        let work = root.path().join(&task.name);
        eval::task::copy_fixture(task, &work).expect("copy");
        let raw = run_check(
            &bash,
            &task.check_script(),
            &work,
            task.timeout(),
            &env(&task.name),
        )
        .await
        .expect("check runs");
        if raw.passed {
            failures.push(format!(
                "{}: the check passes on the raw fixture",
                task.name
            ));
        }
        let solved_name = format!("{}-solved", task.name);
        let work = root.path().join(&solved_name);
        eval::task::copy_fixture(task, &work).expect("copy");
        overlay(&task.dir.join("solution"), &work);
        let solved = run_check(
            &bash,
            &task.check_script(),
            &work,
            task.timeout(),
            &env(&solved_name),
        )
        .await
        .expect("check runs");
        if !solved.passed {
            failures.push(format!(
                "{}: the check fails on the solution:\n{}",
                task.name, solved.output_tail
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

fn names(tasks: &[Task]) -> Vec<&str> {
    tasks.iter().map(|t| t.name.as_str()).collect()
}

/// Copies every file under `from` over `into`.
fn overlay(from: &Path, into: &Path) {
    for entry in std::fs::read_dir(from).expect("solution dir") {
        let entry = entry.expect("entry");
        let dest = into.join(entry.file_name());
        if entry.path().is_dir() {
            std::fs::create_dir_all(&dest).expect("dir");
            overlay(&entry.path(), &dest);
        } else {
            std::fs::copy(entry.path(), dest).expect("copy");
        }
    }
}
