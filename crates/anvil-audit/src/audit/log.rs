//! Appending sealed records to a session's audit file and its mirror.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use chrono::Utc;

use super::chain::{AuditError, GENESIS, Identity, RecordBody, seal, verify};
use super::event::AuditEvent;

/// The file name of a session's audit log inside its folder.
pub const AUDIT_FILE: &str = "audit.jsonl";

/// The file a session's records are mirrored to in the policy's
/// `[audit] mirror_dir`.
///
/// # Arguments
///
/// * `mirror_dir` - The mirror directory.
/// * `session` - The session id.
///
/// # Returns
///
/// `<mirror_dir>/<session>.audit.jsonl`.
pub fn mirror_file(mirror_dir: &Path, session: &str) -> PathBuf {
    mirror_dir.join(format!("{session}.audit.jsonl"))
}

/// Where a session's records go and whether a failed write must stop it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditTarget {
    /// The session's own file, `<session folder>/audit.jsonl`.
    pub primary: PathBuf,
    /// The copy in the policy's mirror directory, if one is set.
    pub mirror: Option<PathBuf>,
    /// Whether the policy requires the log: a failed write then stops
    /// every later write and the caller ends the session.
    pub required: bool,
}

/// One session's audit log. Appends are serialized, so records from the
/// event tee, the permission check and subagents interleave in one chain.
pub struct AuditLog {
    /// Fixed for the session.
    session: String,
    /// Fixed for the session.
    identity: Identity,
    /// Whether a failed write stops the log.
    required: bool,
    /// The files and the position in the chain.
    state: Mutex<State>,
}

/// Where records are written, with the path named in errors.
pub type Sink = (PathBuf, Box<dyn Write + Send>);

/// What changes as records are appended.
struct State {
    /// The open files, primary first.
    files: Vec<Sink>,
    /// The next record's `seq`.
    seq: u64,
    /// The last record's hash.
    prev: String,
    /// The first failed write, which under `required` stops the log.
    failure: Option<String>,
}

impl AuditLog {
    /// Opens or creates the files of a session's log, so a log that
    /// cannot be written fails before the session does anything.
    ///
    /// A file that already holds records, as when a session resumes, is
    /// verified and continued from its last record. The primary file
    /// decides where the chain continues; a mirror that holds a different
    /// last record is reported and appended to anyway.
    ///
    /// # Arguments
    ///
    /// * `session` - The session id.
    /// * `identity` - Who and what produces the records.
    /// * `target` - Where they go.
    ///
    /// # Returns
    ///
    /// The log.
    ///
    /// # Errors
    ///
    /// [`AuditError::Io`] when a folder or file cannot be created or
    /// opened, and, when the log is required, [`AuditError::Broken`] when
    /// the existing primary file does not verify.
    pub fn open(
        session: &str,
        identity: Identity,
        target: &AuditTarget,
    ) -> Result<Self, AuditError> {
        let mut seq = 0;
        let mut prev = GENESIS.to_owned();
        match std::fs::read_to_string(&target.primary) {
            Ok(text) if !text.is_empty() => match verify(&text) {
                Ok(verified) => {
                    seq = u64::try_from(verified.records.len()).unwrap_or(u64::MAX);
                    prev = verified.last_hash().to_owned();
                }
                Err(error) if target.required => return Err(error),
                Err(error) => {
                    tracing::warn!(path = %target.primary.display(), %error, "the existing audit log does not verify; continuing after its last line");
                    if let Some(last) = text.lines().last()
                        && let Ok(record) = super::chain::open_line(last)
                    {
                        seq = record.body.seq.saturating_add(1);
                        prev = record.hash;
                    }
                }
            },
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(AuditError::io(&target.primary, &error)),
        }
        let mut files: Vec<Sink> = Vec::new();
        for path in std::iter::once(&target.primary).chain(target.mirror.as_ref()) {
            files.push((path.clone(), Box::new(open_append(path)?)));
        }
        let log = Self::with_sinks(session, identity, target.required, files);
        {
            let mut state = log.lock();
            state.seq = seq;
            state.prev = prev;
        }
        Ok(log)
    }

    /// A log over writers that are not necessarily files, starting a new
    /// chain. Tests use it to make a write fail on purpose.
    ///
    /// # Arguments
    ///
    /// * `session` - The session id.
    /// * `identity` - Who and what produces the records.
    /// * `required` - Whether a failed write stops the log.
    /// * `sinks` - Where records go, each with the path errors name.
    ///
    /// # Returns
    ///
    /// The log.
    pub fn with_sinks(session: &str, identity: Identity, required: bool, sinks: Vec<Sink>) -> Self {
        Self {
            session: session.to_owned(),
            identity,
            required,
            state: Mutex::new(State {
                files: sinks,
                seq: 0,
                prev: GENESIS.to_owned(),
                failure: None,
            }),
        }
    }

    /// Whether a failed write stops the log.
    ///
    /// # Returns
    ///
    /// The policy's `[audit] required`.
    pub fn required(&self) -> bool {
        self.required
    }

    /// The first failed write, if any.
    ///
    /// # Returns
    ///
    /// Its message.
    pub fn failure(&self) -> Option<String> {
        self.lock().failure.clone()
    }

