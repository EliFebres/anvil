//! The real `AnvilRunner` driving a shell script that stands in for anvil.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use eval::{AnvilOptions, AnvilRunner, AttemptStatus, EvalError, RunOptions, run_with};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn runner(provider: Option<&str>) -> Result<AnvilRunner, EvalError> {
    AnvilRunner::new(
        AnvilOptions {
            anvil_bin: fixtures().join("fake-anvil.sh"),
            real_home: Some(fixtures().join("home")),
        },
        provider.map(str::to_owned),
    )
}

fn options(tags: &[&str]) -> RunOptions {
    RunOptions {
        repo_root: PathBuf::from(env!("CARGO_MANIFEST_DIR")),
        tasks_dir: fixtures().join("tasks"),
        tags: tags.iter().map(|t| (*t).to_owned()).collect(),
        repeat: 1,
        parallel: 2,
        provider: Some("fake".to_owned()),
        progress: None,
    }
}

#[tokio::test]
async fn runs_the_fake_in_an_isolated_home_and_prices_its_telemetry() {
    let runner = Arc::new(runner(Some("fake")).expect("runner"));
    let home = runner.home().to_owned();
    assert!(
        home.join(".anvil/config.toml").is_file(),
        "the config was seeded"
    );

    let result = run_with(
        &options(&["fast"]),
        Arc::clone(&runner) as Arc<dyn eval::EvalRunner>,
    )
    .await
    .expect("runs");

    let done = result.task("pass-when-done").expect("task");
    assert!(done.is_passing(), "{:?}", done.attempts[0]);
    let attempt = &done.attempts[0];
    assert_eq!(attempt.status, AttemptStatus::Done);
    assert_eq!(
        attempt.session.as_deref(),
        Some("01ARZ3NDEKTSV4RRFFQ69G5FAV")
    );
    assert_eq!(attempt.provider.as_deref(), Some("fake"));
    assert_eq!(attempt.model.as_deref(), Some("fake-1"));
    assert_eq!(attempt.tokens.total(), 1100);
    assert!((attempt.cost_usd.expect("priced") - 0.0012).abs() < 1e-12);
    assert!(attempt.stderr_tail.contains("done"));

    let marker = result.task("greps-marker").expect("task");
    assert!(!marker.is_passing(), "the fake never writes notes.md");
    assert_eq!(marker.attempts[0].status, AttemptStatus::Done);

    assert_eq!(result.model.as_deref(), Some("fake-1"));
    assert!(result.runner.ends_with("fake-anvil.sh"));

    let prompt = std::fs::read_to_string(home.join(".anvil/last-prompt.txt")).expect("prompt");
    assert!(
        prompt.contains("DONE") || prompt.contains("notes.md"),
        "{prompt}"
    );
    let project = std::fs::read_to_string(home.join(".anvil/last-project-config.toml"))
        .expect("project config");
    assert!(project.contains("coder = \"fake\""), "{project}");
    assert!(project.contains("session_tokens = 5000"), "{project}");

    assert!(
        home.join(".anvil/memory/touched").is_file(),
        "the fake wrote memory into the temporary home"
    );
    assert!(
        !fixtures().join("home/.anvil/memory").exists(),
        "the real home stayed clean"
    );
}

#[tokio::test]
async fn a_failing_fake_is_incomplete_with_its_reason() {
    let runner = Arc::new(runner(None).expect("runner"));
    std::fs::write(runner.home().join(".anvil/fail"), "1").expect("write");
    let result = run_with(&options(&["fast"]), runner).await.expect("runs");
    let done = result.task("pass-when-done").expect("task");
    assert!(!done.is_passing());
    assert_eq!(
        done.attempts[0].status,
        AttemptStatus::Incomplete {
            reason: "scripted".to_owned()
        }
    );
    assert_eq!(
        done.attempts[0].tokens.total(),
        1100,
        "tokens survive a failed run"
    );
}

#[tokio::test]
async fn a_slow_fake_times_out_and_keeps_the_tokens_it_recorded() {
    let runner = Arc::new(runner(None).expect("runner"));
    std::fs::write(runner.home().join(".anvil/sleep"), "20").expect("write");
    let result = run_with(&options(&["slow"]), runner).await.expect("runs");
    let slow = result.task("always-fails").expect("task");
    let attempt = &slow.attempts[0];
    assert_eq!(attempt.status, AttemptStatus::Timeout);
    assert!(
        attempt.anvil_ms < 15_000,
        "killed at the 1s timeout, took {}ms",
        attempt.anvil_ms
    );
    assert_eq!(attempt.tokens.total(), 1100);
    assert!(!attempt.passed);
}

#[test]
fn unknown_provider_is_rejected_up_front() {
    assert!(matches!(
        runner(Some("nope")),
        Err(EvalError::UnknownProvider { name }) if name == "nope"
    ));
}
