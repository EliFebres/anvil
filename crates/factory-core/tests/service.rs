//! The service loop with a fake worker, host and notifier over real git.

mod support;

use std::sync::Arc;

use anvil_types::ticket::dirs;
use anvil_types::{FactoryConfig, Priority, TicketKind, TicketStatus};
use chrono::{DateTime, Local, TimeZone};
use factory_core::fakes::{FakeHost, RecordingNotifier, ScriptedWorker};
use factory_core::{
    CiStatus, Event, FactoryError, Outcome, OutcomeKind, Service, ServiceDeps, Tick,
};
use support::{World, have_git, ticket};

/// A local clock stopped at the given hour on 2026-10-31.
fn clock_at(hour: u32) -> factory_core::Clock {
    Arc::new(move || -> DateTime<Local> {
        Local
            .with_ymd_and_hms(2026, 10, 31, hour, 0, 0)
            .single()
            .expect("valid time")
    })
}

/// The pieces of a service, kept so the test can inspect them afterwards.
struct Harness {
    world: World,
    worker: Arc<ScriptedWorker>,
    host: Arc<FakeHost>,
    notifier: Arc<RecordingNotifier>,
    service: Service,
}

impl Harness {
    async fn new(worker: ScriptedWorker, hour: u32) -> Self {
        let world = World::new();
        let queue = world.queue().await;
        let worker = Arc::new(worker);
        let host = Arc::new(FakeHost::green());
        let notifier = Arc::new(RecordingNotifier::default());
        let config: FactoryConfig =
            toml::from_str(&FactoryConfig::template("owner/repo")).expect("template");
        let service = Service::new(ServiceDeps {
            home: world.home.clone(),
            config,
            queue,
            worker: Arc::clone(&worker) as Arc<_>,
            host: Arc::clone(&host) as Arc<_>,
            notifier: Arc::clone(&notifier) as Arc<_>,
            hostname: "nuc".to_owned(),
            clock: clock_at(hour),
            probe: Arc::new(factory_core::NoProbe),
        })
        .expect("service");
        Self {
            world,
            worker,
            host,
            notifier,
            service,
        }
    }

    fn submit(&self, priority: Priority, kind: TicketKind) -> anvil_types::Ticket {
        let t = ticket(priority, "2026-10-01T00:00:00Z", kind);
        self.world.submit(&t, "origin/main");
        t
    }
}

#[tokio::test]
async fn a_model_ticket_waits_without_starting_the_worker() {
    if !have_git() {
        return;
    }
    let h = Harness::new(ScriptedWorker::default(), 7).await;
    let t = h.submit(Priority::P0, TicketKind::Model);
    let tick = h.service.run_once().await.expect("runs");
    assert_eq!(tick, Tick::Worked(t.id));
    assert_eq!(h.worker.calls(), 0, "no worker for model tickets");
    let stored = h.world.state_ticket(dirs::WAITING_DIR, t.id);
    assert_eq!(stored.status, TicketStatus::Waiting);
    assert!(
        stored
            .note
            .as_deref()
            .expect("note")
            .contains("anvil serve")
    );
    let events = h.notifier.events();
    assert!(
        matches!(&events[..], [Event::NeedsHuman { pr: None, .. }]),
        "{events:?}"
    );
    assert_eq!(h.service.state().outcomes[0].kind, OutcomeKind::Waiting);
    assert_eq!(h.service.run_once().await.expect("runs"), Tick::Idle);
}

#[tokio::test]
async fn a_worked_ticket_is_merged_recorded_and_announced() {
    if !have_git() {
        return;
    }
    let h = Harness::new(
        ScriptedWorker::scripted(vec![Ok(Outcome::Merged {
            pr: 42,
            cost_usd: Some(1.25),
        })]),
        7,
    )
    .await;
    let t = h.submit(Priority::P1, TicketKind::Bug);
    assert_eq!(
        h.service.run_once().await.expect("runs"),
        Tick::Worked(t.id)
    );
    assert_eq!(h.worker.calls(), 1);
    let stored = h.world.state_ticket(dirs::MERGED_DIR, t.id);
    assert_eq!(stored.status, TicketStatus::Merged);
    assert_eq!(stored.pr, Some(42));
    assert_eq!(stored.claimed_by.as_deref(), Some("nuc"));
    let state = h.service.state();
    assert_eq!(state.outcomes.len(), 1);
    assert_eq!(state.outcomes[0].kind, OutcomeKind::Merged);
    assert_eq!(state.outcomes[0].detail, "#42");
    assert!(state.in_progress.is_none());
    assert_eq!(h.worker.stages(), vec!["scripted".to_owned()]);
    assert!(matches!(
        &h.notifier.events()[..],
        [Event::Merged {
            pr: 42,
            cost_usd: Some(_),
            ..
        }]
    ));
    assert!(h.service.paused().is_none());
}

