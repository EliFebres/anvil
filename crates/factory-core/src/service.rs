//! The service loop: poll the state branch, take the next ticket, work it,
//! record what happened.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anvil_types::ticket::dirs;
use anvil_types::{FactoryConfig, Ticket, TicketKind};
use chrono::{DateTime, FixedOffset, Local, NaiveTime, Offset, Utc};
use serde::Serialize;
use ulid::Ulid;

use crate::board::{
    Board, BudgetView, CiView, Live, PollView, QueuedTicket, RunState, STATUS_VERSION, Stage,
    StageView, Status, TicketView, write_atomic,
};
use crate::budget::{MAX_GATE_A_ATTEMPTS, MAX_REVIEW_ROUNDS};
use crate::error::{FactoryError, Result};
use crate::notify::{Event, Notifier};
use crate::paths::FactoryHome;
use crate::ports::{CiStatus, CodeHost, Outcome, Probe, Progress, ProgressEvent, Worker};
use crate::status::{InProgress, OutcomeKind, OutcomeRecord, ServiceState};
use crate::tickets::{MODEL_NOTE, TicketQueue, Transition};
use crate::{history, tail};

/// Errors in a row after which the loop pauses itself.
pub const ERRORS_BEFORE_PAUSE: u32 = 2;

/// How often the dashboard status is rewritten while a ticket is in progress.
const BOARD_TICK: Duration = Duration::from_secs(2);

/// How often the providers are pinged for the health panel.
const PING_EVERY: Duration = Duration::from_secs(3600);

/// A source of the current local time; tests inject one.
pub type Clock = Arc<dyn Fn() -> DateTime<Local> + Send + Sync>;

/// What one pass of the loop did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tick {
    /// The pause flag is set; nothing ran.
    Paused(String),
    /// No ticket was waiting.
    Idle,
    /// A ticket was taken and finished one way or another.
    Worked(Ulid),
}

/// Everything the service is built from.
pub struct ServiceDeps {
    /// `~/.factory`.
    pub home: FactoryHome,
    /// The repository's `factory.toml`.
    pub config: FactoryConfig,
    /// The queue on the state worktree.
    pub queue: TicketQueue,
    /// Works a claimed ticket.
    pub worker: Arc<dyn Worker>,
    /// GitHub, or a fake.
    pub host: Arc<dyn CodeHost>,
    /// Where messages go.
    pub notifier: Arc<dyn Notifier>,
    /// This machine, written into every claim.
    pub hostname: String,
    /// The local clock.
    pub clock: Clock,
    /// Looks at the machine for the dashboard.
    pub probe: Arc<dyn Probe>,
}

/// The factory service.
pub struct Service {
    home: FactoryHome,
    config: FactoryConfig,
    queue: TicketQueue,
    worker: Arc<dyn Worker>,
    host: Arc<dyn CodeHost>,
    notifier: Arc<dyn Notifier>,
    hostname: String,
    clock: Clock,
    state: Arc<Mutex<ServiceState>>,
    board: Arc<Board>,
    probe: Arc<dyn Probe>,
    last_ping: Mutex<Option<Instant>>,
}

impl Service {
    /// Builds the service and loads its state.
    ///
    /// # Arguments
    ///
    /// * `deps` - The parts.
    ///
    /// # Returns
    ///
    /// The service.
    ///
    /// # Errors
    ///
    /// [`FactoryError::Config`] when `factory.toml` has no protected paths
    /// (the list belongs in the file, never in code), and when the state file
    /// is unreadable.
    pub fn new(deps: ServiceDeps) -> Result<Self> {
        if deps.config.protected.paths.is_empty() {
            return Err(FactoryError::Config(
                "[protected] paths in factory.toml is empty; the factory refuses to run without the list of paths that need a human".to_owned(),
            ));
        }
        let state = ServiceState::load(&deps.home.state_file())?;
        let outcomes: Vec<OutcomeRecord> = state.outcomes.iter().cloned().collect();
        if let Err(error) = history::seed(&deps.home.outcomes_file(), &outcomes) {
            tracing::warn!(%error, "cannot start the outcome history");
        }
        let board = Board::new(
            deps.home.status_file(),
            Status {
                v: STATUS_VERSION,
                host: deps.hostname.clone(),
                repo: deps.config.github.repo.clone(),
                started_at: Some((deps.clock)().with_timezone(&Utc)),
                ..Status::default()
            },
        );
        let service = Self {
            home: deps.home,
            config: deps.config,
            queue: deps.queue,
            worker: deps.worker,
            host: deps.host,
            notifier: deps.notifier,
            hostname: deps.hostname,
            clock: deps.clock,
            state: Arc::new(Mutex::new(state)),
            board: Arc::new(board),
            probe: deps.probe,
            last_ping: Mutex::new(None),
        };
        service.refresh_board();
        Ok(service)
    }

