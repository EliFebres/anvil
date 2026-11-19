//! Tests of the reporter's files, driven without a terminal.

use anvil_core::agent::{IncompleteReason, ScriptedGate, State};
use anvil_core::tools::{ScriptedPrompt, StaticPrompt};
use anvil_types::{LoopState, SessionState};

use super::file::read_status;
use super::*;

fn setup(dir: &Path) -> Setup {
    Setup {
        project: dir.to_path_buf(),
        home: None,
        session_id: "01SESSION".to_owned(),
        session_dir: dir.join("session"),
        task: "fix the bug\nwith details".to_owned(),
        prices: Prices::default(),
    }
}

fn on_disk(dir: &Path) -> SessionStatus {
    read_status(&dir.join("session")).expect("status.json")
}

fn entered(state: State) -> LoopEvent {
    LoopEvent::StateEntered { state, visit: 1 }
}

fn request() -> PermissionRequest {
    PermissionRequest {
        tool: "shell".to_owned(),
        target: Some("cargo test".to_owned()),
        arguments: serde_json::json!({}),
        needs_person: false,
    }
}

/// A prompt that reads the status file while it is being asked.
struct Peeking {
    dir: PathBuf,
    seen: Arc<Mutex<Option<SessionStatus>>>,
}

impl PermissionPrompt for Peeking {
    fn ask(&self, _request: &PermissionRequest) -> PromptAnswer {
        *self.seen.lock().expect("lock") = read_status(&self.dir);
        PromptAnswer::Yes
    }
}

#[tokio::test]
async fn the_first_write_has_every_field() {
    let dir = tempfile::tempdir().expect("dir");
    let _status = StatusReporter::start(setup(dir.path()));
    let written = on_disk(dir.path());
    assert_eq!(written.schema, 1);
    assert_eq!(written.session_id, "01SESSION");
    assert_eq!(written.task, "fix the bug");
    assert_eq!(written.state, SessionState::Working);
    assert_eq!(written.loop_state, None);
    assert!(written.updated_at >= written.started_at);
}

#[tokio::test]
async fn a_permission_prompt_shows_needs_input_in_the_file_while_it_waits() {
    let dir = tempfile::tempdir().expect("dir");
    let status = StatusReporter::start(setup(dir.path()));
    status.observe(&entered(State::Build));
    let seen = Arc::new(Mutex::new(None));
    let prompt = status.prompt(
        false,
        Box::new(Peeking {
            dir: dir.path().join("session"),
            seen: Arc::clone(&seen),
        }),
    );
    assert_eq!(prompt.ask(&request()), PromptAnswer::Yes);
    let during = seen
        .lock()
        .expect("lock")
        .clone()
        .expect("read while asking");
    assert_eq!(during.state, SessionState::NeedsInput);
    assert_eq!(during.detail, "permission: shell cargo test");
    assert_eq!(during.loop_state, Some(LoopState::Build));
    let after = on_disk(dir.path());
    assert_eq!(after.state, SessionState::Working);
    assert!(after.updated_at >= during.updated_at);
}

#[tokio::test]
async fn an_unattended_run_wraps_nothing() {
    let dir = tempfile::tempdir().expect("dir");
    let status = StatusReporter::start(setup(dir.path()));
    let prompt = status.prompt(true, Box::new(StaticPrompt(PromptAnswer::No)));
    assert_eq!(prompt.ask(&request()), PromptAnswer::No);
    assert_eq!(on_disk(dir.path()).state, SessionState::Working);
}

/// A gate that reads the status file while the plan waits.
struct PeekingGate {
    dir: PathBuf,
    seen: Arc<Mutex<Option<SessionStatus>>>,
}

impl PlanGate for PeekingGate {
    fn review(&self, _plan: &str) -> PlanDecision {
        *self.seen.lock().expect("lock") = read_status(&self.dir);
        PlanDecision::Approve
    }
}

#[tokio::test]
async fn a_plan_waits_as_needs_input() {
    let dir = tempfile::tempdir().expect("dir");
    let status = StatusReporter::start(setup(dir.path()));
    status.observe(&entered(State::Plan));
    let seen = Arc::new(Mutex::new(None));
    let gate = status.gate(
        false,
        Box::new(PeekingGate {
            dir: dir.path().join("session"),
            seen: Arc::clone(&seen),
        }),
    );
    assert_eq!(gate.review("the plan"), PlanDecision::Approve);
    let during = seen
        .lock()
        .expect("lock")
        .clone()
        .expect("read while waiting");
    assert_eq!(during.state, SessionState::NeedsInput);
    assert_eq!(during.detail, "plan approval");
    assert_eq!(during.loop_state, Some(LoopState::Plan));
    assert_eq!(on_disk(dir.path()).state, SessionState::Working);
    // A scripted gate behind the wrapper still decides.
    let scripted = status.gate(
        false,
        Box::new(ScriptedGate::new(vec![PlanDecision::Abort])),
    );
    assert_eq!(scripted.review("p"), PlanDecision::Abort);
}

#[tokio::test]
async fn a_budget_stop_is_blocked_and_names_the_budget() {
    let dir = tempfile::tempdir().expect("dir");
    let status = StatusReporter::start(setup(dir.path()));
    status.finish_run(&Status::Incomplete {
        reason: IncompleteReason::SessionBudget {
            used: 20,
            limit: 10,
        },
    });
    let written = on_disk(dir.path());
    assert_eq!(written.state, SessionState::Blocked);
    assert!(
        written.detail.contains("token budget"),
        "{}",
        written.detail
    );
}

#[tokio::test]
async fn a_normal_finish_is_done_and_a_failure_is_failed() {
    let dir = tempfile::tempdir().expect("dir");
    let status = StatusReporter::start(setup(dir.path()));
    status.finish_run(&Status::Done);
    assert_eq!(on_disk(dir.path()).state, SessionState::Done);

    let other = tempfile::tempdir().expect("dir");
    let status = StatusReporter::start(setup(other.path()));
    status.fail("cannot reach\nthe provider");
    let written = on_disk(other.path());
    assert_eq!(written.state, SessionState::Failed);
    assert_eq!(written.detail, "cannot reach the provider");
}

#[tokio::test]
async fn the_session_holds_its_lock_until_it_is_dropped() {
    let dir = tempfile::tempdir().expect("dir");
    let status = StatusReporter::start(setup(dir.path()));
    let session = dir.path().join("session");
    assert!(!file::lock_is_free(&session));
    drop(status);
    assert!(file::lock_is_free(&session));
}

#[tokio::test]
async fn a_scripted_prompt_behind_the_wrapper_still_answers() {
    let dir = tempfile::tempdir().expect("dir");
    let status = StatusReporter::start(setup(dir.path()));
    let prompt = status.prompt(
        false,
        Box::new(ScriptedPrompt::new(vec![PromptAnswer::Session])),
    );
    assert_eq!(prompt.ask(&request()), PromptAnswer::Session);
}
