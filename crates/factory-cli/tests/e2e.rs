//! End to end: a ticket goes from `open` to `merged` against a bare
//! repository standing in for GitHub, with eval's `FakeRunner`, the review
//! swarm's `FakeReviewer` and a scripted implementer.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Instant;

use anvil_types::ticket::dirs::{self, OPEN_DIR, STATE_BRANCH};
use anvil_types::{
    FactoryConfig, Priority, Stance, Ticket, TicketKind, TicketOrigin, TicketStatus,
};
use chrono::{Local, TimeZone};
use eval::{FakeRunner, RunOptions};
use factory_cli::adapters::eval_gate::{HarnessGate, RunnerSource};
use factory_cli::adapters::review_swarm::Swarm;
use factory_core::fakes::{LocalHost, RecordingNotifier, ScriptedImplementer, ScriptedRun};
use factory_core::{
    Event, FactoryHome, Git, Service, ServiceDeps, Tick, TicketQueue, TicketWorker, WorkerDeps,
};
use factory_review::{FakeReviewer, Reviewer};
use ulid::Ulid;

/// The three fixture tasks the eval crate ships for its own tests.
fn tasks_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../eval/tests/fixtures/tasks")
}

/// Runs git for test setup and panics with its stderr when it fails.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, text).expect("write");
}

/// The eval script both the baseline and the candidate follow: no regressions.
fn runner() -> Arc<dyn eval::EvalRunner> {
    FakeRunner::scripted(&[
        ("pass-when-done", &[true]),
        ("greps-marker", &[true]),
        ("always-fails", &[true]),
    ])
}

/// A baseline result over the fixture tasks, as JSON.
async fn baseline_json(repo_root: &Path) -> String {
    let options = RunOptions {
        repo_root: repo_root.to_path_buf(),
        tasks_dir: tasks_dir(),
        tags: Vec::new(),
        repeat: 1,
        parallel: 2,
        provider: Some("fake".to_owned()),
        progress: None,
    };
    let result = eval::run_with(&options, runner())
        .await
        .expect("baseline runs");
    serde_json::to_string_pretty(&result).expect("serializes")
}

fn ticket() -> Ticket {
    Ticket {
        id: Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("ulid"),
        created_at: "2026-10-30T10:00:00Z".parse().expect("rfc3339"),
        kind: TicketKind::Skill,
        title: "Add a hello skill".to_owned(),
        symptom: "There is no skill that says hello.".to_owned(),
        repro: None,
        spec: "A `hello` skill under skills/hello/SKILL.md.".to_owned(),
        acceptance: vec!["skills/hello/SKILL.md exists".to_owned()],
        evidence: Vec::new(),
        priority: Priority::P1,
        status: TicketStatus::Open,
        origin: TicketOrigin::Human,
        claimed_at: None,
        claimed_by: None,
        pr: None,
        note: None,
    }
}

