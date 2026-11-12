//! Session status: a `status.json` in every session folder that says
//! whether the session is working, waiting for the owner, blocked, done or
//! failed, plus the things that follow a change of state (a webhook
//! message, a report to herdr) and `anvil status`, which reads the files.
//!
//! The mapping from what a session observes to a state is pure and lives
//! in [`tracker`]; this file wires it to the files and processes.
//!
//! "A prompt is waiting" is signalled where the prompts are asked, above
//! both front ends: [`StatusReporter::prompt`], [`StatusReporter::gate`]
//! and [`StatusReporter::decider`] wrap whatever the front end (the line
//! renderer's stdin prompts or the full-screen UI's modals) provides.

mod file;
mod herdr;
pub mod list;
mod notify;
mod tracker;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use anvil_core::agent::{LoopEvent, PlanDecision, PlanGate, Status};
use anvil_core::tools::{PermissionPrompt, PermissionRequest, PromptAnswer};
use anvil_memory::{Decision, Proposal};
use anvil_types::status::SCHEMA_VERSION;
use anvil_types::{SessionState, SessionStatus};
use chrono::{DateTime, Utc};
use tokio::task::JoinHandle;

use self::file::StatusFile;
pub(crate) use self::file::write_atomic;
use self::herdr::{HerdrEnv, HerdrReporter, Report, report_for};
use self::notify::{Message, Webhook, webhook_url};
use self::tracker::{Prices, Snapshot, Tracker, one_line};
use crate::wiring::Wiring;

/// How long the end of a session waits for its last webhook posts to go out.
const FLUSH_WAIT: Duration = Duration::from_secs(3);

/// Everything a reporter needs to know about its session.
#[derive(Debug)]
pub struct Setup {
    /// The project root.
    pub project: PathBuf,
    /// The anvil home, where the project is registered for `anvil status --all`.
    pub home: Option<PathBuf>,
    /// The session id.
    pub session_id: String,
    /// The session folder.
    pub session_dir: PathBuf,
    /// The task; only its first line is kept.
    pub task: String,
    /// Prices for the cost estimate.
    pub prices: Prices,
    /// The webhook URL, when notifications are on.
    pub webhook: Option<String>,
    /// The herdr pane, when reporting is on.
    pub herdr: Option<HerdrEnv>,
}

impl Setup {
    /// Reads the configuration and the environment for one session.
    ///
    /// # Arguments
    ///
    /// * `wiring` - Config and paths.
    /// * `session_id` - The session id.
    /// * `session_dir` - The session folder.
    /// * `task` - The task.
    ///
    /// # Returns
    ///
    /// The setup; the webhook and herdr parts are `None` unless configured
    /// and, for herdr, running in a herdr pane.
    pub fn from_wiring(wiring: &Wiring, session_id: &str, session_dir: &Path, task: &str) -> Self {
        let env = |name: &str| std::env::var(name).ok();
        let config = &wiring.config;
        Self {
            project: wiring.cwd.clone(),
            home: wiring.home.clone(),
            session_id: session_id.to_owned(),
            session_dir: session_dir.to_path_buf(),
            task: task.to_owned(),
            prices: crate::frontend::role_price_map(config),
            webhook: webhook_url(
                config
                    .notify
                    .as_ref()
                    .map(|notify| notify.webhook_env.as_str()),
                env,
            ),
            herdr: HerdrEnv::detect(config.integrations.herdr, env),
        }
    }
}

/// Keeps one session's `status.json` current. Cloning shares the session.
///
/// Nothing here fails the session: a file that cannot be written, a
/// webhook that is down and a `herdr` that is missing are logged and skipped.
#[derive(Clone)]
pub struct StatusReporter {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    session_id: String,
    project: String,
    task: String,
    started_at: DateTime<Utc>,
    tracker: Tracker,
    file: Option<StatusFile>,
    /// What the file says now, so an event that changes nothing writes nothing.
    written: Option<Snapshot>,
    /// The state last announced, so a state is announced once per entry.
    announced: SessionState,
    webhook: Option<Webhook>,
    herdr: Option<HerdrReporter>,
    last_herdr: Option<Report>,
    runtime: Option<tokio::runtime::Handle>,
    posts: Vec<JoinHandle<()>>,
}

