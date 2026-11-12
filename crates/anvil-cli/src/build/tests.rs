//! Tests of `anvil build`: the plan, and whole builds in throwaway git
//! repositories driven through the real agent loop with scripted providers.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use anvil_core::agent::{
    AgentLoop, AutoApprove, CheckCommand, EventSink, LoopConfig, LoopProviders, PlanDecision,
    ScriptedGate, Status,
};
use anvil_core::provider::{Budgeted, FakeProvider, Provider, Script, TokenBudget, ToolCall};
use anvil_core::tools::{
    Permissions, PromptAnswer, Shell, StaticPrompt, ToolContext, ToolRegistry,
};
use anvil_types::LimitsConfig;

use super::git;
use super::plan::{
    Finished, MAX_ITEMS, PLANNER_SYSTEM, SPEC_WHOLE_CAP, SUMMARY_CAP, TaskInputs, WorkItem,
    finished_summary, item_file_name, item_task, parse_plan, planning_prompt, spec_for_item,
};
use super::run::{
    Budgets, DECLINED_EXIT, Env, STOPPED_EXIT, SessionReport, SessionRequest, Settings, resume,
    start,
};
use super::state::{BuildState, BuildStatus, ItemStatus, build_ids};

const PLAN: &str = "Looked around.\n\n# Plan\n\n1. [ ] Write the file\n2. [ ] Check it\n";

/// A planner reply with one item per `(title, acceptance)`.
fn plan_reply(items: &[(&str, &str)]) -> String {
    let items: Vec<serde_json::Value> = items
        .iter()
        .map(|(title, acceptance)| {
            serde_json::json!({
                "title": title,
                "description": format!("{title}, in detail."),
                "area": "src",
                "acceptance": acceptance,
            })
        })
        .collect();
    format!(
        "Here is the plan.\n```json\n{}\n```",
        serde_json::json!({ "items": items })
    )
}

/// Runs `git` in `dir` and returns its trimmed stdout.
fn git_out(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// A repository on `main` with one commit.
fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    git_out(dir.path(), &["init", "-q"]);
    git_out(dir.path(), &["symbolic-ref", "HEAD", "refs/heads/main"]);
    for (key, value) in [
        ("user.name", "Test"),
        ("user.email", "test@example.com"),
        ("commit.gpgsign", "false"),
        ("core.autocrlf", "false"),
    ] {
        git_out(dir.path(), &["config", key, value]);
    }
    std::fs::write(dir.path().join("README.md"), "hello\n").expect("write");
    git_out(dir.path(), &["add", "-A"]);
    git_out(dir.path(), &["commit", "-q", "-m", "initial"]);
    dir
}

fn settings(cwd: &Path, check: CheckCommand) -> Settings {
    Settings {
        cwd: cwd.to_path_buf(),
        shell: Shell::for_platform(),
        check,
        session_tokens: 1_000_000,
        price: None,
    }
}

fn write_spec(cwd: &Path) -> PathBuf {
    let path = cwd.join("spec.md");
    std::fs::write(&path, "# Tiny\n\nBuild three things.\n").expect("spec");
    // Outside the work tree's tracked files would be cleaner, but the spec
    // must not make the tree dirty: commit it.
    git_out(cwd, &["add", "spec.md"]);
    git_out(cwd, &["commit", "-q", "-m", "add the spec"]);
    path
}

/// Scripts for sessions that each write the given file and are approved.
fn sessions(files: &[&str]) -> (Vec<Script>, Vec<Script>) {
    let mut coder = Vec::new();
    let mut reviewer = Vec::new();
    for file in files {
        coder.push(FakeProvider::text_turn(PLAN));
        coder.push(FakeProvider::tool_turn(vec![ToolCall {
            id: format!("w-{file}"),
            name: "write_file".to_owned(),
            arguments_json: serde_json::json!({"path": file, "content": "hi"}).to_string(),
        }]));
        coder.push(FakeProvider::text_turn("built"));
        reviewer.push(FakeProvider::text_turn(
            "Fine.\n```json\n{\"verdict\": \"approve\"}\n```",
        ));
    }
    (coder, reviewer)
}

