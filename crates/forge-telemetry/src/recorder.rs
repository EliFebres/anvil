//! Appending records to the day files and reading them back.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, NaiveDate, Utc};

use crate::error::TelemetryError;
use crate::paths::{SIGNALS_FILE, day_file, home_telemetry_dir, project_telemetry_dir};
use crate::record::{Record, RecordKind, digest, hex};

/// Writes one session's records to the project's telemetry folder and, when
/// a home is known, the same lines to the cross-project copy.
///
/// Recording never fails the session: a file that cannot be written is
/// reported once through `tracing` and skipped.
#[derive(Debug, Clone)]
pub struct Recorder {
    /// The session id.
    session: String,
    /// Digest of the project path.
    project: String,
    /// The folders written to.
    dirs: Vec<PathBuf>,
}

impl Recorder {
    /// A recorder for one session.
    ///
    /// # Arguments
    ///
    /// * `project_root` - The project directory; its path is digested, not
    ///   stored.
    /// * `home` - The user's home directory, for the cross-project copy.
    /// * `session` - The session id.
    ///
    /// # Returns
    ///
    /// The recorder. Folders are created on the first write.
    pub fn open(project_root: &Path, home: Option<&Path>, session: &str) -> Self {
        let canonical = project_root
            .canonicalize()
            .unwrap_or_else(|_| project_root.to_path_buf());
        let mut dirs = vec![project_telemetry_dir(project_root)];
        dirs.extend(home.map(home_telemetry_dir));
        Self {
            session: session.to_owned(),
            project: hex(digest(canonical.to_string_lossy().as_bytes())),
            dirs,
        }
    }

    /// The session id.
    ///
    /// # Returns
    ///
    /// The id.
    pub fn session(&self) -> &str {
        &self.session
    }

    /// The project-local telemetry folder.
    ///
    /// # Returns
    ///
    /// The folder.
    pub fn project_dir(&self) -> &Path {
        &self.dirs[0]
    }

    /// The cross-project telemetry folder, when a home is known.
    ///
    /// # Returns
    ///
    /// The folder.
    pub fn home_dir(&self) -> Option<&Path> {
        self.dirs.get(1).map(PathBuf::as_path)
    }

    /// Appends one record to every folder.
    ///
    /// # Arguments
    ///
    /// * `kind` - What happened.
    pub fn record(&self, kind: RecordKind) {
        let record = Record::now(&self.session, &self.project, kind);
        let Ok(mut line) = serde_json::to_string(&record) else {
            tracing::warn!(
                event = record.kind.tag(),
                "cannot serialize a telemetry record"
            );
            return;
        };
        line.push('\n');
        for dir in &self.dirs {
            let path = day_file(dir, record.ts.date_naive());
            if let Err(error) = append_line(&path, &line) {
                tracing::warn!(%error, "telemetry record dropped");
            }
        }
    }
}

/// Appends one line, creating the folder and file as needed.
fn append_line(path: &Path, line: &str) -> Result<(), TelemetryError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| TelemetryError::io(parent, &e))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| TelemetryError::io(path, &e))?;
    file.write_all(line.as_bytes())
        .map_err(|e| TelemetryError::io(path, &e))
}

/// Reads every record in a telemetry folder, oldest first.
///
/// Day files whose date is before `since` are skipped without being opened;
/// records inside a kept file are still filtered by their own timestamp.
/// Lines that do not parse are reported through `tracing` and skipped, so
/// one bad line never hides a folder.
///
/// # Arguments
///
/// * `dir` - The telemetry folder.
/// * `since` - Ignore records before this time.
///
/// # Returns
///
/// The records, sorted by timestamp. A missing folder yields none.
///
/// # Errors
///
/// When the folder exists but cannot be listed, or a file cannot be read.
pub fn load_records(
    dir: &Path,
    since: Option<DateTime<Utc>>,
) -> Result<Vec<Record>, TelemetryError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(TelemetryError::io(dir, &error)),
    };
    let mut records = Vec::new();
    for entry in entries {
        let path = entry.map_err(|e| TelemetryError::io(dir, &e))?.path();
        let Some(date) = day_file_date(&path) else {
            continue;
        };
        if since.is_some_and(|since| date < since.date_naive()) {
            continue;
        }
        let text = std::fs::read_to_string(&path).map_err(|e| TelemetryError::io(&path, &e))?;
        for (index, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Record>(line) {
                Ok(record) if since.is_none_or(|since| record.ts >= since) => {
                    records.push(record);
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(path = %path.display(), line = index + 1, %error, "skipping a bad telemetry line");
                }
            }
        }
    }
    records.sort_by_key(|record| record.ts);
    Ok(records)
}

/// The date of a day file, or `None` for anything else (such as the
/// signals file).
fn day_file_date(path: &Path) -> Option<NaiveDate> {
    let name = path.file_name()?.to_str()?;
    if name == SIGNALS_FILE {
        return None;
    }
    let stem = name.strip_suffix(".jsonl")?;
    NaiveDate::parse_from_str(stem, "%Y-%m-%d").ok()
}

