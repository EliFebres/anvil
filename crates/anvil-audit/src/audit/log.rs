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

/// One place records go, and what its file must still look like.
struct Output {
    /// The path named in errors and checked before each write.
    path: PathBuf,
    /// The writer.
    writer: Box<dyn Write + Send>,
    /// The file as this log left it; `None` for a writer that is not a file.
    expected: Option<Expected>,
}

/// What a log file must look like before the next record is appended.
struct Expected {
    /// Its length: every byte this log wrote and nothing else.
    len: u64,
    /// The file's device and inode, so a file swapped in under the same
    /// name is noticed.
    #[cfg(unix)]
    id: (u64, u64),
}

impl Expected {
    /// Records what an open file looks like now.
    fn of(file: &File, path: &Path) -> Result<Self, AuditError> {
        let meta = file
            .metadata()
            .map_err(|error| AuditError::io(path, &error))?;
        Ok(Self {
            len: meta.len(),
            #[cfg(unix)]
            id: {
                use std::os::unix::fs::MetadataExt as _;
                (meta.dev(), meta.ino())
            },
        })
    }

    /// Why the file at `path` is no longer the one this log writes, if it
    /// is not.
    fn changed(&self, path: &Path) -> Option<String> {
        let meta = match std::fs::metadata(path) {
            Ok(meta) => meta,
            Err(error) => return Some(format!("the file is gone ({error})")),
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            if (meta.dev(), meta.ino()) != self.id {
                return Some("another file was put in its place".to_owned());
            }
        }
        (meta.len() != self.len).then(|| {
            format!(
                "it is {} bytes where this log wrote {}: something else changed it",
                meta.len(),
                self.len
            )
        })
    }
}