    /// A copy of the dashboard snapshot.
    pub fn status(&self) -> Status {
        self.board.snapshot()
    }

    /// The queue.
    pub fn queue(&self) -> &TicketQueue {
        &self.queue
    }

    /// A copy of the current state.
    pub fn state(&self) -> ServiceState {
        lock(&self.state).clone()
    }

    /// Runs forever: one pass, then a pause of `[service] poll_minutes`
    /// unless a ticket was just worked, in which case the next pass follows
    /// at once.
    ///
    /// # Errors
    ///
    /// Never returns `Ok`; errors inside a pass are logged, counted and
    /// pause the service after [`ERRORS_BEFORE_PAUSE`] in a row.
    pub async fn run_forever(&self) -> Result<()> {
        let poll = Duration::from_secs(self.config.service.poll_minutes * 60);
        loop {
            self.refresh_health().await;
            let started = Utc::now();
            let clock = Instant::now();
            let tick = match self.run_once().await {
                Ok(tick) => tick,
                Err(error) => {
                    tracing::error!(%error, "poll failed");
                    self.count_error(None, &error).await;
                    self.refresh_board();
                    Tick::Idle
                }
            };
            let secs = clock.elapsed().as_secs_f64();
            let worked = matches!(tick, Tick::Worked(_));
            self.board.update(|status| {
                status.last_poll = Some(PollView { at: started, secs });
                status.next_poll_at = (!worked).then(|| {
                    Utc::now()
                        + chrono::Duration::seconds(i64::try_from(poll.as_secs()).unwrap_or(0))
                });
            });
            if !worked {
                tokio::time::sleep(poll).await;
            }
        }
    }

    /// One pass of the loop.
    ///
    /// # Returns
    ///
    /// What happened.
    ///
    /// # Errors
    ///
    /// When the state branch cannot be refreshed, the code host cannot be
    /// reached, or a ticket move fails.
    pub async fn run_once(&self) -> Result<Tick> {
        let tick = self.poll_and_work().await;
        self.refresh_board();
        tick
    }

    /// The body of [`Self::run_once`].
    async fn poll_and_work(&self) -> Result<Tick> {
        if let Some(reason) = self.paused() {
            tracing::info!(reason, "paused; not taking tickets");
            return Ok(Tick::Paused(reason));
        }
        self.queue.refresh().await?;
        let (ci, sha) = self.host.main_ci_with_sha().await?;
        self.board
            .update(|status| status.health.main_ci = Some(ci_view(&ci, sha)));
        if let CiStatus::Red { sha, url } = ci {
            let reason =
                format!("main CI is red at {sha} ({url}); not stacking changes on a broken base");
            self.pause_and_notify(&reason).await?;
            return Ok(Tick::Paused(reason));
        }
        let ingested = self.queue.ingest_branches().await?;
        for refused in ingested.refused {
            let key = format!("{}@{}", refused.branch, refused.sha);
            let first_time = lock(&self.state).refused.insert(key);
            if first_time {
                self.save_state()?;
                self.notify(Event::BranchRefused {
                    branch: refused.branch,
                    reason: refused.reason,
                })
                .await;
            }
        }
        self.maybe_send_digest().await?;

        let Some(ticket) = self.queue.pick().await? else {
            return Ok(Tick::Idle);
        };
        let id = ticket.id;
        let title = ticket.title.clone();
        if ticket.kind == TicketKind::Model {
            self.queue
                .transition(
                    id,
                    Transition::Waiting {
                        note: MODEL_NOTE.to_owned(),
                        pr: None,
                    },
                )
                .await?;
            self.record(id, &title, OutcomeKind::Waiting, MODEL_NOTE, None)?;
            self.notify(Event::NeedsHuman {
                id: id.to_string(),
                title,
                pr: None,
                reason: MODEL_NOTE.to_owned(),
            })
            .await;
            return Ok(Tick::Worked(id));
        }

        let claimed = self.queue.claim(&ticket, &self.hostname).await?;
        let claimed_at = claimed.claimed_at.unwrap_or_else(Utc::now);
        self.set_in_progress(Some(InProgress {
            id: id.to_string(),
            title: title.clone(),
            stage: "claimed".to_owned(),
            since: claimed_at,
        }))?;
        self.board
            .start_ticket(self.ticket_view(&claimed, claimed_at));
        let ticker = self.spawn_ticker(&id.to_string());
        let progress = self.progress();
        let result = self.worker.work(&claimed, &self.queue, progress).await;
        ticker.abort();
        self.board.end_ticket();
        let _ = std::fs::remove_file(self.home.output_log(&id.to_string()));
        let _ = std::fs::remove_file(self.home.egress_file(&id.to_string()));
        self.set_in_progress(None)?;
        self.finish(id, &title, claimed_at, result).await?;
        Ok(Tick::Worked(id))
    }