/// The build's `Env` over scripted providers and the real agent loop.
struct ScriptEnv {
    cwd: PathBuf,
    planner: Arc<FakeProvider>,
    coder: Arc<FakeProvider>,
    reviewer: Arc<FakeProvider>,
    tasks: Mutex<Vec<String>>,
    limits: Mutex<Vec<u64>>,
    reported: Mutex<Vec<bool>>,
}

impl ScriptEnv {
    fn new(cwd: &Path, plans: Vec<String>, files: &[&str]) -> Self {
        let (coder, reviewer) = sessions(files);
        Self {
            cwd: cwd.to_path_buf(),
            planner: Arc::new(FakeProvider::new(
                plans
                    .iter()
                    .map(|plan| FakeProvider::text_turn(plan))
                    .collect(),
            )),
            coder: Arc::new(FakeProvider::new(coder)),
            reviewer: Arc::new(FakeProvider::new(reviewer)),
            tasks: Mutex::new(Vec::new()),
            limits: Mutex::new(Vec::new()),
            reported: Mutex::new(Vec::new()),
        }
    }

    fn tasks(&self) -> Vec<String> {
        self.tasks.lock().expect("lock").clone()
    }
}

impl Env for ScriptEnv {
    fn planner(&self) -> anyhow::Result<Arc<dyn Provider>> {
        Ok(self.planner.clone())
    }

    async fn run_session(&self, request: SessionRequest) -> SessionReport {
        self.tasks.lock().expect("lock").push(request.task.clone());
        self.limits.lock().expect("lock").push(request.token_limit);
        let budget = TokenBudget::shared(u64::MAX);
        let coder: Arc<dyn Provider> = Arc::new(Budgeted::new(self.coder.clone(), budget.clone()));
        let reviewer: Arc<dyn Provider> =
            Arc::new(Budgeted::new(self.reviewer.clone(), budget.clone()));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let registry = ToolRegistry::with_builtins(
            Permissions::from_toml("allow = [\"write_file\"]", None).expect("rules"),
            Box::new(StaticPrompt(PromptAnswer::No)),
        );
        let agent = AgentLoop::new(
            LoopConfig {
                task: request.task,
                cwd: self.cwd.clone(),
                session_dir: request.session_dir,
                limits: LimitsConfig::default(),
                check: CheckCommand::Command("exit 0".to_owned()),
                context_window: 100_000,
                home_dir: None,
                extra_instructions: Vec::new(),
                sections: Vec::new(),
                review_sections: Vec::new(),
            },
            LoopProviders {
                coder: coder.clone(),
                planner: None,
                reviewer,
                summarizer: coder,
            },
            registry,
            ToolContext::new(&self.cwd),
            Box::new(AutoApprove),
            EventSink::new(tx),
        );
        let result = agent.run().await;
        let done = result.status == Status::Done;
        self.reported.lock().expect("lock").push(done);
        let tokens = budget.lock().expect("lock").used();
        SessionReport {
            reported_done: done,
            detail: result.status.to_string(),
            tokens,
            cost_usd: None,
        }
    }
}

fn subjects(cwd: &Path) -> Vec<String> {
    git_out(cwd, &["log", "--format=%s", "main"])
        .lines()
        .map(str::to_owned)
        .collect()
}

fn only_build(cwd: &Path) -> (String, PathBuf, BuildState) {
    let ids = build_ids(cwd);
    assert_eq!(ids.len(), 1, "{ids:?}");
    let dir = super::state::build_dir(cwd, &ids[0]);
    let state = BuildState::load(&dir).expect("state");
    (ids[0].clone(), dir, state)
}

fn clean(cwd: &Path) -> bool {
    git::dirty_paths(cwd).expect("status").is_empty()
}