impl StatusReporter {
    /// Takes the session's lock, writes the first `status.json` and
    /// registers the project.
    ///
    /// # Arguments
    ///
    /// * `setup` - The session.
    ///
    /// # Returns
    ///
    /// The reporter, in the `working` state. If the folder cannot be
    /// written the reporter still works and reports nothing to disk.
    pub fn start(setup: Setup) -> Self {
        let file = StatusFile::open(&setup.session_dir)
            .inspect_err(|error| tracing::warn!(%error, "session status is not written"))
            .ok();
        if let Some(home) = &setup.home
            && let Err(error) = file::register_project(home, &setup.project)
        {
            tracing::warn!(%error, "the project is not registered for `anvil status --all`");
        }
        let runtime = tokio::runtime::Handle::try_current().ok();
        let herdr = match (&runtime, setup.herdr) {
            (Some(runtime), Some(env)) => Some(HerdrReporter::spawn(env, runtime)),
            _ => None,
        };
        let tracker = Tracker::new(setup.prices);
        let mut inner = Inner {
            session_id: setup.session_id,
            project: setup.project.display().to_string(),
            task: setup.task.lines().next().unwrap_or("").trim().to_owned(),
            started_at: Utc::now(),
            tracker,
            file,
            written: None,
            announced: SessionState::Working,
            webhook: setup.webhook.as_deref().and_then(Webhook::new),
            herdr,
            last_herdr: None,
            runtime,
            posts: Vec::new(),
        };
        inner.publish();
        Self {
            inner: Arc::new(Mutex::new(inner)),
        }
    }

    /// Takes in one loop event.
    ///
    /// # Arguments
    ///
    /// * `event` - What the loop did.
    pub fn observe(&self, event: &LoopEvent) {
        self.update(|tracker| tracker.observe(event));
    }

    /// Notes that something now waits for the owner.
    fn waiting(&self, what: &str) {
        self.update(|tracker| tracker.prompt_waiting(what));
    }

    /// Notes that the owner answered.
    fn answered(&self) {
        self.update(Tracker::prompt_answered);
    }

    /// Ends the session the way the loop ended.
    ///
    /// # Arguments
    ///
    /// * `status` - The loop's final status.
    pub fn finish_run(&self, status: &Status) {
        self.update(|tracker| tracker.finish_run(status));
    }

    /// Says what the session is doing, for a build that drives sessions.
    ///
    /// # Arguments
    ///
    /// * `detail` - One line.
    pub fn note_working(&self, detail: &str) {
        self.update(|tracker| tracker.working(detail));
    }

    /// Ends the session in a state the caller chose, such as a build that
    /// stops as `blocked` or `needs_input`.
    ///
    /// # Arguments
    ///
    /// * `state` - The state; `needs_input` stays until the file is replaced,
    ///   as a question nobody answers.
    /// * `detail` - Why, in one line.
    pub fn finish_as(&self, state: SessionState, detail: &str) {
        self.update(|tracker| match state {
            SessionState::NeedsInput => tracker.prompt_waiting(detail),
            _ => tracker.finish(state, detail),
        });
    }

    /// What the session has used so far.
    ///
    /// # Returns
    ///
    /// Tokens and, when a price is configured, cost.
    pub fn spend(&self) -> anvil_types::SessionSpend {
        self.lock().tracker.snapshot().spend
    }

    /// Ends the session as done, for a chat the owner closed.
    ///
    /// # Arguments
    ///
    /// * `detail` - Why, in one line.
    pub fn finish_done(&self, detail: &str) {
        self.update(|tracker| tracker.finish(SessionState::Done, detail));
    }

    /// Ends the session as failed, when an error aborts the command.
    ///
    /// # Arguments
    ///
    /// * `detail` - The error, in one line.
    pub fn fail(&self, detail: &str) {
        self.update(|tracker| tracker.finish(SessionState::Failed, detail));
    }

    /// Waits briefly for the last webhook posts, then tells herdr the
    /// session is gone. Call it once, when the command ends.
    pub async fn shutdown(&self) {
        let (posts, herdr) = {
            let mut inner = self.lock();
            (std::mem::take(&mut inner.posts), inner.herdr.take())
        };
        let deadline = tokio::time::Instant::now() + FLUSH_WAIT;
        for post in posts {
            let _ = tokio::time::timeout_at(deadline, post).await;
        }
        if let Some(herdr) = herdr {
            herdr.release().await;
        }
    }

    /// Wraps a permission prompt so a question waiting for the owner shows
    /// as `needs_input`.
    ///
    /// # Arguments
    ///
    /// * `yes` - Whether the run is unattended; nothing waits then, so the
    ///   prompt is returned as it is.
    /// * `inner` - The front end's prompt.
    ///
    /// # Returns
    ///
    /// The prompt to register.
    pub fn prompt(&self, yes: bool, inner: Box<dyn PermissionPrompt>) -> Box<dyn PermissionPrompt> {
        if yes {
            return inner;
        }
        Box::new(WatchedPrompt {
            inner,
            status: self.clone(),
        })
    }

