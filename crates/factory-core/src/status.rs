//! The service state in `~/.factory/state.json`, and the text `factory status` prints.

use std::collections::{BTreeSet, VecDeque};
use std::path::Path;

use anvil_types::Ticket;
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{FactoryError, Result};

/// How many outcomes the state keeps.
pub const KEPT_OUTCOMES: usize = 10;

/// The ticket being worked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InProgress {
    /// The ticket id.
    pub id: String,
    /// The ticket title.
    pub title: String,
    /// What the worker is doing, such as `implementer attempt 1` or `gate B`.
    pub stage: String,
    /// When the ticket was claimed.
    pub since: DateTime<Utc>,
}

/// How a ticket left the worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeKind {
    /// Squash-merged to main.
    Merged,
    /// Moved to `rejected/`.
    Rejected,
    /// Moved to `waiting/`.
    Waiting,
    /// Put back into `open/` after an error.
    Reopened,
}

/// One finished ticket.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutcomeRecord {
    /// The ticket id.
    pub id: String,
    /// The ticket title.
    pub title: String,
    /// How it ended.
    pub kind: OutcomeKind,
    /// The pull request number, the rejection reason or the waiting note.
    pub detail: String,
    /// When.
    pub finished_at: DateTime<Utc>,
    /// What the ticket cost in model calls, when every price was known.
    pub cost_usd: Option<f64>,
}

/// Counts since the last digest.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct DigestCounters {
    /// The day the last digest went out.
    pub last_sent: Option<NaiveDate>,
    /// Tickets merged.
    pub merged: u32,
    /// Tickets rejected.
    pub rejected: u32,
    /// Tickets parked for a human.
    pub waiting: u32,
    /// Times the service paused itself.
    pub paused: u32,
    /// Model cost over every outcome with a known cost.
    pub cost_usd: f64,
}

/// Everything `factory status` shows that is not in the queue itself.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ServiceState {
    /// The ticket being worked, if any.
    pub in_progress: Option<InProgress>,
    /// The last [`KEPT_OUTCOMES`] outcomes, newest first.
    pub outcomes: VecDeque<OutcomeRecord>,
    /// Counts for the daily digest.
    pub digest: DigestCounters,
    /// `branch@sha` of every refused ticket branch already reported.
    pub refused: BTreeSet<String>,
    /// Errors in a row from the loop; two pause the service.
    pub consecutive_errors: u32,
}

impl ServiceState {
    /// Reads the state, or the default when the file does not exist.
    ///
    /// # Arguments
    ///
    /// * `path` - `~/.factory/state.json`.
    ///
    /// # Returns
    ///
    /// The state.
    ///
    /// # Errors
    ///
    /// When the file exists but cannot be read or parsed.
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).map_err(|source| FactoryError::Json {
                path: path.to_path_buf(),
                source,
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(FactoryError::io("reading", path, source)),
        }
    }

    /// Writes the state, replacing the file in one step.
    ///
    /// # Arguments
    ///
    /// * `path` - `~/.factory/state.json`.
    ///
    /// # Errors
    ///
    /// When the folder cannot be created or the file cannot be written.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|source| FactoryError::io("creating", dir, source))?;
        }
        let text = serde_json::to_string_pretty(self).map_err(|source| FactoryError::Json {
            path: path.to_path_buf(),
            source,
        })?;
        let temp = path.with_extension("json.tmp");
        std::fs::write(&temp, text).map_err(|source| FactoryError::io("writing", &temp, source))?;
        std::fs::rename(&temp, path).map_err(|source| FactoryError::io("replacing", path, source))
    }

    /// Records an outcome: newest first, at most [`KEPT_OUTCOMES`] kept, and
    /// the digest counters updated.
    ///
    /// # Arguments
    ///
    /// * `record` - The outcome.
    pub fn record(&mut self, record: OutcomeRecord) {
        match record.kind {
            OutcomeKind::Merged => self.digest.merged += 1,
            OutcomeKind::Rejected => self.digest.rejected += 1,
            OutcomeKind::Waiting => self.digest.waiting += 1,
            OutcomeKind::Reopened => {}
        }
        if let Some(cost) = record.cost_usd {
            self.digest.cost_usd += cost;
        }
        self.outcomes.push_front(record);
        self.outcomes.truncate(KEPT_OUTCOMES);
    }

    /// The text of `factory status`.
    ///
    /// # Arguments
    ///
    /// * `queue` - The open tickets in pick order.
    /// * `paused` - The pause reason, when paused.
    ///
    /// # Returns
    ///
    /// The queue, the ticket in progress and the last outcomes.
    pub fn render(&self, queue: &[Ticket], paused: Option<&str>) -> String {
        let mut out = String::new();
        match paused {
            Some(reason) => out.push_str(&format!("PAUSED: {reason}\n\n")),
            None => out.push_str("running\n\n"),
        }
        out.push_str(&format!("queue ({} open):\n", queue.len()));
        if queue.is_empty() {
            out.push_str("  (empty)\n");
        }
        for ticket in queue {
            out.push_str(&format!(
                "  {:<3} {} {:<6} {}\n",
                priority_label(ticket.priority),
                ticket.id,
                kind_label(ticket.kind),
                ticket.title
            ));
        }
        out.push_str("\nin progress:\n");
        match &self.in_progress {
            Some(current) => out.push_str(&format!(
                "  {} {} [{}] since {}\n",
                current.id,
                current.title,
                current.stage,
                current.since.format("%Y-%m-%d %H:%M UTC")
            )),
            None => out.push_str("  (none)\n"),
        }
        out.push_str(&format!("\nlast {} outcomes:\n", KEPT_OUTCOMES));
        if self.outcomes.is_empty() {
            out.push_str("  (none yet)\n");
        }
        for record in &self.outcomes {
            let cost = record
                .cost_usd
                .map(|cost| format!(" ${cost:.2}"))
                .unwrap_or_default();
            out.push_str(&format!(
                "  {} {:<8} {} {}: {}{cost}\n",
                record.finished_at.format("%Y-%m-%d %H:%M"),
                kind_word(record.kind),
                record.id,
                record.title,
                record.detail
            ));
        }
        out
    }
}