    /// Applies the worker's outcome to the queue, the state and the notifier.
    async fn finish(
        &self,
        id: Ulid,
        title: &str,
        started: DateTime<Utc>,
        result: Result<Outcome>,
    ) -> Result<()> {
        let record = |kind, detail: &str, cost_usd, pr| {
            self.record_outcome(OutcomeRecord {
                id: id.to_string(),
                title: title.to_owned(),
                kind,
                detail: detail.to_owned(),
                finished_at: Utc::now(),
                cost_usd,
                pr,
                started_at: Some(started),
            })
        };
        match result {
            Ok(Outcome::Merged { pr, cost_usd }) => {
                self.queue.transition(id, Transition::Merged { pr }).await?;
                record(OutcomeKind::Merged, &format!("#{pr}"), cost_usd, Some(pr))?;
                self.reset_errors()?;
                self.notify(Event::Merged {
                    id: id.to_string(),
                    title: title.to_owned(),
                    pr,
                    cost_usd,
                })
                .await;
            }
            Ok(Outcome::NeedsHuman { pr, held, cost_usd }) => {
                let reason = format!("PR #{pr} touches protected paths: {}", held.join(", "));
                self.queue
                    .transition(
                        id,
                        Transition::Waiting {
                            note: reason.clone(),
                            pr: Some(pr),
                        },
                    )
                    .await?;
                record(OutcomeKind::Waiting, &reason, cost_usd, Some(pr))?;
                self.reset_errors()?;
                self.notify(Event::NeedsHuman {
                    id: id.to_string(),
                    title: title.to_owned(),
                    pr: Some(pr),
                    reason,
                })
                .await;
            }
            Ok(Outcome::Rejected {
                reason,
                notes,
                cost_usd,
            }) => {
                self.queue
                    .transition(
                        id,
                        Transition::Rejected {
                            note: reason.clone(),
                            notes_md: notes,
                        },
                    )
                    .await?;
                record(OutcomeKind::Rejected, &reason, cost_usd, None)?;
                self.reset_errors()?;
                self.notify(Event::Rejected {
                    id: id.to_string(),
                    title: title.to_owned(),
                    reason,
                })
                .await;
            }
            Ok(Outcome::Paused { reason }) => {
                self.queue.transition(id, Transition::Reopen).await?;
                record(OutcomeKind::Reopened, &reason, None, None)?;
                self.pause_and_notify(&reason).await?;
            }
            Err(error) => {
                tracing::error!(%id, %error, "working the ticket failed; reopening it");
                self.queue.transition(id, Transition::Reopen).await?;
                record(OutcomeKind::Reopened, &error.to_string(), None, None)?;
                self.count_error(Some(id), &error).await;
            }
        }
        Ok(())
    }

