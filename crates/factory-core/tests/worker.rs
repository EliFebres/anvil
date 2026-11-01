//! The worker loop with fakes for the implementer, the eval gate and the
//! review swarm, over real git and a bare repository standing in for GitHub.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anvil_types::ticket::dirs;
use anvil_types::{FactoryConfig, Priority, Ticket, TicketKind, TicketStatus};
use factory_core::fakes::{FakeEvalGate, FakeSwarm, LocalHost, ScriptedImplementer, ScriptedRun};
use factory_core::{GateB, Git, Outcome, TicketQueue, TicketWorker, Worker, WorkerDeps};
use support::{World, git, have_git, ticket};

/// A clock that jumps two hours ahead after `ticks_before_jump` reads.
fn clock(ticks_before_jump: usize) -> factory_core::worker::InstantClock {
    let start = Instant::now();
    let reads = Arc::new(AtomicUsize::new(0));
    Arc::new(move || {
        let n = reads.fetch_add(1, Ordering::SeqCst);
        if n >= ticks_before_jump {
            start + Duration::from_secs(2 * 3600)
        } else {
            start
        }
    })
}

/// The pieces, kept for assertions.
struct Harness {
    world: World,
    queue: TicketQueue,
    implementer: Arc<ScriptedImplementer>,
    eval: Arc<FakeEvalGate>,
    swarm: Arc<FakeSwarm>,
    host: Arc<LocalHost>,
    worker: TicketWorker,
    ticket: Ticket,
}

impl Harness {
    async fn new(
        implementer: ScriptedImplementer,
        eval: FakeEvalGate,
        swarm: FakeSwarm,
        clock: factory_core::worker::InstantClock,
    ) -> Self {
        let world = World::new();
        let queue = world.queue().await;
        let t = ticket(Priority::P1, "2026-10-01T00:00:00Z", TicketKind::Skill);
        world.submit(&t, "origin/main");
        queue.refresh().await.expect("refresh");
        queue.ingest_branches().await.expect("ingest");
        let claimed = queue.claim(&t, "nuc").await.expect("claim");
        let implementer = Arc::new(implementer);
        let eval = Arc::new(eval);
        let swarm = Arc::new(swarm);
        let host = Arc::new(LocalHost::new(
            &world.remote,
            &world.dir.path().join("scratch"),
        ));
        let config: FactoryConfig =
            toml::from_str(&FactoryConfig::template("owner/repo")).expect("template");
        let worker = TicketWorker::new(WorkerDeps {
            repo: Git::new(&world.factory),
            remote: "origin".to_owned(),
            home: world.home.clone(),
            config,
            implementer: Arc::clone(&implementer) as Arc<_>,
            eval_gate: Arc::clone(&eval) as Arc<_>,
            review: Arc::clone(&swarm) as Arc<_>,
            host: Arc::clone(&host) as Arc<_>,
            secrets: vec![
                "sk-live-SECRET-0001".to_owned(),
                "ghp_FACTORYTOKEN".to_owned(),
            ],
            clock,
        })
        .expect("worker");
        Self {
            world,
            queue,
            implementer,
            eval,
            swarm,
            host,
            worker,
            ticket: claimed,
        }
    }

    async fn work(&self) -> Outcome {
        let stages = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Arc::clone(&stages);
        let progress: factory_core::Progress = Arc::new(move |stage: &str| {
            sink.lock().expect("lock").push(stage.to_owned());
        });
        self.worker
            .work(&self.ticket, &self.queue, progress)
            .await
            .expect("the factory itself did not fail")
    }