/// `p0`, `p1`, `p2`.
fn priority_label(priority: anvil_types::Priority) -> String {
    serde_json::to_value(priority)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// The kind as it is written in a ticket file.
fn kind_label(kind: anvil_types::TicketKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// The outcome as one word.
fn kind_word(kind: OutcomeKind) -> &'static str {
    match kind {
        OutcomeKind::Merged => "merged",
        OutcomeKind::Rejected => "rejected",
        OutcomeKind::Waiting => "waiting",
        OutcomeKind::Reopened => "reopened",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(n: u32, kind: OutcomeKind, cost: Option<f64>) -> OutcomeRecord {
        OutcomeRecord {
            id: format!("T{n}"),
            title: format!("title {n}"),
            kind,
            detail: "#1".to_owned(),
            finished_at: "2026-10-31T12:00:00Z".parse().expect("rfc3339"),
            cost_usd: cost,
        }
    }

    #[test]
    fn keeps_ten_newest_and_counts_for_the_digest() {
        let mut state = ServiceState::default();
        for n in 0..12 {
            state.record(record(n, OutcomeKind::Merged, Some(1.5)));
        }
        state.record(record(99, OutcomeKind::Rejected, None));
        state.record(record(98, OutcomeKind::Waiting, Some(0.5)));
        assert_eq!(state.outcomes.len(), KEPT_OUTCOMES);
        assert_eq!(state.outcomes[0].id, "T98");
        assert_eq!(state.outcomes[1].id, "T99");
        assert_eq!(state.digest.merged, 12);
        assert_eq!(state.digest.rejected, 1);
        assert_eq!(state.digest.waiting, 1);
        assert!((state.digest.cost_usd - 18.5).abs() < 1e-9);
    }

    #[test]
    fn round_trips_through_the_file_and_defaults_when_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("state.json");
        assert_eq!(
            ServiceState::load(&path).expect("default"),
            ServiceState::default()
        );
        let mut state = ServiceState::default();
        state.record(record(1, OutcomeKind::Merged, Some(2.0)));
        state.refused.insert("tickets/x@abc".to_owned());
        state.consecutive_errors = 1;
        state.save(&path).expect("saves");
        assert_eq!(ServiceState::load(&path).expect("loads"), state);
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[test]
    fn render_shows_queue_progress_and_outcomes() {
        let mut state = ServiceState::default();
        state.record(record(1, OutcomeKind::Merged, Some(2.25)));
        state.in_progress = Some(InProgress {
            id: "T7".to_owned(),
            title: "seven".to_owned(),
            stage: "gate B".to_owned(),
            since: "2026-10-31T11:00:00Z".parse().expect("rfc3339"),
        });
        let text = state.render(&[], Some("main CI is red"));
        assert!(text.starts_with("PAUSED: main CI is red"), "{text}");
        assert!(text.contains("queue (0 open):\n  (empty)"), "{text}");
        assert!(
            text.contains("T7 seven [gate B] since 2026-10-31 11:00 UTC"),
            "{text}"
        );
        assert!(text.contains("merged   T1 title 1: #1 $2.25"), "{text}");
        let text = state.render(&[], None);
        assert!(text.starts_with("running\n"), "{text}");
    }
}
