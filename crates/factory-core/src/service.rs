//! The service loop: poll the state branch, take the next ticket, work it,
//! record what happened.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anvil_types::{FactoryConfig, TicketKind};
use chrono::{DateTime, Local, NaiveTime, Utc};
use ulid::Ulid;

use crate::error::{FactoryError, Result};
use crate::notify::{Event, Notifier};
use crate::paths::FactoryHome;
use crate::ports::{CiStatus, CodeHost, Outcome, Progress, Worker};
use crate::status::{InProgress, OutcomeKind, OutcomeRecord, ServiceState};
use crate::tickets::{MODEL_NOTE, TicketQueue, Transition};

/// Errors in a row after which the loop pauses itself.
pub const ERRORS_BEFORE_PAUSE: u32 = 2;

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
        Ok(Self {
            home: deps.home,
            config: deps.config,
            queue: deps.queue,
            worker: deps.worker,
            host: deps.host,
            notifier: deps.notifier,
            hostname: deps.hostname,
            clock: deps.clock,
            state: Arc::new(Mutex::new(state)),
        })
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
            let tick = match self.run_once().await {
                Ok(tick) => tick,
                Err(error) => {
                    tracing::error!(%error, "poll failed");
                    self.count_error(None, &error).await;
                    Tick::Idle
                }
            };
            if !matches!(tick, Tick::Worked(_)) {
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
        if let Some(reason) = self.paused() {
            tracing::info!(reason, "paused; not taking tickets");
            return Ok(Tick::Paused(reason));
        }
        self.queue.refresh().await?;
        if let CiStatus::Red { sha, url } = self.host.main_ci().await? {
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
        self.set_in_progress(Some(InProgress {
            id: id.to_string(),
            title: title.clone(),
            stage: "claimed".to_owned(),
            since: claimed.claimed_at.unwrap_or_else(Utc::now),
        }))?;
        let progress = self.progress();
        let result = self.worker.work(&claimed, &self.queue, progress).await;
        self.set_in_progress(None)?;
        self.finish(id, &title, result).await?;
        Ok(Tick::Worked(id))
    }

    /// Applies the worker's outcome to the queue, the state and the notifier.
    async fn finish(&self, id: Ulid, title: &str, result: Result<Outcome>) -> Result<()> {
        match result {
            Ok(Outcome::Merged { pr, cost_usd }) => {
                self.queue.transition(id, Transition::Merged { pr }).await?;
                self.record(id, title, OutcomeKind::Merged, &format!("#{pr}"), cost_usd)?;
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
                self.record(id, title, OutcomeKind::Waiting, &reason, cost_usd)?;
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
                self.record(id, title, OutcomeKind::Rejected, &reason, cost_usd)?;
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
                self.record(id, title, OutcomeKind::Reopened, &reason, None)?;
                self.pause_and_notify(&reason).await?;
            }
            Err(error) => {
                tracing::error!(%id, %error, "working the ticket failed; reopening it");
                self.queue.transition(id, Transition::Reopen).await?;
                self.record(id, title, OutcomeKind::Reopened, &error.to_string(), None)?;
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
        let path = self.home.state_file();
        Arc::new(move |stage: &str| {
            let mut guard = lock(&state);
            if let Some(current) = guard.in_progress.as_mut() {
                current.stage = stage.to_owned();
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
        lock(&self.state).record(OutcomeRecord {
            id: id.to_string(),
            title: title.to_owned(),
            kind,
            detail: detail.to_owned(),
            finished_at: Utc::now(),
            cost_usd,
        });
        self.save_state()
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
    let path = home.pause_file();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|source| FactoryError::io("creating", dir, source))?;
    }
    std::fs::write(&path, format!("{reason}\n"))
        .map_err(|source| FactoryError::io("writing", &path, source))
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