/// The records of one session, in order.
///
/// # Arguments
///
/// * `records` - Records from [`load_records`].
/// * `session` - The session id.
///
/// # Returns
///
/// The matching records.
pub fn session_records(records: &[Record], session: &str) -> Vec<Record> {
    records
        .iter()
        .filter(|record| record.session == session)
        .cloned()
        .collect()
}

/// The records of the `count` most recent sessions.
///
/// A session's age is its latest record, so a resumed session that spans
/// several days counts once.
///
/// # Arguments
///
/// * `records` - Records from [`load_records`].
/// * `count` - How many sessions to keep.
///
/// # Returns
///
/// The records of those sessions, in their original order.
pub fn last_sessions(records: &[Record], count: usize) -> Vec<Record> {
    let mut latest: BTreeMap<&str, DateTime<Utc>> = BTreeMap::new();
    for record in records {
        let entry = latest.entry(&record.session).or_insert(record.ts);
        *entry = (*entry).max(record.ts);
    }
    let mut sessions: Vec<(&str, DateTime<Utc>)> = latest.into_iter().collect();
    sessions.sort_by_key(|(_, ts)| std::cmp::Reverse(*ts));
    let keep: Vec<&str> = sessions.into_iter().take(count).map(|(id, _)| id).collect();
    records
        .iter()
        .filter(|record| keep.contains(&record.session.as_str()))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use chrono::Duration;

    use super::*;
    use crate::record::{SessionMode, SessionOutcome};

    #[test]
    fn records_go_to_both_folders_and_load_back() {
        let project = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let recorder = Recorder::open(project.path(), Some(home.path()), "s1");
        recorder.record(RecordKind::SessionStarted {
            mode: SessionMode::Run,
        });
        recorder.record(RecordKind::SessionEnded {
            outcome: SessionOutcome::Done,
        });
        let local = load_records(recorder.project_dir(), None).expect("load");
        let rolled = load_records(recorder.home_dir().expect("home"), None).expect("load");
        assert_eq!(local.len(), 2);
        assert_eq!(local, rolled);
        assert_eq!(local[0].session, "s1");
        assert_eq!(local[0].project.len(), 16);
        assert!(!local[0].project.contains("tmp"));
        assert!(matches!(local[1].kind, RecordKind::SessionEnded { .. }));
    }

    #[test]
    fn missing_folder_loads_nothing_and_bad_lines_are_skipped() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(
            load_records(&dir.path().join("none"), None)
                .expect("load")
                .is_empty()
        );
        let folder = dir.path().join("t");
        std::fs::create_dir_all(&folder).expect("mkdir");
        let good = serde_json::to_string(&Record::now(
            "s",
            "p",
            RecordKind::SessionStarted {
                mode: SessionMode::Chat,
            },
        ))
        .expect("json");
        std::fs::write(
            folder.join("2026-10-30.jsonl"),
            format!("not json\n{good}\n\n"),
        )
        .expect("write");
        std::fs::write(folder.join("signals.jsonl"), "{}\n").expect("write");
        std::fs::write(folder.join("notes.txt"), "x").expect("write");
        assert_eq!(load_records(&folder, None).expect("load").len(), 1);
    }

    #[test]
    fn since_skips_old_files_and_old_records() {
        let dir = tempfile::tempdir().expect("tempdir");
        let folder = dir.path();
        let now = Utc::now();
        let old = Record {
            ts: now - Duration::days(10),
            session: "old".to_owned(),
            project: "p".to_owned(),
            kind: RecordKind::SessionStarted {
                mode: SessionMode::Run,
            },
        };
        let fresh = Record {
            ts: now,
            session: "new".to_owned(),
            project: "p".to_owned(),
            kind: RecordKind::SessionStarted {
                mode: SessionMode::Run,
            },
        };
        let line = |record: &Record| format!("{}\n", serde_json::to_string(record).expect("json"));
        std::fs::write(day_file(folder, old.ts.date_naive()), line(&old)).expect("write");
        std::fs::write(
            day_file(folder, now.date_naive()),
            format!("{}{}", line(&old), line(&fresh)),
        )
        .expect("write");
        let all = load_records(folder, None).expect("load");
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].session, "old");
        let recent = load_records(folder, Some(now - Duration::days(7))).expect("load");
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].session, "new");
    }

    #[test]
    fn last_sessions_keeps_the_newest_distinct_ids() {
        let now = Utc::now();
        let make = |session: &str, age_minutes: i64| Record {
            ts: now - Duration::minutes(age_minutes),
            session: session.to_owned(),
            project: "p".to_owned(),
            kind: RecordKind::SessionStarted {
                mode: SessionMode::Run,
            },
        };
        let records = vec![
            make("a", 50),
            make("b", 40),
            make("a", 5),
            make("c", 30),
            make("d", 20),
        ];
        let kept = last_sessions(&records, 2);
        let ids: Vec<&str> = kept.iter().map(|r| r.session.as_str()).collect();
        assert_eq!(ids, vec!["a", "a", "d"]);
        assert_eq!(session_records(&records, "a").len(), 2);
    }
}
