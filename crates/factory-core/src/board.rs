//! The dashboard snapshot in `~/.factory/status.json`: what `factory watch`
//! draws.
//!
//! The service writes it on every state change and every two seconds while
//! a ticket is in progress; the dashboard only reads it, so it also works
//! over ssh. Every field past the version has a default, so a file written
//! by an older or newer service still renders whatever it carries.

use std::path::{Path, PathBuf};

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{FactoryError, Result};
use crate::status::OutcomeRecord;

/// The schema version written into every snapshot.
pub const STATUS_VERSION: u32 = 1;

/// How many implementer output lines the snapshot keeps.
pub const TAIL_LINES: usize = 30;

/// Everything the dashboard shows.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Status {
    /// The schema version, [`STATUS_VERSION`].
    pub v: u32,
    /// When the service wrote this snapshot.
    pub written_at: Option<DateTime<Utc>>,
    /// The machine the service runs on.
    pub host: String,
    /// `owner/name` of the repository, for pull request links.
    pub repo: String,
    /// Running, idle or paused.
    pub service: ServiceView,
    /// When the service process started.
    pub started_at: Option<DateTime<Utc>>,
    /// When the next poll is due; none while a ticket is being worked.
    pub next_poll_at: Option<DateTime<Utc>>,
    /// The last poll of the queue.
    pub last_poll: Option<PollView>,
    /// Today's outcomes, in the service's local day.
    pub today: DayCounts,
    /// The ticket in progress.
    pub ticket: Option<TicketView>,
    /// The last [`TAIL_LINES`] lines of implementer output, oldest first.
    pub implementer_tail: Vec<TailLine>,
    /// Tokens and cost of the ticket in progress.
    pub usage: Option<UsageView>,
    /// The container running a step of the ticket in progress.
    pub container: Option<ContainerView>,
    /// The egress proxy's counters for the ticket in progress.
    pub egress: Option<EgressView>,
    /// The open queue in the order it will be worked.
    pub queue: Vec<QueuedTicket>,
    /// The latest outcomes, newest first.
    pub recent: Vec<OutcomeRecord>,
    /// Tickets in `waiting/`.
    pub waiting: Vec<QueuedTicket>,
    /// Open pull requests labelled for a human.
    pub needs_human: Vec<NeedsHuman>,
    /// The last seven days.
    pub week: Option<WeekView>,
    /// Whether the machine and its dependencies are healthy.
    pub health: HealthView,
}

/// What the service is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    /// Working a ticket.
    Running,
    /// Waiting for the next poll.
    #[default]
    Idle,
    /// The pause flag is set.
    Paused,
}

/// The service state and, when paused, why.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ServiceView {
    /// Running, idle or paused.
    pub state: RunState,
    /// The pause reason.
    pub reason: Option<String>,
}

/// One poll of the queue.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PollView {
    /// When it started.
    pub at: DateTime<Utc>,
    /// How long it took, in seconds.
    pub secs: f64,
}

/// Outcome counts and spend over one span of time.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DayCounts {
    /// Tickets merged.
    pub merged: u32,
    /// Tickets rejected.
    pub rejected: u32,
    /// Model cost over the outcomes with a known cost.
    pub cost_usd: f64,
}

/// A pipeline stage, in the order a ticket goes through them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// The ticket moved to `claimed/`.
    Claim,
    /// The worktree and the sandbox are prepared.
    Worktree,
    /// The implementer runs, with gate A's fix-up runs.
    Implement,
    /// `just check` in the sandbox.
    GateA,
    /// The release build and the eval against main.
    GateB,
    /// The review swarm.
    GateC,
    /// The branch is pushed and the pull request opened.
    Pr,
    /// CI on the pull request, then the squash merge.
    Merge,
}

impl Stage {
    /// Every stage in pipeline order.
    pub const ALL: [Stage; 8] = [
        Stage::Claim,
        Stage::Worktree,
        Stage::Implement,
        Stage::GateA,
        Stage::GateB,
        Stage::GateC,
        Stage::Pr,
        Stage::Merge,
    ];

