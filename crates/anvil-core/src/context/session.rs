//! Session persistence: one JSONL log per session under `.anvil/sessions/<id>/`.
//!
//! The log is append-only. A `compacted` record carries the whole
//! conversation as it stands after compaction, so loading is a single pass.
//!
//! The policy's transcript mode decides what is kept. `full` keeps every
//! message as the provider received it. `metadata` keeps roles, tool
//! names, sizes, SHA-256 digests, token counts, timings, states and
//! redaction counts, and replaces every message, the task and each tool
//! argument with its size and digest. `off` writes no log at all (the
//! caller does not open one). Only a `full` log can be resumed.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use anvil_types::policy::TranscriptMode;
use sha2::{Digest, Sha256};

use crate::agent::{Role, State, Status};
use crate::provider::{Message, Usage};

/// Where sessions live, relative to the project root.
pub const SESSIONS_DIR: &str = ".anvil/sessions";

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
    /// The session was recorded without its conversation, so it cannot be
    /// resumed.
    #[error(
        "session `{id}` cannot be resumed: its transcript was kept as `{}`, which holds no conversation",
        mode.as_str()
    )]
    NoTranscript {
        /// The session id.
        id: String,
        /// The mode the session was recorded in.
        mode: TranscriptMode,
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
    /// What the log keeps. Logs written before modes existed are `full`.
    #[serde(default)]
    pub transcript: TranscriptMode,
}

/// A message as a `metadata` transcript keeps it: sizes and digests, no text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageDigest {
    /// `user`, `assistant` or `tool_results`.
    pub role: String,
    /// Byte length of the message text (for tool results, of all results).
    pub bytes: usize,
    /// [`sha256_hex`] of that text.
    pub sha256: String,
    /// The tool calls of an assistant turn.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<CallDigest>,
    /// The results of a tool results message.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub results: Vec<ResultDigest>,
}

/// A tool call without its arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallDigest {
    /// The call id.
    pub id: String,
    /// The tool name.
    pub name: String,
    /// Byte length of the arguments.
    pub bytes: usize,
    /// [`sha256_hex`] of the arguments.
    pub sha256: String,
}

/// A tool result without its content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultDigest {
    /// The call id it answers.
    pub call_id: String,
    /// Byte length of the content.
    pub bytes: usize,
    /// [`sha256_hex`] of the content.
    pub sha256: String,
    /// Whether the tool reported an error.
    pub is_error: bool,
}

impl MessageDigest {
    /// Describes a message without its text.
    ///
    /// # Arguments
    ///
    /// * `message` - The message.
    ///
    /// # Returns
    ///
    /// The digest.
    pub fn of(message: &Message) -> Self {
        match message {
            Message::User { text } => Self {
                role: "user".to_owned(),
                bytes: text.len(),
                sha256: sha256_hex(text.as_bytes()),
                tool_calls: Vec::new(),
                results: Vec::new(),
            },
            Message::Assistant {
                text, tool_calls, ..
            } => Self {
                role: "assistant".to_owned(),
                bytes: text.len(),
                sha256: sha256_hex(text.as_bytes()),
                tool_calls: tool_calls
                    .iter()
                    .map(|call| CallDigest {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        bytes: call.arguments_json.len(),
                        sha256: sha256_hex(call.arguments_json.as_bytes()),
                    })
                    .collect(),
                results: Vec::new(),
            },
            Message::ToolResults { results } => {
                let all: String = results.iter().map(|r| r.content.as_str()).collect();
                Self {
                    role: "tool_results".to_owned(),
                    bytes: all.len(),
                    sha256: sha256_hex(all.as_bytes()),
                    tool_calls: Vec::new(),
                    results: results
                        .iter()
                        .map(|result| ResultDigest {
                            call_id: result.call_id.clone(),
                            bytes: result.content.len(),
                            sha256: sha256_hex(result.content.as_bytes()),
                            is_error: result.is_error,
                        })
                        .collect(),
                }
            }
        }
    }
}

