//! Session persistence: one JSONL log per session under `.forge/sessions/<id>/`.
//!
//! The log is append-only. A `compacted` record carries the whole
//! conversation as it stands after compaction, so loading is a single pass.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::agent::{State, Status};
use crate::provider::Message;

/// Where sessions live, relative to the project root.
pub const SESSIONS_DIR: &str = ".forge/sessions";

/// The log file inside a session folder.
pub const LOG_FILE: &str = "session.jsonl";

/// Why a session could not be read or written.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SessionError {
    /// No session folder with that id.
    #[error("no session `{id}` under {dir}")]
    NotFound {
        /// The id asked for.
        id: String,
        /// Where sessions were looked for.
        dir: PathBuf,
    },
    /// The log could not be read or written.
    #[error("cannot access {path}: {detail}")]
    Io {
        /// The file.
        path: PathBuf,
        /// The I/O error text.
        detail: String,
    },
    /// A log line is not a valid record.
    #[error("{path} line {line}: {detail}")]
    Parse {
        /// The file.
        path: PathBuf,
        /// 1-based line number.
        line: usize,
        /// The parse error text.
        detail: String,
    },
    /// The log has no `meta` record first.
    #[error("{path} does not start with a meta record")]
    MissingMeta {
        /// The file.
        path: PathBuf,
    },
}

/// What a session is about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMeta {
    /// The session id, a ULID.
    pub id: String,
    /// When it started.
    pub created_at: DateTime<Utc>,
    /// The task.
    pub task: String,
    /// The project root.
    pub cwd: PathBuf,
}

/// One line of the log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionRecord {
    /// Always the first line.
    Meta(SessionMeta),
    /// The git tree of the working tree at session start.
    Baseline {
        /// The tree hash.
        tree: String,
    },
    /// A message appended to the conversation.
    Message {
        /// The message.
        message: Message,
    },
    /// The conversation after a compaction, replacing everything before.
    Compacted {
        /// The whole conversation now.
        messages: Vec<Message>,
        /// Where the summary was written.
        summary_path: PathBuf,
    },
    /// A state was entered.
    State {
        /// The state.
        state: State,
        /// Which visit.
        visit: u32,
    },
    /// The run ended.
    Finished {
        /// How.
        status: Status,
    },
}

/// Appends records to a session log.
#[derive(Debug)]
pub struct SessionLog {
    path: PathBuf,
    file: File,
}

impl SessionLog {
    /// Opens the log in a session folder, creating the folder and file.
    ///
    /// # Arguments
    ///
    /// * `session_dir` - The session folder.
    ///
    /// # Returns
    ///
    /// The log, positioned at the end.
    ///
    /// # Errors
    ///
    /// [`SessionError::Io`] when the folder or file cannot be created.
    pub fn open(session_dir: &Path) -> Result<Self, SessionError> {
        let path = session_dir.join(LOG_FILE);
        let io = |error: std::io::Error| SessionError::Io {
            path: path.clone(),
            detail: error.to_string(),
        };
        std::fs::create_dir_all(session_dir).map_err(io)?;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(io)?;
        Ok(Self { path, file })
    }

    /// Whether the log has no records yet.
    ///
    /// # Returns
    ///
    /// `true` for a fresh log.
    pub fn is_empty(&self) -> bool {
        self.file.metadata().map(|m| m.len() == 0).unwrap_or(true)
    }

    /// Appends one record.
    ///
    /// # Arguments
    ///
    /// * `record` - The record.
    ///
    /// # Errors
    ///
    /// [`SessionError::Io`] when the write fails.
    pub fn append(&mut self, record: &SessionRecord) -> Result<(), SessionError> {
        let mut line = serde_json::to_string(record).map_err(|error| SessionError::Io {
            path: self.path.clone(),
            detail: error.to_string(),
        })?;
        line.push('\n');
        self.file
            .write_all(line.as_bytes())
            .map_err(|error| SessionError::Io {
                path: self.path.clone(),
                detail: error.to_string(),
            })
    }

    /// The log file's path.
    ///
    /// # Returns
    ///
    /// The path.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// A session as reconstructed from its log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSnapshot {
    /// What it is about.
    pub meta: SessionMeta,
    /// The session folder.
    pub dir: PathBuf,
    /// The conversation as it stood at the end of the log.
    pub messages: Vec<Message>,
    /// The git baseline, if one was recorded.
    pub baseline: Option<String>,
    /// The last state entered.
    pub last_state: Option<State>,
    /// How the run ended, if it did.
    pub status: Option<Status>,
}