    /// The stage's name on the dashboard.
    ///
    /// # Returns
    ///
    /// `claim`, `worktree`, `implement`, `gate A`, `gate B`, `gate C`, `PR`
    /// or `merge`.
    pub fn label(self) -> &'static str {
        match self {
            Stage::Claim => "claim",
            Stage::Worktree => "worktree",
            Stage::Implement => "implement",
            Stage::GateA => "gate A",
            Stage::GateB => "gate B",
            Stage::GateC => "gate C",
            Stage::Pr => "PR",
            Stage::Merge => "merge",
        }
    }
}

/// When one stage started and ended.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StageView {
    /// The stage.
    pub stage: Stage,
    /// When it started; none while pending.
    #[serde(default)]
    pub started_at: Option<DateTime<Utc>>,
    /// When it ended; none while pending or running.
    #[serde(default)]
    pub ended_at: Option<DateTime<Utc>>,
}

/// The run's limits from `[budget]` in `factory.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BudgetView {
    /// `[budget] wall_clock_minutes`.
    pub wall_clock_minutes: u64,
    /// `[budget] tokens`.
    pub tokens: u64,
}

/// The ticket in progress.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TicketView {
    /// The ticket id.
    pub id: String,
    /// The title.
    pub title: String,
    /// `bug`, `tool` and so on.
    pub kind: String,
    /// `p0`, `p1` or `p2`.
    pub priority: String,
    /// The symptom, the ticket's first required field.
    pub symptom: String,
    /// When the service claimed it.
    pub claimed_at: Option<DateTime<Utc>>,
    /// Every stage of [`Stage::ALL`], in order.
    pub stages: Vec<StageView>,
    /// What anvil is doing inside the running stage, such as `build`.
    pub detail: Option<String>,
    /// Which implementer run this is, from 1.
    pub implementer_run: u32,
    /// The most implementer runs gate A allows.
    pub implementer_runs_max: u32,
    /// Review rounds that asked for changes so far.
    pub review_round: u32,
    /// The most review rounds allowed.
    pub review_rounds_max: u32,
    /// The run's limits.
    pub budget: BudgetView,
    /// The pull request, once opened.
    pub pr: Option<u64>,
}

impl TicketView {
    /// The stage running now: the first one started and not ended.
    ///
    /// # Returns
    ///
    /// The stage, or none before the claim and after the merge.
    pub fn running_stage(&self) -> Option<Stage> {
        self.stages
            .iter()
            .find(|view| view.started_at.is_some() && view.ended_at.is_none())
            .map(|view| view.stage)
    }
}

/// One line of implementer output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TailLine {
    /// A tool call.
    Tool {
        /// `ok`, `failed`, `denied` and so on; `..` while running.
        status: String,
        /// The tool name.
        name: String,
        /// The argument summary anvil printed.
        args: String,
        /// How long the call took, when its start was seen.
        #[serde(default)]
        millis: Option<u64>,
        /// Whether the call is still running.
        #[serde(default)]
        running: bool,
    },
    /// Model text or any other line.
    Text {
        /// The line.
        text: String,
    },
}

/// Tokens and cost so far.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UsageView {
    /// Input tokens.
    pub input: u64,
    /// Output tokens.
    pub output: u64,
    /// Cached input tokens.
    pub cached: u64,
    /// The cost, when every price was known.
    pub cost_usd: Option<f64>,
    /// Context tokens sent on each turn, oldest first.
    pub context_per_turn: Vec<u64>,
    /// The context size the chart is scaled to: the coder's compaction
    /// threshold, when known.
    pub context_limit: Option<u64>,
}

/// The container running a step, from `podman stats`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ContainerView {
    /// The short container id.
    pub id: String,
    /// CPU use in percent of one core.
    pub cpu_pct: Option<f64>,
    /// Memory in use, in bytes.
    pub mem_bytes: Option<u64>,
}

/// The egress proxy's counters.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct EgressView {
    /// Connections allowed.
    pub allowed: u64,
    /// Connections refused.
    pub denied: u64,
    /// The last refused `host:port`.
    pub last_denied: Option<String>,
}

/// A ticket in a queue folder.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct QueuedTicket {
    /// The ticket id.
    pub id: String,
    /// `p0`, `p1` or `p2`.
    pub priority: String,
    /// The title.
    pub title: String,
}