/// The SHA-256 of some bytes, lowercase hex.
///
/// # Arguments
///
/// * `bytes` - What to digest.
///
/// # Returns
///
/// 64 hex digits.
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
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
        /// Where the summary was written, if anywhere.
        summary_path: Option<PathBuf>,
    },
    /// A new conversation replaced the old one: the coder's build
    /// conversation, started from the task and the approved plan.
    Rebased {
        /// The whole conversation now.
        messages: Vec<Message>,
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
    /// One completion: who served it, what it cost and how long it took.
    Turn {
        /// The state it ran in.
        state: State,
        /// Whose provider served it.
        role: Role,
        /// Tokens used, when reported.
        usage: Option<Usage>,
        /// Milliseconds from request to the end of the answer.
        latency_ms: u64,
        /// Retries before the request got through.
        retries: u32,
    },
    /// Secrets were redacted from something before it entered the
    /// conversation. Kinds and counts only, never a value.
    Redacted {
        /// What the text came from: a tool name, `user` or `review`.
        source: String,
        /// How many of each kind.
        counts: BTreeMap<String, usize>,
    },
    /// A message, as a `metadata` transcript keeps it.
    MessageDigest {
        /// Its sizes and digests.
        message: MessageDigest,
    },
    /// The conversation after a compaction or a rebase, as a `metadata`
    /// transcript keeps it.
    ConversationDigest {
        /// Each message's sizes and digests.
        messages: Vec<MessageDigest>,
    },
}

impl SessionRecord {
    /// The record as a transcript in `mode` keeps it.
    ///
    /// # Arguments
    ///
    /// * `mode` - The transcript mode.
    ///
    /// # Returns
    ///
    /// The record unchanged for `full` (and `off`, which writes nothing);
    /// for `metadata`, messages and the task replaced by sizes and digests.
    pub fn kept_as(&self, mode: TranscriptMode) -> Self {
        if mode != TranscriptMode::Metadata {
            return self.clone();
        }
        match self {
            Self::Meta(meta) => Self::Meta(SessionMeta {
                task: format!(
                    "(not kept: {} bytes, sha256:{})",
                    meta.task.len(),
                    sha256_hex(meta.task.as_bytes())
                ),
                transcript: mode,
                ..meta.clone()
            }),
            Self::Message { message } => Self::MessageDigest {
                message: MessageDigest::of(message),
            },
            Self::Compacted { messages, .. } | Self::Rebased { messages } => {
                Self::ConversationDigest {
                    messages: messages.iter().map(MessageDigest::of).collect(),
                }
            }
            other => other.clone(),
        }
    }
}