/// One row of `forge sessions`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    /// What it is about.
    pub meta: SessionMeta,
    /// The last state entered.
    pub last_state: Option<State>,
    /// How the run ended, if it did.
    pub status: Option<Status>,
}

/// A fresh session id.
///
/// # Returns
///
/// A ULID, which sorts by creation time.
pub fn new_session_id() -> String {
    ulid::Ulid::generate().to_string()
}

/// The folder of a session.
///
/// # Arguments
///
/// * `project_root` - The project root.
/// * `id` - The session id.
///
/// # Returns
///
/// `<project_root>/.forge/sessions/<id>`.
pub fn session_dir(project_root: &Path, id: &str) -> PathBuf {
    project_root.join(SESSIONS_DIR).join(id)
}

/// Reads a session log back.
///
/// # Arguments
///
/// * `project_root` - The project root.
/// * `id` - The session id.
///
/// # Returns
///
/// The snapshot.
///
/// # Errors
///
/// [`SessionError::NotFound`] when there is no such session, or a read or
/// parse error.
pub fn load_session(project_root: &Path, id: &str) -> Result<SessionSnapshot, SessionError> {
    let dir = session_dir(project_root, id);
    let path = dir.join(LOG_FILE);
    if !path.is_file() {
        return Err(SessionError::NotFound {
            id: id.to_owned(),
            dir: project_root.join(SESSIONS_DIR),
        });
    }
    let records = read_records(&path)?;
    let Some(SessionRecord::Meta(meta)) = records.first().cloned() else {
        return Err(SessionError::MissingMeta { path });
    };
    let mut snapshot = SessionSnapshot {
        meta,
        dir,
        messages: Vec::new(),
        baseline: None,
        last_state: None,
        status: None,
    };
    for record in records.into_iter().skip(1) {
        match record {
            SessionRecord::Meta(_) => {}
            SessionRecord::Baseline { tree } => snapshot.baseline = Some(tree),
            SessionRecord::Message { message } => snapshot.messages.push(message),
            SessionRecord::Compacted { messages, .. } => snapshot.messages = messages,
            SessionRecord::State { state, .. } => snapshot.last_state = Some(state),
            SessionRecord::Finished { status } => snapshot.status = Some(status),
        }
    }
    Ok(snapshot)
}

/// Lists every session, newest first.
///
/// Folders without a readable log are skipped.
///
/// # Arguments
///
/// * `project_root` - The project root.
///
/// # Returns
///
/// One summary per session.
///
/// # Errors
///
/// [`SessionError::Io`] when the sessions folder exists but cannot be read.
pub fn list_sessions(project_root: &Path) -> Result<Vec<SessionSummary>, SessionError> {
    let root = project_root.join(SESSIONS_DIR);
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(SessionError::Io {
                path: root,
                detail: error.to_string(),
            });
        }
    };
    let mut sessions = Vec::new();
    for entry in entries.filter_map(Result::ok) {
        let id = entry.file_name().to_string_lossy().into_owned();
        match load_session(project_root, &id) {
            Ok(snapshot) => sessions.push(SessionSummary {
                meta: snapshot.meta,
                last_state: snapshot.last_state,
                status: snapshot.status,
            }),
            Err(error) => tracing::debug!(id, %error, "skipping session"),
        }
    }
    sessions.sort_by(|a, b| b.meta.id.cmp(&a.meta.id));
    Ok(sessions)
}