    /// Notifies about an error and pauses after [`ERRORS_BEFORE_PAUSE`] in a row.
    async fn count_error(&self, id: Option<Ulid>, error: &FactoryError) {
        let errors = {
            let mut state = lock(&self.state);
            state.consecutive_errors += 1;
            state.consecutive_errors
        };
        if let Err(save_error) = self.save_state() {
            tracing::error!(%save_error, "cannot save the state");
        }
        self.notify(Event::Error {
            id: id.map(|id| id.to_string()),
            detail: error.to_string(),
        })
        .await;
        if errors >= ERRORS_BEFORE_PAUSE {
            let reason = format!("{errors} errors in a row; the last: {error}");
            if let Err(pause_error) = self.pause_and_notify(&reason).await {
                tracing::error!(%pause_error, "cannot pause");
            }
        }
    }

    /// Clears the error streak after a ticket went through.
    fn reset_errors(&self) -> Result<()> {
        lock(&self.state).consecutive_errors = 0;
        self.save_state()
    }

    /// Sends the digest when the configured time has passed and none went
    /// out today, then resets the counters.
    async fn maybe_send_digest(&self) -> Result<()> {
        let now = (self.clock)();
        let at =
            NaiveTime::parse_from_str(&self.config.notify.digest_at, "%H:%M").map_err(|_| {
                FactoryError::Config(format!(
                    "notify.digest_at `{}` is not HH:MM",
                    self.config.notify.digest_at
                ))
            })?;
        let today = now.date_naive();
        let due = {
            let state = lock(&self.state);
            now.time() >= at && state.digest.last_sent != Some(today)
        };
        if !due {
            return Ok(());
        }
        let counters = lock(&self.state).digest.clone();
        self.notify(Event::Digest {
            date: today,
            merged: counters.merged,
            rejected: counters.rejected,
            waiting: counters.waiting,
            paused: counters.paused,
            cost_usd: counters.cost_usd,
        })
        .await;
        {
            let mut state = lock(&self.state);
            state.digest = Default::default();
            state.digest.last_sent = Some(today);
        }
        self.save_state()
    }

    /// The pause reason, when the pause flag is set.
    pub fn paused(&self) -> Option<String> {
        std::fs::read_to_string(self.home.pause_file())
            .ok()
            .map(|text| text.trim().to_owned())
    }

    /// Sets the pause flag.
    ///
    /// # Arguments
    ///
    /// * `reason` - Written into the flag file and shown by `factory status`.
    pub fn pause(&self, reason: &str) -> Result<()> {
        pause(&self.home, reason)
    }

    /// Clears the pause flag.
    pub fn resume(&self) -> Result<()> {
        resume(&self.home)
    }

    /// Pauses, counts it for the digest and tells the humans.
    async fn pause_and_notify(&self, reason: &str) -> Result<()> {
        tracing::warn!(reason, "pausing");
        self.pause(reason)?;
        lock(&self.state).digest.paused += 1;
        self.save_state()?;
        self.notify(Event::SelfPaused {
            reason: reason.to_owned(),
        })
        .await;
        Ok(())
    }

    /// Delivers an event; a failed delivery is logged, never fatal.
    async fn notify(&self, event: Event) {
        if let Err(error) = self.notifier.send(&event).await {
            tracing::warn!(%error, "notification not delivered: {}", event.text());
        }
    }

    /// A progress sink that updates the in-progress stage.
    fn progress(&self) -> Progress {
        let state = Arc::clone(&self.state);
        let board = Arc::clone(&self.board);
        let path = self.home.state_file();
        Arc::new(move |event: ProgressEvent| {
            board.progress(&event, Utc::now());
            let ProgressEvent::Stage { text, .. } = event else {
                return;
            };
            let mut guard = lock(&state);
            if let Some(current) = guard.in_progress.as_mut() {
                current.stage = text;
            }
            if let Err(error) = guard.save(&path) {
                tracing::warn!(%error, "cannot save the state");
            }
        })
    }

    fn set_in_progress(&self, current: Option<InProgress>) -> Result<()> {
        lock(&self.state).in_progress = current;
        self.save_state()
    }

