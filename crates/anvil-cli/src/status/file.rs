//! Files of the status feature: the atomic `status.json` write, the
//! liveness lock, and the per-user list of projects that `anvil status
//! --all` walks.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anvil_core::context::session::SESSIONS_DIR;
use anvil_types::SessionStatus;
use anvil_types::status::{LOCK_FILE, STATUS_FILE};
use sha2::{Digest, Sha256};

/// Folder under the anvil home with one small file per project that ran a session.
const PROJECTS_DIR: &str = ".anvil/projects";

/// The status file of one session, plus the lock that says its process is alive.
#[derive(Debug)]
pub struct StatusFile {
    dir: PathBuf,
    /// Held for the life of the process; the operating system releases it
    /// when the process ends, however it ends.
    _lock: Option<File>,
}

impl StatusFile {
    /// Creates the session folder if needed and takes the liveness lock.
    ///
    /// A lock another process already holds is not an error: the session
    /// still reports, it just cannot prove it is alive.
    ///
    /// # Arguments
    ///
    /// * `dir` - The session folder.
    ///
    /// # Errors
    ///
    /// When the folder or the lock file cannot be created.
    pub fn open(dir: &Path) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join(LOCK_FILE))?;
        let lock = match lock.try_lock() {
            Ok(()) => Some(lock),
            Err(TryLockError::WouldBlock) => {
                tracing::warn!(dir = %dir.display(), "another process holds this session's status lock");
                None
            }
            Err(TryLockError::Error(error)) => return Err(error),
        };
        Ok(Self {
            dir: dir.to_path_buf(),
            _lock: lock,
        })
    }

    /// Replaces `status.json` atomically.
    ///
    /// # Arguments
    ///
    /// * `status` - The content.
    ///
    /// # Errors
    ///
    /// When the file cannot be serialized or written; the previous file is
    /// then left as it was.
    pub fn write(&self, status: &SessionStatus) -> io::Result<()> {
        let mut text = serde_json::to_string_pretty(status).map_err(io::Error::other)?;
        text.push('\n');
        write_atomic(&self.dir, STATUS_FILE, text.as_bytes())
    }
}

