//! The run loop over the three fixture tasks with a scripted runner.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use eval::{
    AttemptContext, AttemptFuture, AttemptOutcome, AttemptStatus, EvalError, EvalRunner,
    RunOptions, Usage, run_with,
};

/// The three fixture tasks next to this test.
fn tasks_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tasks")
}

/// A runner that follows a script: per task, one outcome per attempt index.
struct FakeRunner {
    script: HashMap<&'static str, Vec<bool>>,
    panic: bool,
}

impl FakeRunner {
    fn scripted(entries: &[(&'static str, &[bool])]) -> Arc<dyn EvalRunner> {
        Arc::new(FakeRunner {
            script: entries
                .iter()
                .map(|(name, outcomes)| (*name, outcomes.to_vec()))
                .collect(),
            panic: false,
        })
    }
}

impl EvalRunner for FakeRunner {
    fn label(&self) -> String {
        "fake".to_owned()
    }

    fn run_attempt<'a>(&'a self, attempt: &'a AttemptContext<'a>) -> AttemptFuture<'a> {
        Box::pin(async move {
            if self.panic {
                panic!("scripted panic");
            }
            let succeed = self
                .script
                .get(attempt.task.name.as_str())
                .and_then(|outcomes| outcomes.get(attempt.index as usize - 1))
                .copied()
                .unwrap_or(false);
            assert!(
                attempt.work_dir.join(".git").is_dir(),
                "the fixture copy is a git repository"
            );
            assert!(
                attempt.env.iter().any(|(k, _)| k == "CARGO_TARGET_DIR"),
                "the shared environment reaches the runner"
            );
            if succeed {
                std::fs::write(attempt.work_dir.join("DONE"), "done\n")
                    .map_err(|e| EvalError::io("writing", attempt.work_dir, e))?;
                std::fs::write(attempt.work_dir.join("notes.md"), "a marker here\n")
                    .map_err(|e| EvalError::io("writing", attempt.work_dir, e))?;
            }
            Ok(AttemptOutcome {
                status: if succeed {
                    AttemptStatus::Done
                } else {
                    AttemptStatus::Incomplete {
                        reason: "scripted".to_owned(),
                    }
                },
                session: Some("01ARZ3NDEKTSV4RRFFQ69G5FAV".to_owned()),
                provider: Some("fake".to_owned()),
                model: Some("fake-1".to_owned()),
                tokens: Usage {
                    input: 1000,
                    output: 100,
                    cached: 0,
                },
                cost_usd: Some(0.0012),
                anvil_ms: 10,
                stderr_tail: "done".to_owned(),
            })
        })
    }
}

fn options(tags: &[&str], repeat: u32) -> RunOptions {
    RunOptions {
        repo_root: PathBuf::from(env!("CARGO_MANIFEST_DIR")),
        tasks_dir: tasks_dir(),
        tags: tags.iter().map(|t| (*t).to_owned()).collect(),
        repeat,
        parallel: 3,
        provider: Some("fake".to_owned()),
        progress: None,
    }
}