    fn record(
        &self,
        id: Ulid,
        title: &str,
        kind: OutcomeKind,
        detail: &str,
        cost_usd: Option<f64>,
    ) -> Result<()> {
        self.record_outcome(OutcomeRecord {
            id: id.to_string(),
            title: title.to_owned(),
            kind,
            detail: detail.to_owned(),
            finished_at: Utc::now(),
            cost_usd,
            pr: None,
            started_at: None,
        })
    }

    /// Keeps an outcome in the state and the history, and shows it.
    fn record_outcome(&self, record: OutcomeRecord) -> Result<()> {
        if let Err(error) = history::append(&self.home.outcomes_file(), &record) {
            tracing::warn!(%error, "cannot append to the outcome history");
        }
        lock(&self.state).record(record);
        self.save_state()?;
        self.refresh_board();
        Ok(())
    }

    /// What the dashboard shows of a claimed ticket.
    fn ticket_view(&self, ticket: &Ticket, claimed_at: DateTime<Utc>) -> TicketView {
        let stages = Stage::ALL
            .iter()
            .map(|stage| StageView {
                stage: *stage,
                started_at: (*stage == Stage::Claim).then_some(claimed_at),
                ended_at: None,
            })
            .collect();
        TicketView {
            id: ticket.id.to_string(),
            title: ticket.title.clone(),
            kind: word(&ticket.kind),
            priority: word(&ticket.priority),
            symptom: ticket
                .symptom
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .to_owned(),
            claimed_at: Some(claimed_at),
            stages,
            detail: None,
            implementer_run: 0,
            implementer_runs_max: MAX_GATE_A_ATTEMPTS,
            review_round: 0,
            review_rounds_max: MAX_REVIEW_ROUNDS,
            budget: BudgetView {
                wall_clock_minutes: self.config.budget.wall_clock_minutes,
                tokens: self.config.budget.tokens,
            },
            pr: None,
        }
    }

    /// Rewrites the dashboard status every [`BOARD_TICK`] with the live
    /// implementer output, usage, container and egress, until aborted.
    fn spawn_ticker(&self, id: &str) -> tokio::task::JoinHandle<()> {
        let board = Arc::clone(&self.board);
        let probe = Arc::clone(&self.probe);
        let home = self.home.clone();
        let id = id.to_owned();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(BOARD_TICK);
            loop {
                interval.tick().await;
                let log = std::fs::read_to_string(home.output_log(&id)).unwrap_or_default();
                let parsed = tail::parse(&log);
                let egress = std::fs::read(home.egress_file(&id))
                    .ok()
                    .and_then(|bytes| serde_json::from_slice(&bytes).ok());
                let live = Live {
                    tail: parsed.lines,
                    state: parsed.state,
                    usage: probe.usage(&home.work_dir(&id)),
                    container: probe.container(&id).await,
                    egress,
                };
                board.tick(live);
            }
        })
    }

    /// Refreshes what the dashboard derives from the state, the history and
    /// the queue folders: the service state, today, the week, the queue,
    /// the waiting tickets and recent outcomes.
    fn refresh_board(&self) {
        let now_local = (self.clock)();
        let now = now_local.with_timezone(&Utc);
        let tz: FixedOffset = now_local.offset().fix();
        let outcomes = history::read(&self.home.outcomes_file());
        let queue: Vec<QueuedTicket> = self
            .queue
            .open_tickets()
            .map(|open| open.tickets.iter().map(queued).collect())
            .unwrap_or_default();
        let waiting = tickets_in(&self.queue.dir().join(dirs::WAITING_DIR));
        let paused = self.paused();
        let state = lock(&self.state).clone();
        let waiting_count = u32::try_from(waiting.len()).unwrap_or(u32::MAX);
        self.board.update(|status| {
            status.service.state = if status.ticket.is_some() {
                RunState::Running
            } else if paused.is_some() {
                RunState::Paused
            } else {
                RunState::Idle
            };
            status.service.reason = paused;
            status.recent = state.outcomes.iter().cloned().collect();
            status.today = history::today(&outcomes, now, tz);
            status.week = Some(history::week(&outcomes, now, tz, waiting_count));
            status.queue = queue;
            status.waiting = waiting;
            status.health.errors_in_a_row = state.consecutive_errors;
        });
    }

    /// Asks the probe about the machine, pings the providers once an hour
    /// and lists the pull requests waiting on a human.
    async fn refresh_health(&self) {
        let probed = self.probe.health().await;
        let ping_due = lock(&self.last_ping).is_none_or(|at| at.elapsed() >= PING_EVERY);
        let providers = if ping_due {
            *lock(&self.last_ping) = Some(Instant::now());
            Some(self.probe.ping_providers().await)
        } else {
            None
        };
        let needs_human = match self.host.needs_human_prs().await {
            Ok(list) => Some(list),
            Err(error) => {
                tracing::warn!(%error, "cannot list the pull requests waiting on a human");
                None
            }
        };
        self.board.update(|status| {
            let health = &mut status.health;
            health.image = probed.image;
            health.binary_version = probed.binary_version;
            health.disk_free_bytes = probed.disk_free_bytes;
            health.cargo_cache_bytes = probed.cargo_cache_bytes;
            health.baseline = probed.baseline.or(health.baseline.take());
            health.sandbox_ready = probed.sandbox_ready;
            if let Some(providers) = providers {
                health.providers = providers;
                health.pinged_at = Some(Utc::now());
            }
            if let Some(list) = needs_human {
                status.needs_human = list;
            }
        });
    }

    fn save_state(&self) -> Result<()> {
        lock(&self.state).save(&self.home.state_file())
    }
}