/// Appends records to a session log.
#[derive(Debug)]
pub struct SessionLog {
    path: PathBuf,
    file: File,
    /// What each record keeps.
    mode: TranscriptMode,
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
        Self::open_in_mode(session_dir, TranscriptMode::Full)
    }

    /// Opens the log for a transcript mode other than `off`.
    ///
    /// # Arguments
    ///
    /// * `session_dir` - The session folder.
    /// * `mode` - `full` or `metadata`.
    ///
    /// # Returns
    ///
    /// The log, positioned at the end.
    ///
    /// # Errors
    ///
    /// [`SessionError::Io`] when the folder or file cannot be created.
    pub fn open_in_mode(session_dir: &Path, mode: TranscriptMode) -> Result<Self, SessionError> {
        debug_assert!(
            mode != TranscriptMode::Off,
            "no session log is opened when the transcript is off"
        );
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
        Ok(Self { path, file, mode })
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
        let kept = record.kept_as(self.mode);
        let mut line = serde_json::to_string(&kept).map_err(|error| SessionError::Io {
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

/// One row of `anvil sessions`.
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

/// The folder a project's sessions go in by default.
///
/// # Arguments
///
/// * `project_root` - The project root.
///
/// # Returns
///
/// `<project_root>/.anvil/sessions`. A managed policy can move it.
pub fn project_sessions_root(project_root: &Path) -> PathBuf {
    project_root.join(SESSIONS_DIR)
}

/// The folder of a session.
///
/// # Arguments
///
/// * `sessions_root` - The folder that holds the project's sessions.
/// * `id` - The session id.
///
/// # Returns
///
/// `<sessions_root>/<id>`.
pub fn session_dir(sessions_root: &Path, id: &str) -> PathBuf {
    sessions_root.join(id)
}

/// Reads a session log back.
///
/// # Arguments
///
/// * `sessions_root` - The folder that holds the project's sessions.
/// * `id` - The session id.
///
/// # Returns
///
/// The snapshot.
///
/// # Errors
///
/// [`SessionError::NotFound`] when there is no such session,
/// [`SessionError::NoTranscript`] when it was recorded in `metadata` mode,
/// or a read or parse error.
pub fn load_session(sessions_root: &Path, id: &str) -> Result<SessionSnapshot, SessionError> {
    let snapshot = read_session(sessions_root, id)?;
    if snapshot.meta.transcript != TranscriptMode::Full {
        return Err(SessionError::NoTranscript {
            id: id.to_owned(),
            mode: snapshot.meta.transcript,
        });
    }
    Ok(snapshot)
}

/// Reads a session log back in any transcript mode, for listings.
fn read_session(sessions_root: &Path, id: &str) -> Result<SessionSnapshot, SessionError> {
    let dir = session_dir(sessions_root, id);
    let path = dir.join(LOG_FILE);
    if !path.is_file() {
        return Err(SessionError::NotFound {
            id: id.to_owned(),
            dir: sessions_root.to_path_buf(),
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
            SessionRecord::Compacted { messages, .. } | SessionRecord::Rebased { messages } => {
                snapshot.messages = messages;
            }
            SessionRecord::State { state, .. } => snapshot.last_state = Some(state),
            SessionRecord::Finished { status } => snapshot.status = Some(status),
            SessionRecord::Turn { .. }
            | SessionRecord::Redacted { .. }
            | SessionRecord::MessageDigest { .. }
            | SessionRecord::ConversationDigest { .. } => {}
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
/// * `sessions_root` - The folder that holds the project's sessions.
///
/// # Returns
///
/// One summary per session.
///
/// # Errors
///
/// [`SessionError::Io`] when the sessions folder exists but cannot be read.
pub fn list_sessions(sessions_root: &Path) -> Result<Vec<SessionSummary>, SessionError> {
    let root = sessions_root.to_path_buf();
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
        match read_session(sessions_root, &id) {
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
            transcript: TranscriptMode::Full,
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
            summary_path: Some(dir.join("compaction-1.md")),
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
                reason: IncompleteReason::SessionBudget { used: 5, limit: 4 },
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
                reason: IncompleteReason::SessionBudget { used: 5, limit: 4 }
            })
        );

        // Reopening appends rather than truncating.
        let reopened = SessionLog::open(&dir).expect("reopen");
        assert!(!reopened.is_empty());
    }

    #[test]
    fn a_metadata_transcript_keeps_sizes_and_digests_but_no_text() {
        let root = tempfile::tempdir().expect("tempdir");
        let id = new_session_id();
        let dir = session_dir(root.path(), &id);
        let mut log = SessionLog::open_in_mode(&dir, TranscriptMode::Metadata).expect("open");
        let secret_task = "fix the bug in payroll.rs".to_owned();
        log.append(&SessionRecord::Meta(SessionMeta {
            task: secret_task.clone(),
            ..meta(&id)
        }))
        .expect("meta");
        let call = crate::provider::ToolCall {
            id: "c1".to_owned(),
            name: "read_file".to_owned(),
            arguments_json: "{\"path\":\"payroll.rs\"}".to_owned(),
        };
        let messages = [
            Message::user("the user's words"),
            Message::Assistant {
                text: "the model's words".to_owned(),
                reasoning: Vec::new(),
                tool_calls: vec![call],
            },
            Message::ToolResults {
                results: vec![crate::provider::ToolResult {
                    call_id: "c1".to_owned(),
                    content: "file contents here".to_owned(),
                    is_error: false,
                }],
            },
        ];
        for message in &messages {
            log.append(&SessionRecord::Message {
                message: message.clone(),
            })
            .expect("message");
        }
        log.append(&SessionRecord::Rebased {
            messages: messages.to_vec(),
        })
        .expect("rebased");
        log.append(&SessionRecord::Turn {
            state: State::Build,
            role: Role::Coder,
            usage: Some(Usage::default()),
            latency_ms: 12,
            retries: 0,
        })
        .expect("turn");

        let text = std::fs::read_to_string(dir.join(LOG_FILE)).expect("reads");
        for hidden in [
            secret_task.as_str(),
            "the user's words",
            "the model's words",
            "payroll.rs",
            "file contents here",
        ] {
            assert!(!text.contains(hidden), "{hidden} leaked into {text}");
        }
        let digest = sha256_hex(b"file contents here");
        for kept in [
            "\"role\":\"tool_results\"",
            "\"name\":\"read_file\"",
            "\"bytes\":18",
            digest.as_str(),
            "\"latency_ms\":12",
            "\"transcript\":\"metadata\"",
        ] {
            assert!(text.contains(kept), "{kept} missing from {text}");
        }
        assert!(matches!(
            load_session(root.path(), &id),
            Err(SessionError::NoTranscript {
                mode: TranscriptMode::Metadata,
                ..
            })
        ));
        let listed = list_sessions(root.path()).expect("lists");
        assert_eq!(listed.len(), 1, "a metadata session is still listed");
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