#[tokio::test]
async fn a_three_item_build_commits_once_per_item() {
    let repo = repo();
    let cwd = repo.path();
    let spec = write_spec(cwd);
    let plan = plan_reply(&[
        ("Item one", "git --version"),
        ("Item two", "git --version"),
        ("Item three", "git --version"),
    ]);
    let env = ScriptEnv::new(cwd, vec![plan], &["one.txt", "two.txt", "three.txt"]);
    let settings = settings(cwd, CheckCommand::Command("git --version".to_owned()));

    let code = start(&env, &settings, &spec, Budgets::default(), &AutoApprove)
        .await
        .expect("build");

    assert_eq!(code, 0);
    assert_eq!(
        subjects(cwd),
        [
            "Item three",
            "Item two",
            "Item one",
            "add the spec",
            "initial"
        ]
    );
    let (_, dir, state) = only_build(cwd);
    assert_eq!(state.status, BuildStatus::Done);
    assert!(
        state
            .items
            .iter()
            .all(|item| item.status == ItemStatus::Done)
    );
    assert_eq!(
        state.items[2].commit.as_deref(),
        Some(git_out(cwd, &["rev-parse", "main"]).as_str())
    );
    assert!(dir.join("01-item-one.md").is_file());
    assert!(dir.join("spec.md").is_file());
    assert!(clean(cwd));

    let tasks = env.tasks();
    assert_eq!(tasks.len(), 3);
    assert!(!tasks[0].contains("# Already finished"));
    assert!(tasks[1].contains("# Already finished"));
    assert!(tasks[1].contains("- Item one: Item one, in detail."));
    assert!(tasks[0].contains("Build three things."));
}

#[tokio::test]
async fn a_budget_stop_leaves_item_one_alone_and_resume_finishes_the_rest() {
    let repo = repo();
    let cwd = repo.path();
    let spec = write_spec(cwd);
    let plan = plan_reply(&[
        ("Item one", "git --version"),
        ("Item two", "git --version"),
        ("Item three", "git --version"),
    ]);
    let env = ScriptEnv::new(cwd, vec![plan], &["one.txt", "two.txt", "three.txt"]);
    let settings = settings(cwd, CheckCommand::None);
    let capped = Budgets {
        tokens: Some(60),
        usd: None,
    };

    let code = start(&env, &settings, &spec, capped, &AutoApprove)
        .await
        .expect("build");

    assert_eq!(code, STOPPED_EXIT);
    let (id, _, state) = only_build(cwd);
    assert_eq!(state.status, BuildStatus::Blocked);
    assert!(state.detail.contains("token budget"), "{}", state.detail);
    assert_eq!(state.items[0].status, ItemStatus::Done);
    assert_eq!(state.items[1].status, ItemStatus::Pending);
    assert_eq!(subjects(cwd)[0], "Item one");
    let first_commit = state.items[0].commit.clone().expect("commit");
    // The planner used 15 tokens, so the first item's share is (60 - 15) / 3.
    assert_eq!(env.limits.lock().expect("lock")[0], 15);

    let raised = Budgets {
        tokens: Some(100_000),
        usd: None,
    };
    let code = resume(&env, &settings, None, raised).await.expect("resume");

    assert_eq!(code, 0);
    let (resumed_id, _, state) = only_build(cwd);
    assert_eq!(resumed_id, id);
    assert_eq!(state.status, BuildStatus::Done);
    assert_eq!(
        state.items[0].commit.as_deref(),
        Some(first_commit.as_str())
    );
    assert_eq!(subjects(cwd)[..3], ["Item three", "Item two", "Item one"]);
    let item_one_sessions = env
        .tasks()
        .iter()
        .filter(|task| task.starts_with("# Work item 1 of 3"))
        .count();
    assert_eq!(item_one_sessions, 1, "item one must not run again");
    assert_eq!(env.tasks().len(), 3);
    assert_eq!(git_out(cwd, &["rev-parse", "main~2"]), first_commit);
}