/// Writes the pause flag.
///
/// # Arguments
///
/// * `home` - `~/.factory`.
/// * `reason` - Why; shown by `factory status`.
///
/// # Errors
///
/// When the file cannot be written.
pub fn pause(home: &FactoryHome, reason: &str) -> Result<()> {
    write_atomic(&home.pause_file(), format!("{reason}\n").as_bytes())
}

/// Removes the pause flag; fine when it is already gone.
///
/// # Arguments
///
/// * `home` - `~/.factory`.
///
/// # Errors
///
/// When the file exists and cannot be removed.
pub fn resume(home: &FactoryHome) -> Result<()> {
    let path = home.pause_file();
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(FactoryError::io("removing", &path, source)),
    }
}

/// The pause reason, when a pause flag is set.
///
/// # Arguments
///
/// * `home` - `~/.factory`.
pub fn pause_reason(home: &FactoryHome) -> Option<String> {
    std::fs::read_to_string(home.pause_file())
        .ok()
        .map(|text| text.trim().to_owned())
}

/// The dashboard's view of main's CI.
fn ci_view(ci: &CiStatus, sha: Option<String>) -> CiView {
    let (state, sha) = match ci {
        CiStatus::Green => ("green", sha),
        CiStatus::Red { sha, .. } => ("red", Some(sha.clone())),
        CiStatus::Pending => ("pending", sha),
        CiStatus::Unknown => ("unknown", sha),
    };
    CiView {
        state: state.to_owned(),
        sha: sha.map(|sha| sha.chars().take(7).collect()),
    }
}

/// A ticket as a queue row.
fn queued(ticket: &Ticket) -> QueuedTicket {
    QueuedTicket {
        id: ticket.id.to_string(),
        priority: word(&ticket.priority),
        title: ticket.title.clone(),
    }
}

/// The valid tickets in a queue folder, as queue rows.
fn tickets_in(dir: &Path) -> Vec<QueuedTicket> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut tickets: Vec<Ticket> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .filter_map(|text| toml::from_str(&text).ok())
        .collect();
    tickets.sort_by_key(|ticket| ticket.id);
    tickets.iter().map(queued).collect()
}

/// How an enum serializes, such as `p2` or `bug`.
fn word<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// This machine's name, from the `hostname` command, else the environment,
/// else `unknown`.
///
/// # Returns
///
/// The hostname.
pub fn hostname() -> String {
    let from_command = std::process::Command::new("hostname")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .filter(|name| !name.is_empty());
    from_command
        .or_else(|| std::env::var("HOSTNAME").ok())
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .unwrap_or_else(|| "unknown".to_owned())
}

/// The system clock.
pub fn system_clock() -> Clock {
    Arc::new(Local::now)
}

/// Locks a mutex, recovering from a poisoned one.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