/// Writes a file so that a reader sees the old content or the new, never
/// half of either: the bytes go to a temporary file in the same folder
/// (same volume, so the rename cannot copy), then the rename replaces the
/// target.
///
/// # Arguments
///
/// * `dir` - The folder.
/// * `name` - The file name inside it.
/// * `bytes` - The content.
///
/// # Errors
///
/// When the temporary file cannot be written or renamed; the target is
/// then untouched and the temporary file is removed.
pub fn write_atomic(dir: &Path, name: &str, bytes: &[u8]) -> io::Result<()> {
    let temp = dir.join(format!("{name}.tmp"));
    let result = (|| {
        let mut file = File::create(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, dir.join(name))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// Whether nobody holds a session's liveness lock, meaning its process is
/// gone (or never took the lock).
///
/// # Arguments
///
/// * `dir` - The session folder.
///
/// # Returns
///
/// True when the lock file is missing or can be taken.
pub fn lock_is_free(dir: &Path) -> bool {
    let Ok(file) = OpenOptions::new().read(true).open(dir.join(LOCK_FILE)) else {
        return true;
    };
    match file.try_lock_shared() {
        Ok(()) => true,
        Err(TryLockError::WouldBlock) => false,
        // Unreadable lock state: do not call a session dead on a guess.
        Err(TryLockError::Error(_)) => false,
    }
}

/// Reads a session's `status.json`.
///
/// # Arguments
///
/// * `dir` - The session folder.
///
/// # Returns
///
/// The status, or `None` when the file is missing or invalid (an older
/// session, or one being replaced right now).
pub fn read_status(dir: &Path) -> Option<SessionStatus> {
    let text = fs::read_to_string(dir.join(STATUS_FILE)).ok()?;
    match SessionStatus::parse(&text) {
        Ok(status) => Some(status),
        Err(error) => {
            tracing::debug!(dir = %dir.display(), %error, "skipping an invalid status file");
            None
        }
    }
}

/// The session folders of a project.
///
/// # Arguments
///
/// * `project` - The project root.
///
/// # Returns
///
/// Every directory under the project's sessions folder; empty when there is none.
pub fn session_dirs(project: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(project.join(SESSIONS_DIR)) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect()
}

/// Remembers that a project ran a session, so `anvil status --all` can
/// find it. Telemetry records only a digest of the project path, so it
/// cannot be used to walk projects.
///
/// # Arguments
///
/// * `home` - The anvil home directory.
/// * `project` - The project root.
///
/// # Errors
///
/// When the marker cannot be written.
pub fn register_project(home: &Path, project: &Path) -> io::Result<()> {
    let dir = home.join(PROJECTS_DIR);
    fs::create_dir_all(&dir)?;
    let path_text = project.to_string_lossy();
    let digest = Sha256::digest(path_text.as_bytes());
    let name: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    write_atomic(&dir, &name, path_text.as_bytes())
}

/// Every project registered under a home directory.
///
/// # Arguments
///
/// * `home` - The anvil home directory.
///
/// # Returns
///
/// The project roots, sorted; empty when none is registered.
pub fn registered_projects(home: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(home.join(PROJECTS_DIR)) else {
        return Vec::new();
    };
    let mut projects: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| fs::read_to_string(entry.path()).ok())
        .map(|text| PathBuf::from(text.trim()))
        .collect();
    projects.sort();
    projects
}

#[cfg(test)]
mod tests {
    use super::*;
    use anvil_types::status::SCHEMA_VERSION;
    use anvil_types::{LoopState, SessionSpend, SessionState};
    use chrono::Utc;

    fn status(detail: &str) -> SessionStatus {
        SessionStatus {
            schema: SCHEMA_VERSION,
            session_id: "S1".to_owned(),
            project: "/p".to_owned(),
            task: "t".to_owned(),
            state: SessionState::Working,
            loop_state: Some(LoopState::Build),
            detail: detail.to_owned(),
            spend: SessionSpend::default(),
            started_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn the_file_always_parses_while_it_is_rewritten() {
        let dir = tempfile::tempdir().expect("dir");
        let file = StatusFile::open(dir.path()).expect("open");
        file.write(&status("first")).expect("write");
        let path = dir.path().to_path_buf();
        let reader = std::thread::spawn(move || {
            let mut seen = 0;
            for _ in 0..2_000 {
                // A read can fail while the rename is in flight on Windows; a
                // read that succeeds must hold a whole document.
                if let Ok(text) = fs::read_to_string(path.join(STATUS_FILE)) {
                    SessionStatus::parse(&text).expect("a whole document");
                    seen += 1;
                }
            }
            seen
        });
        for n in 0..300 {
            file.write(&status(&format!("write {n}"))).expect("write");
        }
        assert!(reader.join().expect("reader") > 0);
        assert_eq!(read_status(dir.path()).expect("status").detail, "write 299");
        assert!(!dir.path().join("status.json.tmp").exists());
    }

    #[test]
    fn a_failed_write_keeps_the_previous_content() {
        let dir = tempfile::tempdir().expect("dir");
        let file = StatusFile::open(dir.path()).expect("open");
        file.write(&status("before")).expect("write");
        // A directory squatting on the temporary name makes the write fail.
        fs::create_dir(dir.path().join("status.json.tmp")).expect("blocker");
        assert!(file.write(&status("after")).is_err());
        assert_eq!(read_status(dir.path()).expect("status").detail, "before");
    }

    #[test]
    fn the_lock_is_held_while_the_writer_lives() {
        let dir = tempfile::tempdir().expect("dir");
        assert!(lock_is_free(dir.path()), "no lock file yet");
        let file = StatusFile::open(dir.path()).expect("open");
        assert!(!lock_is_free(dir.path()));
        drop(file);
        assert!(lock_is_free(dir.path()));
    }

    #[test]
    fn projects_register_once_and_list_sorted() {
        let home = tempfile::tempdir().expect("home");
        register_project(home.path(), Path::new("/b/proj")).expect("register");
        register_project(home.path(), Path::new("/a/proj")).expect("register");
        register_project(home.path(), Path::new("/a/proj")).expect("again");
        assert_eq!(
            registered_projects(home.path()),
            vec![PathBuf::from("/a/proj"), PathBuf::from("/b/proj")]
        );
        assert!(registered_projects(&home.path().join("none")).is_empty());
    }
}