#[tokio::test]
async fn an_item_whose_acceptance_fails_is_not_done_though_the_session_succeeded() {
    let repo = repo();
    let cwd = repo.path();
    let spec = write_spec(cwd);
    let plan = plan_reply(&[
        ("Item one", "git --version"),
        ("Item two", "git not-a-command"),
        ("Item three", "git --version"),
    ]);
    let env = ScriptEnv::new(cwd, vec![plan], &["one.txt", "two.txt", "two.txt"]);
    let settings = settings(cwd, CheckCommand::None);

    let code = start(&env, &settings, &spec, Budgets::default(), &AutoApprove)
        .await
        .expect("build");

    assert_eq!(code, STOPPED_EXIT);
    assert!(
        env.reported.lock().expect("lock").iter().all(|done| *done),
        "every session reported success"
    );
    let (id, _, state) = only_build(cwd);
    assert_eq!(state.status, BuildStatus::NeedsInput);
    assert_eq!(state.items[1].status, ItemStatus::Failed);
    assert_eq!(state.items[1].attempts, 2);
    assert_eq!(state.items[1].commit, None);
    assert_eq!(state.items[2].status, ItemStatus::Pending);
    assert!(
        state.detail.contains("failed 2 attempts"),
        "{}",
        state.detail
    );
    assert_eq!(env.tasks().len(), 3, "item three never started");
    assert!(env.tasks()[2].contains("A previous attempt at this item failed"));

    // Clean tree, build branch at the last good commit, attempts on side branches.
    assert!(clean(cwd));
    assert_eq!(git_out(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]), "main");
    assert_eq!(
        git_out(cwd, &["rev-parse", "HEAD"]),
        state.items[0].commit.clone().expect("commit")
    );
    for attempt in 1..=2 {
        let side = format!("anvil-build/{id}/item-2-attempt-{attempt}");
        assert_eq!(git_out(cwd, &["show", &format!("{side}:two.txt")]), "hi");
    }
    assert!(!cwd.join("two.txt").exists());
}

#[tokio::test]
async fn the_project_check_must_pass_too() {
    let repo = repo();
    let cwd = repo.path();
    let spec = write_spec(cwd);
    let plan = plan_reply(&[("Only item", "git --version")]);
    let env = ScriptEnv::new(cwd, vec![plan], &["a.txt", "a.txt"]);
    let settings = settings(cwd, CheckCommand::Command("git not-a-command".to_owned()));

    let code = start(&env, &settings, &spec, Budgets::default(), &AutoApprove)
        .await
        .expect("build");

    assert_eq!(code, STOPPED_EXIT);
    let (_, _, state) = only_build(cwd);
    assert_eq!(state.status, BuildStatus::NeedsInput);
    assert_eq!(state.items[0].status, ItemStatus::Failed);
    assert!(clean(cwd));
}

#[tokio::test]
async fn an_item_left_in_progress_is_saved_aside_and_retried() {
    let repo = repo();
    let cwd = repo.path();
    let spec = write_spec(cwd);
    let plan = plan_reply(&[("Item one", "git --version"), ("Item two", "git --version")]);
    let env = ScriptEnv::new(cwd, vec![plan], &["one.txt", "two.txt"]);
    let settings = settings(cwd, CheckCommand::None);
    let capped = Budgets {
        tokens: Some(60),
        usd: None,
    };
    start(&env, &settings, &spec, capped, &AutoApprove)
        .await
        .expect("build");

    // What a crash in the middle of item two leaves behind.
    let (id, dir, mut state) = only_build(cwd);
    state.items[1].status = ItemStatus::InProgress;
    state.items[1].attempts = 1;
    state.save(&dir).expect("save");
    std::fs::write(cwd.join("half-done.txt"), "partial").expect("write");

    let raised = Budgets {
        tokens: Some(100_000),
        usd: None,
    };
    let code = resume(&env, &settings, Some(&id), raised)
        .await
        .expect("resume");

    assert_eq!(code, 0);
    let (_, _, state) = only_build(cwd);
    assert_eq!(state.items[1].status, ItemStatus::Done);
    assert_eq!(state.items[1].attempts, 2);
    let side = format!("anvil-build/{id}/item-2-attempt-1");
    assert_eq!(
        git_out(cwd, &["show", &format!("{side}:half-done.txt")]),
        "partial"
    );
    assert!(clean(cwd));
    assert_eq!(subjects(cwd)[..2], ["Item two", "Item one"]);
    assert!(!cwd.join("half-done.txt").exists());
}