/// An open pull request labelled `needs-human`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NeedsHuman {
    /// The pull request number.
    pub pr: u64,
    /// Its title.
    pub title: String,
    /// The top-level folders or files it changes.
    pub touches: Vec<String>,
}

/// One day of the week view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DayBar {
    /// The day.
    pub day: NaiveDate,
    /// Tickets merged.
    pub merged: u32,
    /// Tickets rejected.
    pub rejected: u32,
}

/// The last seven days, oldest day first.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WeekView {
    /// One bar per day.
    pub days: Vec<DayBar>,
    /// Totals over the days.
    pub totals: DayCounts,
    /// Tickets waiting on a human now.
    pub waiting: u32,
    /// The median time from claim to outcome, in seconds.
    pub median_secs: Option<u64>,
}

/// Main's CI.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CiView {
    /// `green`, `red`, `pending` or `unknown`.
    pub state: String,
    /// The commit, when known.
    pub sha: Option<String>,
}

/// The newest eval baseline on main.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BaselineView {
    /// Tasks passed.
    pub passed: u32,
    /// Tasks run.
    pub total: u32,
    /// The model it ran on.
    pub model: Option<String>,
}

/// The worker image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageView {
    /// The image name.
    pub name: String,
    /// The short image id.
    pub id: String,
    /// When it was built.
    pub created: DateTime<Utc>,
}

/// The latency of one provider's last ping.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderLatency {
    /// The provider name.
    pub name: String,
    /// The round trip in milliseconds; none when the ping failed.
    pub millis: Option<u64>,
}

/// Health of the machine and what the factory depends on.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct HealthView {
    /// Main's CI.
    pub main_ci: Option<CiView>,
    /// The newest eval baseline.
    pub baseline: Option<BaselineView>,
    /// The worker image.
    pub image: Option<ImageView>,
    /// `anvil --version` of the binary the sandbox mounts.
    pub binary_version: Option<String>,
    /// Free bytes on the disk holding `~/.factory`.
    pub disk_free_bytes: Option<u64>,
    /// Bytes in the cargo cache volume.
    pub cargo_cache_bytes: Option<u64>,
    /// Errors in a row from the loop.
    pub errors_in_a_row: u32,
    /// The last ping of each provider.
    pub providers: Vec<ProviderLatency>,
    /// When the providers were last pinged.
    pub pinged_at: Option<DateTime<Utc>>,
    /// Whether the sandbox preflight passed.
    pub sandbox_ready: Option<bool>,
}

impl Status {
    /// Writes the snapshot in one step, so a reader never sees half of it.
    ///
    /// # Arguments
    ///
    /// * `path` - `~/.factory/status.json`.
    ///
    /// # Errors
    ///
    /// When the snapshot cannot be serialized or written.
    pub fn save(&self, path: &Path) -> Result<()> {
        let text = serde_json::to_vec_pretty(self).map_err(|source| FactoryError::Json {
            path: path.to_path_buf(),
            source,
        })?;
        write_atomic(path, &text)
    }
}

/// Writes a file through a temporary file in the same folder and a rename,
/// so readers see the old content or the new, never a mix.
///
/// # Arguments
///
/// * `path` - The file.
/// * `bytes` - Its new content.
///
/// # Errors
///
/// When the folder cannot be created or the file cannot be written or
/// renamed.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|source| FactoryError::io("creating", dir, source))?;
    }
    let mut temp = path.as_os_str().to_owned();
    temp.push(format!(".{}.tmp", std::process::id()));
    let temp = PathBuf::from(temp);
    std::fs::write(&temp, bytes).map_err(|source| FactoryError::io("writing", &temp, source))?;
    std::fs::rename(&temp, path).map_err(|source| FactoryError::io("replacing", path, source))
}

/// Reads the snapshot, keeping the last good one when a read fails.
#[derive(Debug, Default)]
pub struct StatusReader {
    last_good: Option<Status>,
    last_error: Option<String>,
}

impl StatusReader {
    /// A reader with nothing read yet.
    ///
    /// # Returns
    ///
    /// The reader.
    pub fn new() -> Self {
        Self::default()
    }