    fn main_subjects(&self) -> Vec<String> {
        git(&self.world.remote, &["log", "--format=%s", "main"])
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn main_has(&self, path: &str) -> bool {
        git(
            &self.world.remote,
            &["ls-tree", "-r", "--name-only", "main"],
        )
        .lines()
        .any(|line| line == path)
    }

    fn remote_branches(&self) -> Vec<String> {
        git(
            &self.world.remote,
            &["for-each-ref", "--format=%(refname:short)", "refs/heads/"],
        )
        .lines()
        .map(str::to_owned)
        .collect()
    }
}

#[tokio::test]
async fn a_clean_ticket_goes_from_worktree_to_squash_merge() {
    if !have_git() {
        return;
    }
    let h = Harness::new(
        ScriptedImplementer::default(),
        FakeEvalGate::default(),
        FakeSwarm::default(),
        clock(usize::MAX),
    )
    .await;
    let outcome = h.work().await;
    let Outcome::Merged { pr, cost_usd } = &outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(*pr, 1);
    assert!((cost_usd.expect("cost") - (0.01 + 0.5 + 0.25)).abs() < 1e-9);
    assert_eq!(h.implementer.runs(), 1);
    assert_eq!(h.implementer.checks(), 1);
    assert_eq!(h.implementer.prepared(), 1);
    assert_eq!(h.eval.calls(), 1);
    assert_eq!(h.swarm.inputs().len(), 1);
    let input = &h.swarm.inputs()[0];
    assert_eq!(
        input.changed_files,
        vec!["skills/hello/SKILL.md".to_owned()]
    );
    assert!(input.diff.contains("+Say hello."), "{}", input.diff);
    assert_eq!(input.ticket.id, h.ticket.id);

    assert_eq!(
        h.main_subjects()[0],
        h.ticket.title,
        "one squash commit titled like the ticket"
    );
    assert!(h.main_has("skills/hello/SKILL.md"));
    let branch = format!("factory/{}", h.ticket.id);
    assert!(
        !h.remote_branches().contains(&branch),
        "{:?}",
        h.remote_branches()
    );
    let pulls = h.host.recorder().pulls();
    assert_eq!(pulls.len(), 1);
    assert!(pulls[0].merged);
    assert!(pulls[0].labels.is_empty());
    assert!(pulls[0].body.contains("- [ ] cargo test passes"));
    assert!(pulls[0].body.contains("| A: just check | pass |"));
    assert!(pulls[0].body.contains("| openai/correctness | approve |"));
    let in_review = h.world.state_ticket(dirs::CLAIMED_DIR, h.ticket.id);
    assert_eq!(in_review.status, TicketStatus::InReview);
    assert_eq!(in_review.pr, Some(1));
    assert!(
        !h.world.home.work_dir(&h.ticket.id.to_string()).exists(),
        "the worktree is removed"
    );
}

#[tokio::test]
async fn gate_a_gets_one_retry_with_the_failure_appended_then_rejects() {
    if !have_git() {
        return;
    }
    let h = Harness::new(
        ScriptedImplementer::default().with_checks(vec![false, false]),
        FakeEvalGate::default(),
        FakeSwarm::default(),
        clock(usize::MAX),
    )
    .await;
    let outcome = h.work().await;
    let Outcome::Rejected { reason, notes, .. } = &outcome else {
        panic!("{outcome:?}");
    };
    assert!(reason.contains("gate A"), "{reason}");
    assert!(reason.contains("2 implementer attempts"), "{reason}");
    assert_eq!(h.implementer.runs(), 2);
    assert_eq!(h.implementer.checks(), 2);
    assert_eq!(h.eval.calls(), 0, "gate B never ran");
    let tasks = h.implementer.tasks();
    assert!(!tasks[0].contains("Previous attempt failed"));
    assert!(
        tasks[1].contains("## Previous attempt failed `just check`"),
        "{}",
        tasks[1]
    );
    assert!(tasks[1].contains("E0425"));
    assert!(notes.contains("## gate A after run 1: fail"));
    assert!(notes.contains("## gate A after run 2: fail"));
    assert!(notes.contains("**Rejected:**"));
    assert!(h.host.recorder().pulls().is_empty(), "nothing was pushed");
    assert_eq!(h.remote_branches().len(), 2, "main and factory/state only");
}

#[tokio::test]
async fn review_changes_buy_one_more_round_and_the_second_approval_merges() {
    if !have_git() {
        return;
    }
    let h = Harness::new(
        ScriptedImplementer::default(),
        FakeEvalGate::default(),
        FakeSwarm::scripted(vec![
            FakeSwarm::changes("`max_results` is never applied"),
            FakeSwarm::approved(),
        ]),
        clock(usize::MAX),
    )
    .await;
    let outcome = h.work().await;
    assert!(matches!(outcome, Outcome::Merged { .. }), "{outcome:?}");
    assert_eq!(h.implementer.runs(), 2);
    assert_eq!(h.eval.calls(), 2, "all gates re-run after changes");
    assert_eq!(h.swarm.inputs().len(), 2);
    let tasks = h.implementer.tasks();
    assert!(
        tasks[1].contains("## Review findings to address"),
        "{}",
        tasks[1]
    );
    assert!(tasks[1].contains("`max_results` is never applied"));
}

#[tokio::test]
async fn two_review_rounds_asking_for_changes_reject() {
    if !have_git() {
        return;
    }
    let h = Harness::new(
        ScriptedImplementer::default(),
        FakeEvalGate::default(),
        FakeSwarm::scripted(vec![FakeSwarm::changes("a"), FakeSwarm::changes("b")]),
        clock(usize::MAX),
    )
    .await;
    let outcome = h.work().await;
    let Outcome::Rejected { reason, .. } = &outcome else {
        panic!("{outcome:?}");
    };
    assert!(reason.contains("gate C"), "{reason}");
    assert!(reason.contains("2 review rounds"), "{reason}");
    assert_eq!(
        h.implementer.runs(),
        2,
        "one implementer run per review round"
    );
}

#[tokio::test]
async fn gate_b_regressions_reject_and_a_missing_baseline_pauses() {
    if !have_git() {
        return;
    }
    let h = Harness::new(
        ScriptedImplementer::default(),
        FakeEvalGate::scripted(vec![GateB::Failed(FakeEvalGate::report(&[
            "rs-grep", "py-cli",
        ]))]),
        FakeSwarm::default(),
        clock(usize::MAX),
    )
    .await;
    let outcome = h.work().await;
    let Outcome::Rejected { reason, notes, .. } = &outcome else {
        panic!("{outcome:?}");
    };
    assert!(reason.contains("gate B"), "{reason}");
    assert!(reason.contains("rs-grep, py-cli"), "{reason}");
    assert!(notes.contains("## gate B: fail"));
    assert_eq!(h.swarm.inputs().len(), 0, "the review never ran");

    let h = Harness::new(
        ScriptedImplementer::default(),
        FakeEvalGate::scripted(vec![GateB::NoBaseline]),
        FakeSwarm::default(),
        clock(usize::MAX),
    )
    .await;
    let outcome = h.work().await;
    assert!(
        matches!(&outcome, Outcome::Paused { reason } if reason.contains("no eval result")),
        "{outcome:?}"
    );
    assert!(h.host.recorder().pulls().is_empty());
}

#[tokio::test]
async fn budgets_reject_with_their_reason() {
    if !have_git() {
        return;
    }
    // Tokens: the template allows 6,000,000; one run uses more.
    let h = Harness::new(
        ScriptedImplementer::default().with_runs(vec![ScriptedRun::done(
            &[("skills/big/SKILL.md", "x")],
            7_000_000,
        )]),
        FakeEvalGate::default(),
        FakeSwarm::default(),
        clock(usize::MAX),
    )
    .await;
    let outcome = h.work().await;
    assert!(
        matches!(&outcome, Outcome::Rejected { reason, .. } if reason.contains("token budget exceeded")),
        "{outcome:?}"
    );

    // Wall clock: the clock jumps two hours ahead right after the budget starts.
    let h = Harness::new(
        ScriptedImplementer::default(),
        FakeEvalGate::default(),
        FakeSwarm::default(),
        clock(1),
    )
    .await;
    let outcome = h.work().await;
    assert!(
        matches!(&outcome, Outcome::Rejected { reason, .. } if reason.contains("wall clock budget of 90 minutes exceeded")),
        "{outcome:?}"
    );
    assert_eq!(
        h.implementer.runs(),
        0,
        "nothing ran once the clock was out"
    );

    // No change: the implementer wrote nothing.
    let h = Harness::new(
        ScriptedImplementer::default().with_runs(vec![ScriptedRun::done(&[], 10)]),
        FakeEvalGate::default(),
        FakeSwarm::default(),
        clock(usize::MAX),
    )
    .await;
    let outcome = h.work().await;
    assert!(
        matches!(&outcome, Outcome::Rejected { reason, .. } if reason.contains("changed nothing")),
        "{outcome:?}"
    );
}

#[tokio::test]
async fn a_secret_in_the_worktree_rejects_before_anything_is_pushed() {
    if !have_git() {
        return;
    }
    let h = Harness::new(
        ScriptedImplementer::default().with_runs(vec![ScriptedRun::done(
            &[("skills/leak/SKILL.md", "key: sk-live-SECRET-0001\n")],
            10,
        )]),
        FakeEvalGate::default(),
        FakeSwarm::default(),
        clock(usize::MAX),
    )
    .await;
    let outcome = h.work().await;
    let Outcome::Rejected { reason, .. } = &outcome else {
        panic!("{outcome:?}");
    };
    assert!(reason.contains("secret value appeared"), "{reason}");
    assert!(reason.contains("skills/leak/SKILL.md"), "{reason}");
    assert!(
        !reason.contains("sk-live-SECRET-0001"),
        "the value itself is never written"
    );
    assert_eq!(h.implementer.checks(), 0, "gate A never ran");
    assert_eq!(h.remote_branches().len(), 2, "nothing was pushed");
}

#[tokio::test]
async fn a_protected_path_opens_a_labelled_pull_request_and_waits() {
    if !have_git() {
        return;
    }
    let h = Harness::new(
        ScriptedImplementer::default().with_runs(vec![ScriptedRun::done(
            &[
                ("skills/hello/SKILL.md", "fine"),
                ("deploy/nuc/Containerfile", "FROM scratch\n"),
                (
                    "crates/anvil-core/Cargo.toml",
                    "[package]\nname = \"anvil-core\"\n[dependencies]\nsha2 = \"0.10\"\n",
                ),
            ],
            100,
        )]),
        FakeEvalGate::default(),
        FakeSwarm::default(),
        clock(usize::MAX),
    )
    .await;
    let outcome = h.work().await;
    let Outcome::NeedsHuman { pr, held, .. } = &outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(*pr, 1);
    assert!(
        held.iter()
            .any(|h| h.starts_with("deploy/nuc/Containerfile (deploy/**)")),
        "{held:?}"
    );
    assert!(
        held.iter()
            .any(|h| h.contains("new dependency `sha2` in [dependencies]")),
        "{held:?}"
    );
    assert!(!held.iter().any(|h| h.starts_with("skills/")), "{held:?}");
    let pulls = h.host.recorder().pulls();
    assert_eq!(pulls[0].labels, vec!["needs-human".to_owned()]);
    assert!(!pulls[0].merged);
    assert!(pulls[0].body.contains("## Needs a human"));
    assert!(
        h.remote_branches()
            .iter()
            .any(|b| b == &format!("factory/{}", h.ticket.id)),
        "the branch stays for the human"
    );
    assert_eq!(h.main_subjects()[0], "init", "nothing merged");
}

#[tokio::test]
async fn red_ci_on_the_pull_request_closes_it_and_rejects() {
    if !have_git() {
        return;
    }
    let h = Harness::new(
        ScriptedImplementer::default(),
        FakeEvalGate::default(),
        FakeSwarm::default(),
        clock(usize::MAX),
    )
    .await;
    h.host.recorder().set_pull_ci(factory_core::CiStatus::Red {
        sha: "x".to_owned(),
        url: "https://ci.example/run/9".to_owned(),
    });
    let outcome = h.work().await;
    let Outcome::Rejected { reason, .. } = &outcome else {
        panic!("{outcome:?}");
    };
    assert!(reason.contains("CI failed"), "{reason}");
    assert!(reason.contains("https://ci.example/run/9"));
    let pulls = h.host.recorder().pulls();
    assert!(
        pulls[0]
            .closed
            .as_deref()
            .expect("closed")
            .contains("CI failed")
    );
    assert!(!pulls[0].merged);
    let branch = format!("factory/{}", h.ticket.id);
    assert!(
        !h.remote_branches().contains(&branch),
        "the branch is deleted"
    );
}

#[tokio::test]
async fn minor_findings_are_posted_as_a_review_and_denied_hosts_land_in_the_notes() {
    if !have_git() {
        return;
    }
    let mut report = FakeSwarm::approved();
    report.minors = vec![
        factory_core::MinorFinding {
            file: Some("skills/hello/SKILL.md".to_owned()),
            line: Some(1),
            anchored: true,
            body: "**minor** (openai/style): name it better".to_owned(),
        },
        factory_core::MinorFinding {
            file: None,
            line: None,
            anchored: false,
            body: "**minor** (google/simplicity): nothing to delete".to_owned(),
        },
    ];
    let h = Harness::new(
        ScriptedImplementer::default()
            .with_checks(vec![false, false])
            .with_denied(vec!["evil.example:443".to_owned()]),
        FakeEvalGate::default(),
        FakeSwarm::scripted(vec![report.clone()]),
        clock(usize::MAX),
    )
    .await;
    let outcome = h.work().await;
    let Outcome::Rejected { notes, .. } = &outcome else {
        panic!("{outcome:?}");
    };
    assert!(
        notes.contains("## egress refused by the allowlist"),
        "{notes}"
    );
    assert!(notes.contains("- `evil.example:443`"));

    let h = Harness::new(
        ScriptedImplementer::default(),
        FakeEvalGate::default(),
        FakeSwarm::scripted(vec![report]),
        clock(usize::MAX),
    )
    .await;
    assert!(matches!(h.work().await, Outcome::Merged { .. }));
    let pulls = h.host.recorder().pulls();
    assert_eq!(pulls[0].reviews.len(), 1);
    let (body, inline) = &pulls[0].reviews[0];
    assert_eq!(inline.len(), 1);
    assert_eq!(inline[0].path, "skills/hello/SKILL.md");
    assert!(body.contains("nothing to delete"), "{body}");
}