    /// Wraps a plan gate so a plan waiting for approval shows as `needs_input`.
    ///
    /// # Arguments
    ///
    /// * `yes` - Whether the run is unattended; plans are approved at once.
    /// * `inner` - The front end's gate.
    ///
    /// # Returns
    ///
    /// The gate to give the loop.
    pub fn gate(&self, yes: bool, inner: Box<dyn PlanGate>) -> Box<dyn PlanGate> {
        if yes {
            return inner;
        }
        Box::new(WatchedGate {
            inner,
            status: self.clone(),
        })
    }

    /// Wraps the memory keep/drop question so it shows as `needs_input`.
    ///
    /// # Arguments
    ///
    /// * `yes` - Whether the run is unattended; everything is kept at once.
    /// * `inner` - The front end's decider.
    ///
    /// # Returns
    ///
    /// The decider to give the memory step.
    pub fn decider(
        &self,
        yes: bool,
        mut inner: Box<dyn FnMut(&Proposal) -> Decision>,
    ) -> Box<dyn FnMut(&Proposal) -> Decision> {
        if yes {
            return inner;
        }
        let status = self.clone();
        Box::new(move |proposal| {
            status.waiting("question: keep this remembered fact?");
            let decision = inner(proposal);
            status.answered();
            decision
        })
    }

    fn update(&self, change: impl FnOnce(&mut Tracker)) {
        let mut inner = self.lock();
        change(&mut inner.tracker);
        inner.publish();
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Inner {
    /// Writes the file, announces a new state and tells herdr, for
    /// whatever changed since the last call.
    fn publish(&mut self) {
        let snapshot = self.tracker.snapshot();
        if self.written.as_ref() == Some(&snapshot) {
            return;
        }
        let now = Utc::now();
        if let Some(file) = &self.file {
            let status = SessionStatus {
                schema: SCHEMA_VERSION,
                session_id: self.session_id.clone(),
                project: self.project.clone(),
                task: self.task.clone(),
                state: snapshot.state,
                loop_state: snapshot.loop_state,
                detail: snapshot.detail.clone(),
                spend: snapshot.spend,
                started_at: self.started_at,
                updated_at: now,
            };
            if let Err(error) = file.write(&status) {
                tracing::warn!(%error, "cannot write status.json");
            }
        }
        if snapshot.state != self.announced {
            self.announced = snapshot.state;
            self.announce(&snapshot);
        }
        self.tell_herdr(&snapshot);
        self.written = Some(snapshot);
    }

    /// Posts one message for a state just entered; nothing for `working`.
    fn announce(&mut self, snapshot: &Snapshot) {
        let (Some(webhook), Some(runtime)) = (&self.webhook, &self.runtime) else {
            return;
        };
        let task = one_line(&self.task);
        let Some(message) = Message::for_state(snapshot.state, &task, &snapshot.detail) else {
            return;
        };
        let webhook = webhook.clone();
        self.posts
            .push(runtime.spawn(async move { webhook.send(&message).await }));
    }

    /// Reports a changed herdr state; a detail change while working is not one.
    fn tell_herdr(&mut self, snapshot: &Snapshot) {
        let Some(herdr) = &self.herdr else {
            return;
        };
        let report = report_for(snapshot.state, &snapshot.detail);
        if self.last_herdr.as_ref() != Some(&report) {
            herdr.send(report.clone());
            self.last_herdr = Some(report);
        }
    }
}

/// A permission prompt that marks the session as waiting while it asks.
struct WatchedPrompt {
    inner: Box<dyn PermissionPrompt>,
    status: StatusReporter,
}

impl PermissionPrompt for WatchedPrompt {
    fn ask(&self, request: &PermissionRequest) -> PromptAnswer {
        let target = request.target.as_deref().unwrap_or("");
        self.status
            .waiting(&format!("permission: {} {target}", request.tool));
        let answer = self.inner.ask(request);
        self.status.answered();
        answer
    }
}

/// A plan gate that marks the session as waiting while the plan is read.
struct WatchedGate {
    inner: Box<dyn PlanGate>,
    status: StatusReporter,
}

impl PlanGate for WatchedGate {
    fn review(&self, plan: &str) -> PlanDecision {
        self.status.waiting("plan approval");
        let decision = self.inner.review(plan);
        self.status.answered();
        decision
    }
}

#[cfg(test)]
mod tests;
