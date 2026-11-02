//! Every outcome ever recorded, in `~/.factory/outcomes.jsonl`, and the
//! day and week summaries the dashboard shows from it.
//!
//! `state.json` keeps only the last ten outcomes; the week view needs more,
//! so each outcome is also appended here, one JSON object per line.

use std::io::Write;
use std::path::Path;

use chrono::{DateTime, Duration, FixedOffset, NaiveDate, Utc};

use crate::board::{DayBar, DayCounts, WeekView};
use crate::error::{FactoryError, Result};
use crate::status::{OutcomeKind, OutcomeRecord};

/// Days in the week view, today included.
pub const WEEK_DAYS: i64 = 7;

/// Appends one outcome.
///
/// # Arguments
///
/// * `path` - `~/.factory/outcomes.jsonl`.
/// * `record` - The outcome.
///
/// # Errors
///
/// When the file cannot be opened or written.
pub fn append(path: &Path, record: &OutcomeRecord) -> Result<()> {
    let line = serde_json::to_string(record).map_err(|source| FactoryError::Json {
        path: path.to_path_buf(),
        source,
    })?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|source| FactoryError::io("creating", dir, source))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .map_err(|source| FactoryError::io("opening", path, source))?;
    writeln!(file, "{line}").map_err(|source| FactoryError::io("writing", path, source))
}

/// Starts the history from the outcomes `state.json` already holds, once.
///
/// # Arguments
///
/// * `path` - `~/.factory/outcomes.jsonl`.
/// * `newest_first` - The outcomes in `state.json`, newest first.
///
/// # Errors
///
/// When the file cannot be written.
pub fn seed(path: &Path, newest_first: &[OutcomeRecord]) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    let mut text = String::new();
    for record in newest_first.iter().rev() {
        let line = serde_json::to_string(record).map_err(|source| FactoryError::Json {
            path: path.to_path_buf(),
            source,
        })?;
        text.push_str(&line);
        text.push('\n');
    }
    crate::board::write_atomic(path, text.as_bytes())
}

/// Reads the history, skipping lines that do not parse.
///
/// # Arguments
///
/// * `path` - `~/.factory/outcomes.jsonl`.
///
/// # Returns
///
/// The outcomes, oldest first; empty when there is no file.
pub fn read(path: &Path) -> Vec<OutcomeRecord> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// Today's counts in the local day.
///
/// # Arguments
///
/// * `outcomes` - The history.
/// * `now` - The current time.
/// * `tz` - The local time zone.
///
/// # Returns
///
/// Merged and rejected counts and the cost of every outcome today.
pub fn today(outcomes: &[OutcomeRecord], now: DateTime<Utc>, tz: FixedOffset) -> DayCounts {
    let day = local_day(now, tz);
    counts(
        outcomes
            .iter()
            .filter(|record| local_day(record.finished_at, tz) == day),
    )
}

/// The last [`WEEK_DAYS`] local days.
///
/// # Arguments
///
/// * `outcomes` - The history.
/// * `now` - The current time.
/// * `tz` - The local time zone.
/// * `waiting` - Tickets waiting on a human now.
///
/// # Returns
///
/// One bar per day, oldest first, the totals and the median time from
/// claim to outcome of the merged and rejected tickets.
pub fn week(
    outcomes: &[OutcomeRecord],
    now: DateTime<Utc>,
    tz: FixedOffset,
    waiting: u32,
) -> WeekView {
    let today = local_day(now, tz);
    let first = today - Duration::days(WEEK_DAYS - 1);
    let in_week: Vec<&OutcomeRecord> = outcomes
        .iter()
        .filter(|record| local_day(record.finished_at, tz) >= first)
        .collect();
    let days = (0..WEEK_DAYS)
        .map(|offset| {
            let day = first + Duration::days(offset);
            let of_day = counts(
                in_week
                    .iter()
                    .copied()
                    .filter(|record| local_day(record.finished_at, tz) == day),
            );
            DayBar {
                day,
                merged: of_day.merged,
                rejected: of_day.rejected,
            }
        })
        .collect();
    let mut secs: Vec<u64> = in_week
        .iter()
        .filter(|record| matches!(record.kind, OutcomeKind::Merged | OutcomeKind::Rejected))
        .filter_map(|record| {
            let started = record.started_at?;
            u64::try_from((record.finished_at - started).num_seconds()).ok()
        })
        .collect();
    secs.sort_unstable();
    WeekView {
        days,
        totals: counts(in_week.iter().copied()),
        waiting,
        median_secs: secs.get(secs.len() / 2).copied(),
    }
}

