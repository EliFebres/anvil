//! Tests of the reporter: files, notifications and herdr, driven without a terminal.

use anvil_core::agent::{IncompleteReason, ScriptedGate, State};
use anvil_core::tools::{ScriptedPrompt, StaticPrompt};
use anvil_types::{LoopState, SessionState};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::file::read_status;
use super::herdr::HerdrEnv;
use super::*;

fn setup(dir: &Path, webhook: Option<String>) -> Setup {
    Setup {
        project: dir.to_path_buf(),
        home: None,
        session_id: "01SESSION".to_owned(),
        session_dir: dir.join("session"),
        task: "fix the bug\nwith details".to_owned(),
        prices: Prices::default(),
        webhook,
        herdr: None,
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
    let _status = StatusReporter::start(setup(dir.path(), None));
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
    let status = StatusReporter::start(setup(dir.path(), None));
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
    let status = StatusReporter::start(setup(dir.path(), None));
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
    let status = StatusReporter::start(setup(dir.path(), None));
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
    let status = StatusReporter::start(setup(dir.path(), None));
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
    let status = StatusReporter::start(setup(dir.path(), None));
    status.finish_run(&Status::Done);
    assert_eq!(on_disk(dir.path()).state, SessionState::Done);

    let other = tempfile::tempdir().expect("dir");
    let status = StatusReporter::start(setup(other.path(), None));
    status.fail("cannot reach\nthe provider");
    let written = on_disk(other.path());
    assert_eq!(written.state, SessionState::Failed);
    assert_eq!(written.detail, "cannot reach the provider");
}

#[tokio::test]
async fn the_session_holds_its_lock_until_it_is_dropped() {
    let dir = tempfile::tempdir().expect("dir");
    let status = StatusReporter::start(setup(dir.path(), None));
    let session = dir.path().join("session");
    assert!(!file::lock_is_free(&session));
    drop(status);
    assert!(file::lock_is_free(&session));
}

/// What a test does to a reporter to make it enter a state.
type Act = Box<dyn Fn(&StatusReporter)>;

async fn bodies(server: &MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .expect("requests")
        .iter()
        .map(|request| String::from_utf8_lossy(&request.body).into_owned())
        .collect()
}

#[tokio::test]
async fn one_post_per_state_entered_and_none_for_working() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let blocked = Status::Incomplete {
        reason: IncompleteReason::PlanRejected,
    };
    let cases: [(&str, Act); 4] = [
        (
            "needs you",
            Box::new(|status| {
                status
                    .prompt(false, Box::new(StaticPrompt(PromptAnswer::Yes)))
                    .ask(&request());
            }),
        ),
        ("done", Box::new(|status| status.finish_run(&Status::Done))),
        (
            "blocked",
            Box::new(move |status| status.finish_run(&blocked)),
        ),
        ("failed", Box::new(|status| status.fail("boom"))),
    ];
    for (expected, act) in cases {
        server.reset().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().expect("dir");
        let status = StatusReporter::start(setup(dir.path(), Some(server.uri())));
        status.observe(&entered(State::Build));
        status.observe(&LoopEvent::TextDelta("hello".to_owned()));
        assert!(bodies(&server).await.is_empty(), "working posts nothing");
        act(&status);
        // Events after the state was entered do not announce it again.
        status.observe(&entered(State::Verify));
        status.shutdown().await;
        let posted = bodies(&server).await;
        assert_eq!(posted.len(), 1, "{expected}: {posted:?}");
        assert!(
            posted[0].starts_with(&format!("anvil {expected}: fix the bug")),
            "{posted:?}"
        );
    }
}

#[tokio::test]
async fn an_unreachable_webhook_does_not_fail_or_slow_the_session() {
    let dir = tempfile::tempdir().expect("dir");
    let status = StatusReporter::start(setup(
        dir.path(),
        Some("http://127.0.0.1:9/hook".to_owned()),
    ));
    status.fail("boom");
    status.shutdown().await;
    assert_eq!(on_disk(dir.path()).state, SessionState::Failed);
}

#[tokio::test]
async fn a_missing_herdr_binary_does_not_affect_the_session() {
    let dir = tempfile::tempdir().expect("dir");
    let mut setup = setup(dir.path(), None);
    setup.herdr = Some(HerdrEnv {
        bin: PathBuf::from("/definitely/not/herdr"),
        pane: "p1".to_owned(),
    });
    let status = StatusReporter::start(setup);
    let prompt = status.prompt(false, Box::new(StaticPrompt(PromptAnswer::Yes)));
    prompt.ask(&request());
    status.finish_run(&Status::Done);
    status.shutdown().await;
    assert_eq!(on_disk(dir.path()).state, SessionState::Done);
}

#[tokio::test]
async fn a_scripted_prompt_behind_the_wrapper_still_answers() {
    let dir = tempfile::tempdir().expect("dir");
    let status = StatusReporter::start(setup(dir.path(), None));
    let prompt = status.prompt(
        false,
        Box::new(ScriptedPrompt::new(vec![PromptAnswer::Session])),
    );
    assert_eq!(prompt.ask(&request()), PromptAnswer::Session);
}