    /// Appends one record to every file.
    ///
    /// # Arguments
    ///
    /// * `agent` - `main`, or the subagent that acted.
    /// * `event` - What happened.
    ///
    /// # Errors
    ///
    /// The first write error. When the log is required, every later call
    /// fails with [`AuditError::Stopped`] and writes nothing.
    pub fn append(&self, agent: &str, event: AuditEvent) -> Result<(), AuditError> {
        let mut state = self.lock();
        if self.required
            && let Some(detail) = &state.failure
        {
            return Err(AuditError::Stopped {
                detail: detail.clone(),
            });
        }
        let body = RecordBody {
            seq: state.seq,
            ts: Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
            session: self.session.clone(),
            agent: agent.to_owned(),
            user: self.identity.user.clone(),
            host: self.identity.host.clone(),
            version: self.identity.version.clone(),
            commit: self.identity.commit.clone(),
            binary_sha256: self.identity.binary_sha256.clone(),
            policy: self.identity.policy.clone(),
            event,
            prev: state.prev.clone(),
        };
        let (mut line, hash) = seal(&body)?;
        line.push('\n');
        let mut first_error = None;
        for (path, file) in &mut state.files {
            if let Err(error) = file.write_all(line.as_bytes()).and_then(|()| file.flush()) {
                first_error.get_or_insert(AuditError::io(path.as_path(), &error));
            }
        }
        state.seq += 1;
        state.prev = hash;
        match first_error {
            None => Ok(()),
            Some(error) => {
                if state.failure.is_none() {
                    state.failure = Some(error.to_string());
                }
                Err(error)
            }
        }
    }

    /// The state, even after a panic elsewhere poisoned the lock.
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Opens a file for appending, creating it and its folders owner-only.
fn open_append(path: &Path) -> Result<File, AuditError> {
    if let Some(parent) = path.parent() {
        anvil_types::private_fs::create_dir_all(parent)
            .map_err(|error| AuditError::io(parent, &error))?;
    }
    anvil_types::private_fs::open_append(path).map_err(|error| AuditError::io(path, &error))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> Identity {
        Identity {
            user: "u".to_owned(),
            host: "h".to_owned(),
            version: "0.0.0".to_owned(),
            commit: "unknown".to_owned(),
            binary_sha256: "unknown".to_owned(),
            policy: "unmanaged".to_owned(),
        }
    }

    fn end(outcome: &str) -> AuditEvent {
        AuditEvent::SessionEnd {
            outcome: outcome.to_owned(),
        }
    }

    #[test]
    fn records_reach_both_files_and_a_reopened_log_continues_the_chain() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = AuditTarget {
            primary: dir.path().join("s").join(AUDIT_FILE),
            mirror: Some(mirror_file(&dir.path().join("mirror"), "s1")),
            required: true,
        };
        let log = AuditLog::open("s1", identity(), &target).expect("open");
        log.append("main", end("a")).expect("append");
        log.append("explore", end("b")).expect("append");
        drop(log);
        let log = AuditLog::open("s1", identity(), &target).expect("reopen");
        log.append("main", end("c")).expect("append");
        let primary = std::fs::read_to_string(&target.primary).expect("read");
        let mirror =
            std::fs::read_to_string(target.mirror.as_ref().expect("mirror")).expect("read");
        assert_eq!(primary, mirror);
        let verified = verify(&primary).expect("intact");
        assert_eq!(verified.records.len(), 3);
        assert_eq!(verified.records[1].body.agent, "explore");
        assert_eq!(verified.records[2].body.seq, 2);
    }

    #[test]
    fn a_required_log_refuses_to_continue_a_broken_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = AuditTarget {
            primary: dir.path().join(AUDIT_FILE),
            mirror: None,
            required: true,
        };
        std::fs::write(&target.primary, "not a record\n").expect("write");
        assert!(matches!(
            AuditLog::open("s1", identity(), &target),
            Err(AuditError::Broken { line: 1, .. })
        ));
        let lenient = AuditTarget {
            required: false,
            ..target
        };
        AuditLog::open("s1", identity(), &lenient).expect("not required: continues");
    }

    /// A writer that fails every write.
    struct Broken;

    impl Write for Broken {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("disk full"))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_required_log_stops_after_the_first_failed_write() {
        let required = AuditLog::with_sinks(
            "s1",
            identity(),
            true,
            vec![(PathBuf::from("mirror"), Box::new(Broken))],
        );
        assert!(matches!(
            required.append("main", end("a")),
            Err(AuditError::Io { .. })
        ));
        assert!(required.failure().is_some_and(|f| f.contains("disk full")));
        assert!(matches!(
            required.append("main", end("b")),
            Err(AuditError::Stopped { .. })
        ));
        let lenient = AuditLog::with_sinks(
            "s1",
            identity(),
            false,
            vec![(PathBuf::from("mirror"), Box::new(Broken))],
        );
        assert!(lenient.append("main", end("a")).is_err());
        assert!(matches!(
            lenient.append("main", end("b")),
            Err(AuditError::Io { .. })
        ));
    }

    #[test]
    fn a_folder_that_cannot_be_created_fails_the_open() {
        let dir = tempfile::tempdir().expect("tempdir");
        let blocker = dir.path().join("file");
        std::fs::write(&blocker, "x").expect("write");
        let target = AuditTarget {
            primary: dir.path().join(AUDIT_FILE),
            mirror: Some(blocker.join("mirror").join("s1.audit.jsonl")),
            required: true,
        };
        assert!(matches!(
            AuditLog::open("s1", identity(), &target),
            Err(AuditError::Io { .. })
        ));
    }
}