/// Parses every line of a log.
fn read_records(path: &Path) -> Result<Vec<SessionRecord>, SessionError> {
    let file = File::open(path).map_err(|error| SessionError::Io {
        path: path.to_path_buf(),
        detail: error.to_string(),
    })?;
    let mut records = Vec::new();
    for (index, line) in BufReader::new(file).lines().enumerate() {
        let line = line.map_err(|error| SessionError::Io {
            path: path.to_path_buf(),
            detail: error.to_string(),
        })?;
        if line.trim().is_empty() {
            continue;
        }
        let record = serde_json::from_str(&line).map_err(|error| SessionError::Parse {
            path: path.to_path_buf(),
            line: index + 1,
            detail: error.to_string(),
        })?;
        records.push(record);
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::IncompleteReason;

    fn meta(id: &str) -> SessionMeta {
        SessionMeta {
            id: id.to_owned(),
            created_at: Utc::now(),
            task: "do it".to_owned(),
            cwd: PathBuf::from("/proj"),
        }
    }

    #[test]
    fn write_then_load_round_trips_with_compaction() {
        let root = tempfile::tempdir().expect("tempdir");
        let id = new_session_id();
        assert_eq!(id.len(), 26);
        let dir = session_dir(root.path(), &id);
        let mut log = SessionLog::open(&dir).expect("open");
        assert!(log.is_empty());
        log.append(&SessionRecord::Meta(meta(&id))).expect("meta");
        log.append(&SessionRecord::Baseline {
            tree: "abc".to_owned(),
        })
        .expect("baseline");
        log.append(&SessionRecord::State {
            state: State::Plan,
            visit: 1,
        })
        .expect("state");
        log.append(&SessionRecord::Message {
            message: Message::user("old"),
        })
        .expect("message");
        log.append(&SessionRecord::Compacted {
            messages: vec![Message::user("summary")],
            summary_path: dir.join("compaction-1.md"),
        })
        .expect("compacted");
        log.append(&SessionRecord::Message {
            message: Message::user("new"),
        })
        .expect("message");
        log.append(&SessionRecord::State {
            state: State::Build,
            visit: 1,
        })
        .expect("state");
        log.append(&SessionRecord::Finished {
            status: Status::Incomplete {
                reason: IncompleteReason::SessionBudget,
            },
        })
        .expect("finished");
        assert!(!log.is_empty());

        let snapshot = load_session(root.path(), &id).expect("loads");
        assert_eq!(snapshot.meta.task, "do it");
        assert_eq!(snapshot.dir, dir);
        assert_eq!(snapshot.baseline.as_deref(), Some("abc"));
        assert_eq!(
            snapshot.messages,
            vec![Message::user("summary"), Message::user("new")]
        );
        assert_eq!(snapshot.last_state, Some(State::Build));
        assert_eq!(
            snapshot.status,
            Some(Status::Incomplete {
                reason: IncompleteReason::SessionBudget
            })
        );

        // Reopening appends rather than truncating.
        let reopened = SessionLog::open(&dir).expect("reopen");
        assert!(!reopened.is_empty());
    }

    #[test]
    fn list_is_newest_first_and_skips_broken_folders() {
        let root = tempfile::tempdir().expect("tempdir");
        assert!(list_sessions(root.path()).expect("empty").is_empty());
        for id in ["01A", "01C", "01B"] {
            let mut log = SessionLog::open(&session_dir(root.path(), id)).expect("open");
            log.append(&SessionRecord::Meta(meta(id))).expect("meta");
        }
        std::fs::create_dir_all(session_dir(root.path(), "broken")).expect("mkdir");
        std::fs::write(
            session_dir(root.path(), "nometa").join(LOG_FILE),
            "{\"type\":\"baseline\",\"tree\":\"x\"}\n",
        )
        .unwrap_or_else(|_| {
            std::fs::create_dir_all(session_dir(root.path(), "nometa")).expect("mkdir");
            std::fs::write(
                session_dir(root.path(), "nometa").join(LOG_FILE),
                "{\"type\":\"baseline\",\"tree\":\"x\"}\n",
            )
            .expect("write");
        });
        let ids: Vec<String> = list_sessions(root.path())
            .expect("lists")
            .into_iter()
            .map(|s| s.meta.id)
            .collect();
        assert_eq!(ids, vec!["01C", "01B", "01A"]);
    }

    #[test]
    fn errors_are_specific() {
        let root = tempfile::tempdir().expect("tempdir");
        assert!(matches!(
            load_session(root.path(), "nope"),
            Err(SessionError::NotFound { .. })
        ));
        let dir = session_dir(root.path(), "bad");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join(LOG_FILE), "not json\n").expect("write");
        assert!(matches!(
            load_session(root.path(), "bad"),
            Err(SessionError::Parse { line: 1, .. })
        ));
        std::fs::write(
            dir.join(LOG_FILE),
            "{\"type\":\"baseline\",\"tree\":\"x\"}\n",
        )
        .expect("write");
        assert!(matches!(
            load_session(root.path(), "bad"),
            Err(SessionError::MissingMeta { .. })
        ));
    }
}