#[tokio::test]
async fn the_pause_flag_stops_the_loop_between_tickets() {
    if !have_git() {
        return;
    }
    let h = Harness::new(ScriptedWorker::default(), 7).await;
    h.submit(Priority::P1, TicketKind::Bug);
    h.service.pause("by hand").expect("pause");
    assert_eq!(
        h.service.run_once().await.expect("runs"),
        Tick::Paused("by hand".to_owned())
    );
    assert_eq!(h.worker.calls(), 0);
    h.service.resume().expect("resume");
    assert!(matches!(
        h.service.run_once().await.expect("runs"),
        Tick::Worked(_)
    ));
    assert_eq!(h.worker.calls(), 1);
}

#[tokio::test]
async fn a_red_main_pauses_the_service_and_notifies() {
    if !have_git() {
        return;
    }
    let h = Harness::new(ScriptedWorker::default(), 7).await;
    let t = h.submit(Priority::P1, TicketKind::Bug);
    h.host.set_main_ci(CiStatus::Red {
        sha: "abc1234".to_owned(),
        url: "https://ci.example/run/1".to_owned(),
    });
    let tick = h.service.run_once().await.expect("runs");
    assert!(matches!(tick, Tick::Paused(reason) if reason.contains("abc1234")));
    assert!(
        h.service
            .paused()
            .expect("paused")
            .contains("main CI is red")
    );
    assert_eq!(h.worker.calls(), 0);
    assert!(matches!(
        &h.notifier.events()[..],
        [Event::SelfPaused { .. }]
    ));
    assert!(
        h.world
            .remote_ticket_branches()
            .contains(&format!("tickets/{}", t.id)),
        "nothing was ingested while red"
    );
    h.host.set_main_ci(CiStatus::Pending);
    assert!(matches!(
        h.service.run_once().await.expect("runs"),
        Tick::Paused(_)
    ));
    h.service.resume().expect("resume");
    assert!(matches!(
        h.service.run_once().await.expect("runs"),
        Tick::Worked(_)
    ));
}

#[tokio::test]
async fn worker_errors_reopen_the_ticket_and_two_in_a_row_pause() {
    if !have_git() {
        return;
    }
    let h = Harness::new(
        ScriptedWorker::scripted(vec![
            Err("podman missing".to_owned()),
            Err("still".to_owned()),
        ]),
        7,
    )
    .await;
    let t = h.submit(Priority::P1, TicketKind::Bug);
    assert_eq!(
        h.service.run_once().await.expect("runs"),
        Tick::Worked(t.id)
    );
    let stored = h.world.state_ticket(dirs::OPEN_DIR, t.id);
    assert_eq!(stored.status, TicketStatus::Open);
    assert_eq!(stored.claimed_by, None);
    assert!(h.service.paused().is_none(), "one error does not pause");
    assert_eq!(h.service.state().consecutive_errors, 1);
    assert!(matches!(&h.notifier.events()[..], [Event::Error { .. }]));

    assert_eq!(
        h.service.run_once().await.expect("runs"),
        Tick::Worked(t.id)
    );
    assert!(
        h.service
            .paused()
            .expect("paused")
            .contains("2 errors in a row")
    );
    assert_eq!(
        h.world.state_ticket(dirs::OPEN_DIR, t.id).status,
        TicketStatus::Open
    );
    assert_eq!(h.service.state().outcomes[0].kind, OutcomeKind::Reopened);
}