#[tokio::test]
async fn refuses_outside_a_git_repository_and_with_a_dirty_tree() {
    let plain = tempfile::tempdir().expect("tempdir");
    std::fs::write(plain.path().join("spec.md"), "# Spec\n").expect("spec");
    let env = ScriptEnv::new(plain.path(), vec![], &[]);
    let error = start(
        &env,
        &settings(plain.path(), CheckCommand::None),
        &plain.path().join("spec.md"),
        Budgets::default(),
        &AutoApprove,
    )
    .await
    .expect_err("no repository");
    assert!(
        format!("{error:#}").contains("not inside a git repository"),
        "{error:#}"
    );

    let repo = repo();
    let cwd = repo.path();
    let spec = write_spec(cwd);
    std::fs::write(cwd.join("scratch.txt"), "x").expect("write");
    let env = ScriptEnv::new(cwd, vec![], &[]);
    let error = start(
        &env,
        &settings(cwd, CheckCommand::None),
        &spec,
        Budgets::default(),
        &AutoApprove,
    )
    .await
    .expect_err("dirty");
    let text = format!("{error:#}");
    assert!(text.contains("uncommitted changes"), "{text}");
    assert!(text.contains("scratch.txt"), "{text}");
    assert_eq!(
        env.planner.calls(),
        0,
        "nothing was asked before the refusal"
    );
    assert!(build_ids(cwd).is_empty());
}

#[test]
fn anvil_files_do_not_make_the_tree_dirty() {
    let repo = repo();
    let cwd = repo.path();
    let dir = cwd.join(".anvil/builds/some-build");
    std::fs::create_dir_all(&dir).expect("dir");
    std::fs::write(dir.join("state.json"), "{}").expect("write");
    assert!(clean(cwd));
    std::fs::write(cwd.join("other.txt"), "x").expect("write");
    assert_eq!(git::dirty_paths(cwd).expect("status").len(), 1);
}

#[tokio::test]
async fn a_malformed_plan_is_asked_for_again_once() {
    let repo = repo();
    let cwd = repo.path();
    let spec = write_spec(cwd);
    let plans = vec![
        "I would start with the parser.".to_owned(),
        plan_reply(&[("Item one", "git --version")]),
    ];
    let env = ScriptEnv::new(cwd, plans, &["one.txt"]);

    let code = start(
        &env,
        &settings(cwd, CheckCommand::None),
        &spec,
        Budgets::default(),
        &AutoApprove,
    )
    .await
    .expect("build");

    assert_eq!(code, 0);
    assert_eq!(env.planner.calls(), 2);
    let second = &env.planner.requests()[1];
    assert_eq!(second.messages.len(), 3);

    let repo = repo_with_spec();
    let env = ScriptEnv::new(
        repo.0.path(),
        vec!["no json".to_owned(), "still none".to_owned()],
        &[],
    );
    let error = start(
        &env,
        &settings(repo.0.path(), CheckCommand::None),
        &repo.1,
        Budgets::default(),
        &AutoApprove,
    )
    .await
    .expect_err("two bad replies");
    assert!(
        format!("{error:#}").contains("malformed twice"),
        "{error:#}"
    );
    assert!(build_ids(repo.0.path()).is_empty());
}

/// A repository with a committed spec, and the spec's path.
fn repo_with_spec() -> (tempfile::TempDir, PathBuf) {
    let repo = repo();
    let spec = write_spec(repo.path());
    (repo, spec)
}

#[tokio::test]
async fn the_owner_can_revise_or_abort_the_list() {
    let (repo, spec) = repo_with_spec();
    let cwd = repo.path();
    let plans = vec![
        plan_reply(&[("Item one", "git --version"), ("Item two", "git --version")]),
        plan_reply(&[("Item one", "git --version")]),
    ];
    let env = ScriptEnv::new(cwd, plans, &["one.txt"]);
    let gate = ScriptedGate::new(vec![PlanDecision::Revise("fewer items".to_owned())]);

    let code = start(
        &env,
        &settings(cwd, CheckCommand::None),
        &spec,
        Budgets::default(),
        &gate,
    )
    .await
    .expect("build");

    assert_eq!(code, 0);
    assert_eq!(gate.plans().len(), 2);
    assert!(gate.plans()[0].contains("2. Item two"));
    let feedback_seen = format!("{:?}", env.planner.requests()[1].messages);
    assert!(feedback_seen.contains("fewer items"));
    assert_eq!(only_build(cwd).2.items.len(), 1);

    let (repo, spec) = repo_with_spec();
    let env = ScriptEnv::new(
        repo.path(),
        vec![plan_reply(&[("Item one", "git --version")])],
        &[],
    );
    let gate = ScriptedGate::new(vec![PlanDecision::Abort]);
    let code = start(
        &env,
        &settings(repo.path(), CheckCommand::None),
        &spec,
        Budgets::default(),
        &gate,
    )
    .await
    .expect("declined");
    assert_eq!(code, DECLINED_EXIT);
    assert!(build_ids(repo.path()).is_empty());
}