/// Merged and rejected counts and the known cost of some outcomes.
fn counts<'a>(records: impl Iterator<Item = &'a OutcomeRecord>) -> DayCounts {
    let mut counts = DayCounts::default();
    for record in records {
        match record.kind {
            OutcomeKind::Merged => counts.merged += 1,
            OutcomeKind::Rejected => counts.rejected += 1,
            OutcomeKind::Waiting | OutcomeKind::Reopened => {}
        }
        counts.cost_usd += record.cost_usd.unwrap_or(0.0);
    }
    counts
}

/// The local calendar day of a moment.
fn local_day(at: DateTime<Utc>, tz: FixedOffset) -> NaiveDate {
    at.with_timezone(&tz).date_naive()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(kind: OutcomeKind, finished: &str, minutes: i64, cost: f64) -> OutcomeRecord {
        let finished_at: DateTime<Utc> = finished.parse().expect("rfc3339");
        OutcomeRecord {
            id: "T".to_owned(),
            title: "t".to_owned(),
            kind,
            detail: String::new(),
            finished_at,
            cost_usd: Some(cost),
            pr: None,
            started_at: Some(finished_at - Duration::minutes(minutes)),
        }
    }

    #[test]
    fn appends_seeds_once_and_reads_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("outcomes.jsonl");
        let older = record(OutcomeKind::Merged, "2026-11-01T10:00:00Z", 30, 1.0);
        let newer = record(OutcomeKind::Rejected, "2026-11-02T10:00:00Z", 40, 2.0);
        seed(&path, &[newer.clone(), older.clone()]).expect("seed");
        seed(&path, &[]).expect("second seed is a no-op");
        append(&path, &older).expect("append");
        std::fs::write(
            &path,
            std::fs::read_to_string(&path).expect("read") + "garbage\n",
        )
        .expect("write");
        assert_eq!(read(&path), vec![older.clone(), newer, older]);
        assert!(read(&dir.path().join("missing")).is_empty());
    }

    #[test]
    fn today_and_the_week_count_in_the_local_day() {
        let tz = FixedOffset::west_opt(5 * 3600).expect("tz");
        let now: DateTime<Utc> = "2026-11-02T20:00:00Z".parse().expect("rfc3339");
        let outcomes = vec![
            record(OutcomeKind::Merged, "2026-10-20T12:00:00Z", 10, 9.0),
            record(OutcomeKind::Merged, "2026-10-28T12:00:00Z", 20, 1.0),
            record(OutcomeKind::Rejected, "2026-11-02T03:00:00Z", 60, 2.0),
            record(OutcomeKind::Merged, "2026-11-02T15:00:00Z", 30, 3.0),
            record(OutcomeKind::Reopened, "2026-11-02T16:00:00Z", 5, 0.5),
        ];
        let today = today(&outcomes, now, tz);
        assert_eq!((today.merged, today.rejected), (1, 0));
        assert!((today.cost_usd - 3.5).abs() < 1e-9);

        let week = week(&outcomes, now, tz, 2);
        assert_eq!(week.days.len(), 7);
        assert_eq!(week.days[0].day.to_string(), "2026-10-27");
        assert_eq!(week.days[6].day.to_string(), "2026-11-02");
        assert_eq!(week.days[1].merged, 1);
        assert_eq!(
            week.days[5].rejected, 1,
            "03:00Z is the evening before locally"
        );
        assert_eq!(week.days[6].merged, 1);
        assert_eq!((week.totals.merged, week.totals.rejected), (2, 1));
        assert!((week.totals.cost_usd - 6.5).abs() < 1e-9);
        assert_eq!(week.waiting, 2);
        assert_eq!(week.median_secs, Some(30 * 60));
    }
}