#[tokio::test]
async fn a_paused_outcome_reopens_and_pauses_at_once() {
    if !have_git() {
        return;
    }
    let h = Harness::new(
        ScriptedWorker::scripted(vec![Ok(Outcome::Paused {
            reason: FactoryError::NoBaseline.to_string(),
        })]),
        7,
    )
    .await;
    let t = h.submit(Priority::P1, TicketKind::Bug);
    assert_eq!(
        h.service.run_once().await.expect("runs"),
        Tick::Worked(t.id)
    );
    assert!(
        h.service
            .paused()
            .expect("paused")
            .contains("no eval result")
    );
    assert_eq!(
        h.world.state_ticket(dirs::OPEN_DIR, t.id).status,
        TicketStatus::Open
    );
    assert!(matches!(
        &h.notifier.events()[..],
        [Event::SelfPaused { .. }]
    ));
    assert_eq!(h.service.state().digest.paused, 1);
}

#[tokio::test]
async fn the_digest_goes_out_once_a_day_after_the_configured_time() {
    if !have_git() {
        return;
    }
    let early = Harness::new(ScriptedWorker::default(), 7).await;
    assert_eq!(early.service.run_once().await.expect("runs"), Tick::Idle);
    assert!(early.notifier.events().is_empty(), "07:00 is before 08:00");

    let late = Harness::new(
        ScriptedWorker::scripted(vec![Ok(Outcome::Merged {
            pr: 3,
            cost_usd: Some(2.0),
        })]),
        9,
    )
    .await;
    late.submit(Priority::P1, TicketKind::Bug);
    assert!(matches!(
        late.service.run_once().await.expect("runs"),
        Tick::Worked(_)
    ));
    assert_eq!(late.service.run_once().await.expect("runs"), Tick::Idle);
    assert_eq!(late.service.run_once().await.expect("runs"), Tick::Idle);
    let digests: Vec<Event> = late
        .notifier
        .events()
        .into_iter()
        .filter(|e| matches!(e, Event::Digest { .. }))
        .collect();
    assert_eq!(digests.len(), 1, "{:?}", late.notifier.events());
    // The digest goes out at the top of the pass, before that pass's ticket
    // is worked, so the merge counts toward tomorrow's digest.
    match &digests[0] {
        Event::Digest {
            merged, cost_usd, ..
        } => {
            assert_eq!(*merged, 0);
            assert!(cost_usd.abs() < 1e-9);
        }
        other => panic!("not a digest: {other:?}"),
    }
    let counters = late.service.state().digest;
    assert_eq!(
        counters.merged, 1,
        "the merge counts toward the next digest"
    );
    assert!((counters.cost_usd - 2.0).abs() < 1e-9);
    assert!(counters.last_sent.is_some());
}

#[tokio::test]
async fn a_refused_branch_is_announced_once() {
    if !have_git() {
        return;
    }
    let h = Harness::new(ScriptedWorker::default(), 7).await;
    let bad = ticket(Priority::P1, "2026-10-01T00:00:00Z", TicketKind::Bug);
    h.world.submit_with_extra_file(&bad);
    assert_eq!(h.service.run_once().await.expect("runs"), Tick::Idle);
    assert_eq!(h.service.run_once().await.expect("runs"), Tick::Idle);
    let refused: Vec<_> = h
        .notifier
        .events()
        .into_iter()
        .filter(|e| matches!(e, Event::BranchRefused { .. }))
        .collect();
    assert_eq!(refused.len(), 1, "{refused:?}");
}

#[tokio::test]
async fn an_empty_protected_list_is_refused() {
    if !have_git() {
        return;
    }
    let world = World::new();
    let queue = world.queue().await;
    let mut config: FactoryConfig =
        toml::from_str(&FactoryConfig::template("owner/repo")).expect("template");
    config.protected.paths.clear();
    let error = Service::new(ServiceDeps {
        home: world.home.clone(),
        config,
        queue,
        worker: Arc::new(ScriptedWorker::default()),
        host: Arc::new(FakeHost::green()),
        notifier: Arc::new(RecordingNotifier::default()),
        hostname: "nuc".to_owned(),
        clock: clock_at(7),
        probe: Arc::new(factory_core::NoProbe),
    })
    .err()
    .expect("refused");
    assert!(error.to_string().contains("[protected]"), "{error}");
}