#[tokio::test]
async fn three_repeats_give_majorities_totals_and_spread() {
    let runner = FakeRunner::scripted(&[
        ("pass-when-done", &[true, true, true]),
        ("greps-marker", &[true, false, true]),
        ("always-fails", &[true, true, true]),
    ]);
    let result = run_with(&options(&[], 3), runner).await.expect("runs");

    let names: Vec<&str> = result.tasks.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["always-fails", "greps-marker", "pass-when-done"]);
    assert!(result.task("pass-when-done").expect("task").is_passing());
    assert!(result.task("greps-marker").expect("task").is_passing());
    let failing = result.task("always-fails").expect("task");
    assert!(!failing.is_passing());
    assert!(failing.attempts.iter().all(|a| a.check_exit == Some(3)));
    assert!(
        failing
            .attempts
            .iter()
            .all(|a| a.status == AttemptStatus::Done)
    );
    assert!(failing.attempts[0].check_tail.contains("the check decides"));

    let totals = &result.totals;
    assert_eq!(totals.tasks, 3);
    assert_eq!(totals.attempts, 9);
    assert_eq!(totals.passed_attempts, 5);
    assert_eq!(totals.tasks_passing, 2);
    assert!((totals.pass_rate_min - 1.0 / 3.0).abs() < 1e-9);
    assert!((totals.pass_rate_max - 2.0 / 3.0).abs() < 1e-9);
    assert_eq!(totals.tokens.total(), 9 * 1100);
    assert!((totals.cost_usd.expect("priced") - 9.0 * 0.0012).abs() < 1e-9);
    assert!((totals.cost_per_passed_task.expect("priced") - 9.0 * 0.0012 / 2.0).abs() < 1e-9);

    assert_eq!(result.runner, "fake");
    assert_eq!(result.model.as_deref(), Some("fake-1"));
    assert_eq!(result.provider.as_deref(), Some("fake"));
    assert_eq!(result.repeat, 3);
    assert_ne!(result.git_sha, "unknown");
    let attempts = &result.task("greps-marker").expect("task").attempts;
    assert_eq!(
        attempts.iter().map(|a| a.index).collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert_eq!(
        attempts.iter().map(|a| a.passed).collect::<Vec<_>>(),
        [true, false, true]
    );
}

#[tokio::test]
async fn tags_filter_tasks_and_one_of_two_is_not_a_pass() {
    let runner = FakeRunner::scripted(&[("pass-when-done", &[true, false])]);
    let result = run_with(&options(&["fast"], 2), runner)
        .await
        .expect("runs");
    let names: Vec<&str> = result.tasks.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["greps-marker", "pass-when-done"]);
    assert!(!result.task("pass-when-done").expect("task").is_passing());
    assert_eq!(result.totals.tasks_passing, 0);
    assert_eq!(result.tags, vec!["fast"]);
}

#[tokio::test]
async fn progress_is_reported_once_per_attempt() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let mut options = options(&["fast"], 2);
    options.progress = Some(Arc::new(move |progress: &eval::Progress| {
        sink.lock()
            .expect("lock")
            .push((progress.finished, progress.total, progress.task.clone()));
    }));
    let runner = FakeRunner::scripted(&[("pass-when-done", &[true, true])]);
    run_with(&options, runner).await.expect("runs");
    let seen = seen.lock().expect("lock");
    assert_eq!(seen.len(), 4);
    assert!(seen.iter().all(|(_, total, _)| *total == 4));
    assert_eq!(seen.iter().map(|(n, _, _)| *n).max(), Some(4));
}

#[tokio::test]
async fn a_panicking_runner_is_a_harness_error_not_a_crash() {
    let runner: Arc<dyn EvalRunner> = Arc::new(FakeRunner {
        script: HashMap::new(),
        panic: true,
    });
    let result = run_with(&options(&["fast"], 1), runner)
        .await
        .expect("runs");
    for task in &result.tasks {
        assert_eq!(task.attempts.len(), 1);
        assert!(
            matches!(&task.attempts[0].status, AttemptStatus::HarnessError { message } if message.contains("panic")),
            "{:?}",
            task.attempts[0].status
        );
        assert!(!task.attempts[0].passed);
    }
}

#[tokio::test]
async fn zero_options_and_unknown_tags_are_errors() {
    let runner = FakeRunner::scripted(&[]);
    assert!(matches!(
        run_with(&options(&[], 0), Arc::clone(&runner)).await,
        Err(EvalError::ZeroOption { option: "repeat" })
    ));
    let mut no_parallel = options(&[], 1);
    no_parallel.parallel = 0;
    assert!(matches!(
        run_with(&no_parallel, Arc::clone(&runner)).await,
        Err(EvalError::ZeroOption { option: "parallel" })
    ));
    assert!(matches!(
        run_with(&options(&["nope"], 1), runner).await,
        Err(EvalError::NoTasks { .. })
    ));
}