    /// Reads the file again.
    ///
    /// # Arguments
    ///
    /// * `path` - `~/.factory/status.json`.
    ///
    /// # Returns
    ///
    /// The snapshot just read, or the last good one when the file is
    /// missing, unreadable or half-written; none before the first good read.
    pub fn poll(&mut self, path: &Path) -> Option<&Status> {
        let parsed = std::fs::read_to_string(path)
            .map_err(|error| error.to_string())
            .and_then(|text| {
                serde_json::from_str::<Status>(&text).map_err(|error| error.to_string())
            });
        match parsed {
            Ok(status) => {
                self.last_good = Some(status);
                self.last_error = None;
            }
            Err(error) => self.last_error = Some(error),
        }
        self.last_good.as_ref()
    }

    /// Why the last read failed.
    ///
    /// # Returns
    ///
    /// The error text, or none when the last read succeeded.
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Status {
        Status {
            v: STATUS_VERSION,
            host: "minipc".to_owned(),
            ticket: Some(TicketView {
                id: "01M6D210H709VGC2CFEG4BTQ7M".to_owned(),
                stages: Stage::ALL
                    .iter()
                    .map(|stage| StageView {
                        stage: *stage,
                        started_at: None,
                        ended_at: None,
                    })
                    .collect(),
                ..TicketView::default()
            }),
            implementer_tail: vec![
                TailLine::Tool {
                    status: "ok".to_owned(),
                    name: "grep".to_owned(),
                    args: "fn permission".to_owned(),
                    millis: Some(9),
                    running: false,
                },
                TailLine::Text {
                    text: "Approvals are cached.".to_owned(),
                },
            ],
            ..Status::default()
        }
    }

    #[test]
    fn round_trips_and_tolerates_missing_fields() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("status.json");
        sample().save(&path).expect("save");
        let mut reader = StatusReader::new();
        assert_eq!(reader.poll(&path), Some(&sample()));
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .expect("dir")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        assert_eq!(names, vec!["status.json"], "no temp file left behind");

        std::fs::write(&path, r#"{"v":1,"host":"nuc"}"#).expect("write");
        let status = reader.poll(&path).expect("read");
        assert_eq!(status.host, "nuc");
        assert_eq!(status.service.state, RunState::Idle);
    }

    #[test]
    fn a_half_written_file_keeps_the_last_good_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("status.json");
        let mut reader = StatusReader::new();
        assert_eq!(reader.poll(&path), None);
        assert!(reader.last_error().is_some());

        sample().save(&path).expect("save");
        assert!(reader.poll(&path).is_some());
        let text = std::fs::read_to_string(&path).expect("read");
        std::fs::write(&path, &text[..text.len() / 2]).expect("truncate");
        assert_eq!(reader.poll(&path), Some(&sample()));
        assert!(reader.last_error().is_some());

        let mut newer = sample();
        newer.host = "other".to_owned();
        newer.save(&path).expect("save");
        assert_eq!(reader.poll(&path).map(|s| s.host.as_str()), Some("other"));
        assert_eq!(reader.last_error(), None);
    }

    #[test]
    fn a_reader_racing_the_writer_never_sees_partial_json() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("status.json");
        sample().save(&path).expect("save");
        let writer_path = path.clone();
        let writer = std::thread::spawn(move || {
            let mut status = sample();
            for n in 0..1000 {
                status.host = format!("host-{n}");
                status.save(&writer_path).expect("save");
            }
        });
        let mut reads = 0;
        while !writer.is_finished() {
            let text = std::fs::read_to_string(&path).expect("read");
            serde_json::from_str::<Status>(&text).expect("whole JSON");
            reads += 1;
        }
        writer.join().expect("writer");
        assert!(reads > 0);
    }

    #[test]
    fn the_running_stage_is_the_first_unfinished_one() {
        let mut ticket = sample().ticket.expect("ticket");
        assert_eq!(ticket.running_stage(), None);
        let now = Utc::now();
        ticket.stages[0].started_at = Some(now);
        ticket.stages[0].ended_at = Some(now);
        ticket.stages[1].started_at = Some(now);
        assert_eq!(ticket.running_stage(), Some(Stage::Worktree));
    }
}