/// A worker that reports stages and reads the dashboard status while it
/// works, so the test sees what `factory watch` would mid-ticket.
struct PeekingWorker {
    status_file: std::path::PathBuf,
    seen: std::sync::Mutex<Option<factory_core::Status>>,
}

impl factory_core::Worker for PeekingWorker {
    fn work<'a>(
        &'a self,
        _ticket: &'a anvil_types::Ticket,
        _queue: &'a factory_core::TicketQueue,
        progress: factory_core::Progress,
    ) -> factory_core::BoxFuture<'a, factory_core::Result<Outcome>> {
        Box::pin(async move {
            use factory_core::{ProgressEvent, Stage};
            progress(ProgressEvent::stage(
                Stage::Worktree,
                "preparing the worktree",
            ));
            progress(ProgressEvent::stage(Stage::Implement, "implementer run 1"));
            progress(ProgressEvent::Counters { run: 1, round: 0 });
            progress(ProgressEvent::PullRequest(7));
            let mut reader = factory_core::StatusReader::new();
            *self.seen.lock().expect("lock") = reader.poll(&self.status_file).cloned();
            Ok(Outcome::Merged {
                pr: 7,
                cost_usd: Some(0.5),
            })
        })
    }
}

#[tokio::test]
async fn the_dashboard_status_follows_the_ticket_and_keeps_the_history() {
    if !have_git() {
        return;
    }
    let world = World::new();
    let queue = world.queue().await;
    let worker = Arc::new(PeekingWorker {
        status_file: world.home.status_file(),
        seen: std::sync::Mutex::new(None),
    });
    let config: FactoryConfig =
        toml::from_str(&FactoryConfig::template("owner/repo")).expect("template");
    let service = Service::new(ServiceDeps {
        home: world.home.clone(),
        config,
        queue,
        worker: Arc::clone(&worker) as Arc<_>,
        host: Arc::new(FakeHost::green()),
        notifier: Arc::new(RecordingNotifier::default()),
        hostname: "nuc".to_owned(),
        clock: Arc::new(Local::now),
        probe: Arc::new(factory_core::NoProbe),
    })
    .expect("service");
    let t = ticket(Priority::P1, "2026-10-01T00:00:00Z", TicketKind::Bug);
    world.submit(&t, "origin/main");

    assert_eq!(service.run_once().await.expect("runs"), Tick::Worked(t.id));

    let seen = worker
        .seen
        .lock()
        .expect("lock")
        .clone()
        .expect("status mid-ticket");
    assert_eq!(seen.service.state, factory_core::board::RunState::Running);
    assert_eq!(seen.host, "nuc");
    assert_eq!(seen.repo, "owner/repo");
    let ticket_view = seen.ticket.expect("ticket");
    assert_eq!(ticket_view.id, t.id.to_string());
    assert_eq!(ticket_view.kind, "bug");
    assert_eq!(ticket_view.priority, "p1");
    assert_eq!(ticket_view.implementer_run, 1);
    assert_eq!(ticket_view.implementer_runs_max, 2);
    assert_eq!(ticket_view.pr, Some(7));
    assert_eq!(
        ticket_view.running_stage(),
        Some(factory_core::Stage::Implement)
    );
    assert!(ticket_view.stages[0].ended_at.is_some(), "the claim ended");
    assert_eq!(ticket_view.budget.wall_clock_minutes, 90);
    assert_eq!(seen.health.main_ci.expect("ci").state, "green");

    let after: factory_core::Status = serde_json::from_str(
        &std::fs::read_to_string(world.home.status_file()).expect("status file"),
    )
    .expect("parses");
    assert!(after.ticket.is_none());
    assert_eq!(after.service.state, factory_core::board::RunState::Idle);
    assert_eq!(after.recent.len(), 1);
    assert_eq!(after.recent[0].pr, Some(7));
    assert!(after.recent[0].started_at.is_some());
    assert_eq!(after.today.merged, 1);
    assert_eq!(after.week.expect("week").totals.merged, 1);
    let history = factory_core::history::read(&world.home.outcomes_file());
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].kind, OutcomeKind::Merged);
    assert!(
        !world.home.output_log(&t.id.to_string()).exists(),
        "the ticket's live files are cleaned up"
    );
}