#[tokio::test]
async fn a_ticket_goes_from_open_to_merged_against_a_bare_repository() {
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("git not available; skipping");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let remote = dir.path().join("github.git");
    std::fs::create_dir_all(&remote).expect("mkdir");
    git(&remote, &["init", "-q", "--bare", "-b", "main"]);

    // main: a README, an empty queue folder and one eval baseline.
    let seed = dir.path().join("seed");
    std::fs::create_dir_all(&seed).expect("mkdir");
    git(&seed, &["init", "-q", "-b", "main"]);
    write(&seed.join("README.md"), "# repo\n");
    write(&seed.join("CLAUDE.md"), "# Rules\n\nNo unwrap.\n");
    write(&seed.join(OPEN_DIR).join(".gitkeep"), "");
    write(
        &seed.join("eval/results/2026-10-30-abc1234-fake-1.json"),
        &baseline_json(&seed).await,
    );
    git(&seed, &["add", "-A"]);
    git(&seed, &["commit", "-q", "-m", "init"]);
    git(
        &seed,
        &["remote", "add", "origin", &remote.to_string_lossy()],
    );
    git(&seed, &["push", "-q", "origin", "main"]);
    git(
        &seed,
        &["push", "-q", "origin", &format!("main:{STATE_BRANCH}")],
    );

    // A laptop submits the ticket on its own branch, as `anvil ticket submit` does.
    let laptop = dir.path().join("laptop");
    git(
        dir.path(),
        &["clone", "-q", &remote.to_string_lossy(), "laptop"],
    );
    let t = ticket();
    let branch = format!("tickets/{}", t.id);
    git(
        &laptop,
        &[
            "checkout",
            "-q",
            "-b",
            &branch,
            &format!("origin/{STATE_BRANCH}"),
        ],
    );
    write(
        &laptop.join(OPEN_DIR).join(format!("{}.toml", t.id)),
        &toml::to_string_pretty(&t).expect("serializes"),
    );
    git(&laptop, &["add", "-A"]);
    git(&laptop, &["commit", "-q", "-m", "Add ticket"]);
    git(&laptop, &["push", "-q", "origin", &branch]);

    // The factory's clone and home.
    git(
        dir.path(),
        &["clone", "-q", &remote.to_string_lossy(), "factory-repo"],
    );
    let factory_repo = dir.path().join("factory-repo");
    let home = FactoryHome::under(&dir.path().join("home"));
    let queue = TicketQueue::open(&Git::new(&factory_repo), &home.state_dir(), "origin")
        .await
        .expect("queue");
    let config: FactoryConfig =
        toml::from_str(&FactoryConfig::template("owner/repo")).expect("template");
    let host = Arc::new(LocalHost::new(&remote, &dir.path().join("scratch")));
    let notifier = Arc::new(RecordingNotifier::default());
    // Joined at run time so `anvil scan-keys` does not flag this file.
    let secret = ["sk-", "live-ABCDEF0123456789"].concat();
    let implementer = Arc::new(
        ScriptedImplementer::default().with_runs(vec![ScriptedRun::done(
            &[(
                "skills/hello/SKILL.md",
                "---\nname: hello\n---\n\nSay hello.\n",
            )],
            2000,
        )]),
    );
    let eval_gate = HarnessGate::new(
        RunnerSource::Fixed(runner()),
        Git::new(&factory_repo),
        "origin",
        &tasks_dir(),
        2,
        "fake",
    );
    let reviewers: Vec<Box<dyn Reviewer>> = vec![
        Box::new(FakeReviewer::approving("alpha", Stance::Correctness)),
        Box::new(FakeReviewer::approving("beta", Stance::Safety)),
    ];
    let worker = TicketWorker::new(WorkerDeps {
        repo: Git::new(&factory_repo),
        remote: "origin".to_owned(),
        home: home.clone(),
        config: config.clone(),
        implementer: Arc::clone(&implementer) as Arc<_>,
        eval_gate: Arc::new(eval_gate),
        review: Arc::new(Swarm::new(reviewers)),
        host: Arc::clone(&host) as Arc<_>,
        secrets: vec![secret.clone()],
        clock: Arc::new(Instant::now),
    })
    .expect("worker");
    let service = Service::new(ServiceDeps {
        home: home.clone(),
        config,
        queue,
        worker: Arc::new(worker),
        host: Arc::clone(&host) as Arc<_>,
        notifier: Arc::clone(&notifier) as Arc<_>,
        hostname: "nuc".to_owned(),
        clock: Arc::new(|| {
            Local
                .with_ymd_and_hms(2026, 10, 31, 7, 0, 0)
                .single()
                .expect("valid time")
        }),
    })
    .expect("service");

    // One pass: ingest the branch, claim, work, merge.
    let tick = service.run_once().await.expect("pass");
    assert_eq!(tick, Tick::Worked(t.id));

    // main holds the change as one squash commit titled like the ticket.
    let subjects = git(&remote, &["log", "--format=%s", "main"]);
    assert_eq!(
        subjects.lines().next(),
        Some("Add a hello skill"),
        "{subjects}"
    );
    assert_eq!(subjects.lines().count(), 2);
    let tree = git(&remote, &["ls-tree", "-r", "--name-only", "main"]);
    assert!(tree.lines().any(|l| l == "skills/hello/SKILL.md"), "{tree}");
    assert!(
        !tree.contains("tickets/merged"),
        "ticket state never reaches main"
    );

    // factory/state holds the merged ticket with its pull request number.
    let merged = git(
        &remote,
        &[
            "show",
            &format!("{STATE_BRANCH}:{}/{}.toml", dirs::MERGED_DIR, t.id),
        ],
    );
    let merged: Ticket = toml::from_str(&merged).expect("ticket parses");
    assert_eq!(merged.status, TicketStatus::Merged);
    assert_eq!(merged.pr, Some(1));
    assert_eq!(merged.claimed_by.as_deref(), Some("nuc"));
    let branches = git(
        &remote,
        &["for-each-ref", "--format=%(refname:short)", "refs/heads/"],
    );
    assert_eq!(
        branches.lines().collect::<Vec<_>>(),
        vec![STATE_BRANCH, "main"],
        "{branches}"
    );

    // The pull request carried the gates, the eval table and the verdicts.
    let pulls = host.recorder().pulls();
    assert_eq!(pulls.len(), 1);
    assert!(pulls[0].merged);
    assert!(pulls[0].body.contains("- [ ] skills/hello/SKILL.md exists"));
    assert!(pulls[0].body.contains("| A: just check | pass |"));
    assert!(
        pulls[0]
            .body
            .contains("| B: eval | pass | 0 regression(s) |")
    );
    assert!(
        pulls[0].body.contains("pass-when-done"),
        "the compare table is in the body"
    );
    assert!(pulls[0].body.contains("| alpha/correctness | approve |"));
    assert!(pulls[0].body.contains("| beta/safety | approve |"));

    // One notification, the merge, with the cost of the model calls.
    let events = notifier.events();
    assert!(
        matches!(
            &events[..],
            [Event::Merged {
                pr: 1,
                cost_usd: Some(_),
                ..
            }]
        ),
        "{events:?}"
    );
    let state = service.state();
    assert_eq!(state.outcomes.len(), 1);
    assert!(state.in_progress.is_none());

    // Nothing under the factory's work folder holds the secret, and the
    // worktree itself is gone.
    assert!(!home.work_dir(&t.id.to_string()).exists());
    let mut stack = vec![home.root().to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().is_some_and(|n| n == ".git") {
                    continue;
                }
                stack.push(path);
            } else if let Ok(bytes) = std::fs::read(&path) {
                assert!(
                    !bytes.windows(secret.len()).any(|w| w == secret.as_bytes()),
                    "secret in {}",
                    path.display()
                );
            }
        }
    }
    assert_eq!(implementer.runs(), 1);
    assert_eq!(service.run_once().await.expect("pass"), Tick::Idle);
}