#[test]
fn a_plan_must_have_titled_items_with_acceptance_commands() {
    assert!(parse_plan("no json here").is_err());
    assert!(parse_plan("```json\n{\"items\": []}\n```").is_err());
    let missing_title =
        parse_plan("```json\n{\"items\": [{\"title\": \" \", \"acceptance\": \"x\"}]}\n```")
            .expect_err("no title");
    assert!(format!("{missing_title:#}").contains("no title"));
    let missing_check =
        parse_plan("```json\n{\"items\": [{\"title\": \"A\", \"acceptance\": \"\"}]}\n```")
            .expect_err("no acceptance");
    assert!(format!("{missing_check:#}").contains("no acceptance command"));

    let many: Vec<serde_json::Value> = (0..=MAX_ITEMS)
        .map(|n| serde_json::json!({"title": format!("t{n}"), "acceptance": "x"}))
        .collect();
    let too_many = format!("```json\n{}\n```", serde_json::json!({ "items": many }));
    assert!(format!("{:#}", parse_plan(&too_many).expect_err("too many")).contains("too many"));

    let items = parse_plan(&plan_reply(&[("  Padded  ", " git --version ")])).expect("valid");
    assert_eq!(items[0].title, "Padded");
    assert_eq!(items[0].acceptance, "git --version");
}

#[test]
fn item_files_are_numbered_slugs() {
    assert_eq!(item_file_name(1, "Add the parser!"), "01-add-the-parser.md");
    assert_eq!(item_file_name(12, "???"), "12-item.md");
    let long = item_file_name(3, &"word ".repeat(30));
    assert!(long.len() < 60, "{long}");
}

#[test]
fn a_long_spec_is_cut_to_the_named_section() {
    let filler = "x".repeat(SPEC_WHOLE_CAP);
    let spec = format!(
        "# Title\n\nintro\n\n## Parser\n\nparse things\n\n### Detail\n\nmore parsing\n\n## Output\n\nprint things\n\n{filler}"
    );
    let parser = spec_for_item(&spec, "parser");
    assert!(parser.starts_with("## Parser"));
    assert!(parser.contains("### Detail"));
    assert!(!parser.contains("print things"));
    let unknown = spec_for_item(&spec, "Nonexistent");
    assert!(unknown.contains("only its start is shown"));
    assert_eq!(spec_for_item("# Short\n", "anything"), "# Short\n");
}

#[test]
fn the_finished_summary_drops_the_oldest_lines_first() {
    let descriptions: Vec<String> = (0..60)
        .map(|n| format!("item {n} does something fairly long to describe here"))
        .collect();
    let titles: Vec<String> = (0..60).map(|n| format!("Title {n}")).collect();
    let finished: Vec<Finished<'_>> = titles
        .iter()
        .zip(&descriptions)
        .map(|(title, description)| Finished { title, description })
        .collect();
    let summary = finished_summary(&finished);
    assert!(summary.chars().count() <= SUMMARY_CAP, "{}", summary.len());
    assert!(summary.ends_with("- Title 59: item 59 does something fairly long to describe here"));
    assert!(!summary.contains("Title 0:"));
    assert_eq!(finished_summary(&[]), "");
}

#[test]
fn the_planning_prompt_is_stable() {
    insta::assert_snapshot!(
        "planning_prompt",
        format!(
            "{PLANNER_SYSTEM}\n\n---\n\n{}",
            planning_prompt("# Tiny\n\nA CLI that prints hello.\n")
        )
    );
}

#[test]
fn the_item_task_is_stable() {
    let item = WorkItem {
        title: "Add the greeting".to_owned(),
        description: "Print hello from main.".to_owned(),
        area: "src/main.rs".to_owned(),
        acceptance: "cargo run -q".to_owned(),
        spec_section: String::new(),
    };
    insta::assert_snapshot!(
        "item_task",
        item_task(TaskInputs {
            number: 2,
            total: 3,
            item: &item,
            spec: "# Tiny\n\nA CLI that prints hello.\n",
            finished: "- Set up the crate: A new binary crate.",
            previous_failure: Some("the acceptance command did not exit 0"),
        })
    );
}