/// What changes as records are appended.
struct State {
    /// Where records go, primary first.
    outputs: Vec<Output>,
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
    /// verified and continued from its last record. The mirror must then
    /// hold exactly the same bytes; a mirror that is missing or empty gets
    /// a copy of the session's file first, so it always verifies on its own.
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
    /// [`AuditError::Io`] when a folder or file cannot be created, read or
    /// opened, and, when the log is required, [`AuditError::Broken`] when
    /// the existing primary file does not verify and
    /// [`AuditError::MirrorDiffers`] when the mirror holds other records
    /// than the primary file.
    pub fn open(
        session: &str,
        identity: Identity,
        target: &AuditTarget,
    ) -> Result<Self, AuditError> {
        let mut seq = 0;
        let mut prev = GENESIS.to_owned();
        let text = read_existing(&target.primary)?;
        if !text.is_empty() {
            match verify(&text) {
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
            }
        }
        let mut outputs = vec![open_output(&target.primary, "")?];
        if let Some(mirror) = &target.mirror {
            let held = read_existing(mirror)?;
            let seed = if held.is_empty() {
                text.as_str()
            } else if held == text {
                ""
            } else if target.required {
                return Err(AuditError::MirrorDiffers {
                    primary: target.primary.clone(),
                    mirror: mirror.clone(),
                });
            } else {
                tracing::warn!(mirror = %mirror.display(), "the audit mirror does not hold the same records as the session file; appending anyway");
                ""
            };
            outputs.push(open_output(mirror, seed)?);
        }
        Ok(Self {
            session: session.to_owned(),
            identity,
            required: target.required,
            state: Mutex::new(State {
                outputs,
                seq,
                prev,
                failure: None,
            }),
        })
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
                outputs: sinks
                    .into_iter()
                    .map(|(path, writer)| Output {
                        path,
                        writer,
                        expected: None,
                    })
                    .collect(),
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
    /// Before writing, each file must still be the one this log opened,
    /// with exactly the bytes it wrote: a file that was removed, replaced,
    /// cut or appended to by something else is a failed write.
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
        for output in &mut state.outputs {
            if let Err(error) = output.write(&line) {
                first_error.get_or_insert(error);
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

impl Output {
    /// Checks the file is still this log's, then writes one line to it.
    fn write(&mut self, line: &str) -> Result<(), AuditError> {
        if let Some(expected) = &self.expected
            && let Some(reason) = expected.changed(&self.path)
        {
            return Err(AuditError::Io {
                path: self.path.clone(),
                detail: reason,
            });
        }
        self.writer
            .write_all(line.as_bytes())
            .and_then(|()| self.writer.flush())
            .map_err(|error| AuditError::io(self.path.as_path(), &error))?;
        if let Some(expected) = &mut self.expected {
            expected.len += u64::try_from(line.len()).unwrap_or(u64::MAX);
        }
        Ok(())
    }
}

/// A file's text, or nothing when it does not exist.
fn read_existing(path: &Path) -> Result<String, AuditError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(AuditError::io(path, &error)),
    }
}

/// Opens a log file for appending, writes `seed` to it first, and records
/// what it then looks like.
fn open_output(path: &Path, seed: &str) -> Result<Output, AuditError> {
    let mut file = open_append(path)?;
    if !seed.is_empty() {
        file.write_all(seed.as_bytes())
            .and_then(|()| file.flush())
            .map_err(|error| AuditError::io(path, &error))?;
    }
    let expected = Expected::of(&file, path)?;
    Ok(Output {
        path: path.to_path_buf(),
        writer: Box::new(file),
        expected: Some(expected),
    })
}

/// Opens a file for appending, creating it and its folders owner-only.
///
/// On Windows other programs may read the file while it is open but not
/// write, rename or delete it.
fn open_append(path: &Path) -> Result<File, AuditError> {
    if let Some(parent) = path.parent() {
        anvil_types::private_fs::create_dir_all(parent)
            .map_err(|error| AuditError::io(parent, &error))?;
    }
    let mut options = anvil_types::private_fs::owner_only();
    options.create(true).append(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        const FILE_SHARE_READ: u32 = 0x1;
        options.share_mode(FILE_SHARE_READ);
    }
    let file = options
        .open(path)
        .map_err(|error| AuditError::io(path, &error))?;
    anvil_types::private_fs::narrow(&file).map_err(|error| AuditError::io(path, &error))?;
    Ok(file)
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

    #[test]
    fn a_mirror_added_later_receives_the_whole_chain_and_verifies_on_its_own() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut target = AuditTarget {
            primary: dir.path().join("s").join(AUDIT_FILE),
            mirror: None,
            required: true,
        };
        let log = AuditLog::open("s1", identity(), &target).expect("open");
        log.append("main", end("a")).expect("append");
        log.append("main", end("b")).expect("append");
        drop(log);
        target.mirror = Some(mirror_file(&dir.path().join("mirror"), "s1"));
        let log = AuditLog::open("s1", identity(), &target).expect("reopen");
        log.append("main", end("c")).expect("append");
        drop(log);
        let primary = std::fs::read_to_string(&target.primary).expect("read");
        let mirror =
            std::fs::read_to_string(target.mirror.as_ref().expect("mirror")).expect("read");
        assert_eq!(primary, mirror);
        assert_eq!(
            verify(&mirror).expect("the mirror verifies").records.len(),
            3
        );
    }

    #[test]
    fn a_mirror_that_differs_from_the_session_file_stops_a_required_log() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = AuditTarget {
            primary: dir.path().join("s").join(AUDIT_FILE),
            mirror: Some(mirror_file(&dir.path().join("mirror"), "s1")),
            required: true,
        };
        let log = AuditLog::open("s1", identity(), &target).expect("open");
        log.append("main", end("a")).expect("append");
        log.append("main", end("b")).expect("append");
        drop(log);
        let mirror = target.mirror.clone().expect("mirror");
        let full = std::fs::read_to_string(&mirror).expect("read");
        let cut = format!("{}\n", full.lines().next().expect("line"));
        std::fs::write(&target.primary, &cut).expect("cut the primary");
        assert!(matches!(
            AuditLog::open("s1", identity(), &target),
            Err(AuditError::MirrorDiffers { .. })
        ));
        std::fs::remove_file(&target.primary).expect("remove the primary");
        assert!(matches!(
            AuditLog::open("s1", identity(), &target),
            Err(AuditError::MirrorDiffers { .. })
        ));
        let lenient = AuditTarget {
            required: false,
            ..target
        };
        AuditLog::open("s1", identity(), &lenient).expect("not required: continues");
    }

    #[test]
    fn a_removed_or_replaced_session_file_fails_the_next_write() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = AuditTarget {
            primary: dir.path().join(AUDIT_FILE),
            mirror: None,
            required: true,
        };
        let log = AuditLog::open("s1", identity(), &target).expect("open");
        log.append("main", end("a")).expect("append");
        // Where the operating system refuses the change while the log is
        // open, that refusal is the protection; otherwise the next write fails.
        if std::fs::remove_file(&target.primary).is_ok() {
            assert!(
                log.append("main", end("b")).is_err(),
                "a removed file is noticed"
            );
            assert!(log.failure().is_some());
        }
        let log = AuditLog::open(
            "s2",
            identity(),
            &AuditTarget {
                primary: dir.path().join("t").join(AUDIT_FILE),
                ..target.clone()
            },
        )
        .expect("open");
        log.append("main", end("a")).expect("append");
        let path = dir.path().join("t").join(AUDIT_FILE);
        if std::fs::write(&path, "").is_ok() {
            assert!(
                log.append("main", end("b")).is_err(),
                "a truncated file is noticed"
            );
        }
        let replacement = dir.path().join("t").join("other");
        std::fs::write(&replacement, std::fs::read(&path).unwrap_or_default()).expect("copy");
        if std::fs::rename(&replacement, &path).is_ok() {
            assert!(
                log.append("main", end("c")).is_err(),
                "a replaced file is noticed"
            );
        }
    }
}
