//! The files anvil left behind, and what the owner changed in them since.
//!
//! When a session ends, the files it changed are recorded in
//! `~/.anvil/memory/corrections/<repo>.toml` with a SHA-256 of their bytes
//! and a copy of their text (small files only, so the record stays small
//! and needs no git history). Later, a file whose hash differs is one the
//! owner edited: the difference between the copy and the file now is the
//! best signal of what the owner wants. Once a correction is read it leaves
//! the record, so it is never proposed twice.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use similar::TextDiff;

use crate::error::MemoryError;

/// The record format this code reads and writes.
pub const RECORD_VERSION: u32 = 1;

/// Largest file, in bytes, whose text is recorded; bigger files are skipped.
pub const MAX_FILE_BYTES: usize = 32 * 1024;

/// Most text, in bytes, one record holds; files that would exceed it are skipped.
pub const MAX_RECORD_BYTES: usize = 128 * 1024;

/// Most diff text, in bytes, one learning request carries.
pub const MAX_DIFF_BYTES: usize = 16 * 1024;

/// File names that are never learned from.
const GENERATED_NAMES: &[&str] = &[
    "Cargo.lock",
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "poetry.lock",
    "Gemfile.lock",
    "go.sum",
];

/// Directories whose files are never learned from.
const GENERATED_DIRS: &[&str] = &["target", "dist", "node_modules", ".git", ".anvil"];

/// One file as anvil left it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeftFile {
    /// The path relative to the project root, with `/` separators.
    pub path: String,
    /// Lowercase hex SHA-256 of the file's bytes.
    pub hash: String,
    /// The file's text.
    pub content: String,
}

/// What the owner changed in one file anvil left.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Correction {
    /// The path relative to the project root.
    pub path: String,
    /// A unified diff from what anvil left to what the file holds now.
    pub diff: String,
}

/// The files anvil left in one project.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// The format version, [`RECORD_VERSION`].
    pub version: u32,
    /// The files, in the order they were recorded.
    #[serde(default)]
    pub files: Vec<LeftFile>,
}

impl Default for Record {
    fn default() -> Self {
        Self {
            version: RECORD_VERSION,
            files: Vec::new(),
        }
    }
}

/// Decides which files are not worth learning from.
pub struct Skip {
    gitignore: Gitignore,
}

impl Skip {
    /// The skip rules of a project: generated files plus its root `.gitignore`.
    ///
    /// # Arguments
    ///
    /// * `cwd` - The project root.
    ///
    /// # Returns
    ///
    /// The rules. An unreadable `.gitignore` is treated as empty.
    pub fn for_project(cwd: &Path) -> Self {
        let mut builder = GitignoreBuilder::new(cwd);
        let _ = builder.add(cwd.join(".gitignore"));
        Self {
            gitignore: builder.build().unwrap_or_else(|_| Gitignore::empty()),
        }
    }

    /// Whether a file is generated, a lock file, or ignored by git.
    ///
    /// # Arguments
    ///
    /// * `relative` - The path relative to the project root, `/` separated.
    ///
    /// # Returns
    ///
    /// `true` when it must not be recorded or learned from.
    pub fn skips(&self, relative: &str) -> bool {
        let mut parts: Vec<&str> = relative.split('/').collect();
        let name = parts.pop().unwrap_or_default();
        GENERATED_NAMES.contains(&name)
            || name.ends_with(".lock")
            || name.contains(".min.")
            || parts.iter().any(|dir| GENERATED_DIRS.contains(dir))
            || self
                .gitignore
                .matched_path_or_any_parents(relative, false)
                .is_ignore()
    }
}

impl Record {
    /// Reads a record; a missing file is an empty record.
    ///
    /// # Arguments
    ///
    /// * `path` - The record file.
    ///
    /// # Returns
    ///
    /// The record.
    ///
    /// # Errors
    ///
    /// [`MemoryError::Io`] when the file exists but cannot be read, and
    /// [`MemoryError::Parse`] when it is not valid or has another version.
    pub fn load(path: &Path) -> Result<Self, MemoryError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(MemoryError::io(path, &error)),
        };
        let record: Self = toml::from_str(&text).map_err(|error| MemoryError::Parse {
            path: path.to_path_buf(),
            detail: error.to_string(),
        })?;
        if record.version != RECORD_VERSION {
            return Err(MemoryError::Parse {
                path: path.to_path_buf(),
                detail: format!(
                    "record version {} is not the supported version {RECORD_VERSION}",
                    record.version
                ),
            });
        }
        Ok(record)
    }

    /// Writes the record, or deletes the file when it holds no files.
    ///
    /// # Arguments
    ///
    /// * `path` - The record file.
    ///
    /// # Errors
    ///
    /// [`MemoryError::Io`] when the file cannot be written or removed.
    pub fn save(&self, path: &Path) -> Result<(), MemoryError> {
        if self.files.is_empty() {
            return match std::fs::remove_file(path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(MemoryError::io(path, &error)),
            };
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| MemoryError::io(parent, &error))?;
        }
        let text = toml::to_string(self).map_err(|error| MemoryError::Parse {
            path: path.to_path_buf(),
            detail: error.to_string(),
        })?;
        std::fs::write(path, text).map_err(|error| MemoryError::io(path, &error))
    }

    /// Records the files a session changed, replacing earlier entries for
    /// the same paths.
    ///
    /// Files that are skipped, missing, not UTF-8 text, larger than
    /// [`MAX_FILE_BYTES`], or that would take the record past
    /// [`MAX_RECORD_BYTES`] are left out.
    ///
    /// # Arguments
    ///
    /// * `cwd` - The project root.
    /// * `changed` - Absolute paths of the files the session changed.
    /// * `skip` - The files not worth learning from.
    ///
    /// # Returns
    ///
    /// How many files were recorded.
    pub fn remember(&mut self, cwd: &Path, changed: &[PathBuf], skip: &Skip) -> usize {
        let mut recorded = 0;
        for path in changed {
            let Some(relative) = relative_path(cwd, path) else {
                continue;
            };
            if skip.skips(&relative) {
                continue;
            }
            let Ok(bytes) = std::fs::read(path) else {
                continue;
            };
            if bytes.len() > MAX_FILE_BYTES {
                continue;
            }
            let Ok(content) = String::from_utf8(bytes) else {
                continue;
            };
            self.files.retain(|file| file.path != relative);
            let held: usize = self.files.iter().map(|file| file.content.len()).sum();
            if held + content.len() > MAX_RECORD_BYTES {
                continue;
            }
            self.files.push(LeftFile {
                path: relative,
                hash: hash_hex(content.as_bytes()),
                content,
            });
            recorded += 1;
        }
        debug_assert!(
            self.files
                .iter()
                .map(|file| file.content.len())
                .sum::<usize>()
                <= MAX_RECORD_BYTES,
            "the record outgrew its cap"
        );
        recorded
    }

    /// Takes out the files the owner edited since anvil left them.
    ///
    /// Edited files leave the record and come back as corrections. Files
    /// that are gone, skipped or no longer text also leave it, since they
    /// have nothing to learn from; unchanged files stay in case the owner
    /// edits them later.
    ///
    /// # Arguments
    ///
    /// * `cwd` - The project root.
    /// * `skip` - The files not worth learning from.
    ///
    /// # Returns
    ///
    /// One correction per edited file, in record order.
    pub fn take_corrections(&mut self, cwd: &Path, skip: &Skip) -> Vec<Correction> {
        let mut corrections = Vec::new();
        self.files.retain(|left| {
            if skip.skips(&left.path) {
                return false;
            }
            let Ok(bytes) = std::fs::read(cwd.join(&left.path)) else {
                return false;
            };
            if hash_hex(&bytes) == left.hash {
                return true;
            }
            if let Ok(now) = String::from_utf8(bytes) {
                corrections.push(Correction {
                    path: left.path.clone(),
                    diff: unified_diff(&left.path, &left.content, &now),
                });
            }
            false
        });
        corrections
    }
}

/// Keeps the corrections' diffs within [`MAX_DIFF_BYTES`] in total.
///
/// # Arguments
///
/// * `corrections` - The corrections, in order.
///
/// # Returns
///
/// The corrections that fit, the last one cut with a note if needed; later
/// ones are dropped.
pub fn cap_diffs(corrections: &[Correction]) -> Vec<Correction> {
    const CUT_NOTE: &str = "\n... (diff cut)\n";
    let mut left = MAX_DIFF_BYTES;
    let mut kept = Vec::new();
    for correction in corrections {
        if left <= CUT_NOTE.len() {
            break;
        }
        let mut diff = correction.diff.clone();
        if diff.len() > left {
            let mut end = left - CUT_NOTE.len();
            while !diff.is_char_boundary(end) {
                end -= 1;
            }
            diff.truncate(end);
            diff.push_str(CUT_NOTE);
        }
        left -= diff.len();
        kept.push(Correction {
            path: correction.path.clone(),
            diff,
        });
    }
    kept
}

/// The corrections as the text a model reads: one fenced diff per file.
///
/// # Arguments
///
/// * `corrections` - The corrections, already capped by [`cap_diffs`].
///
/// # Returns
///
/// The text.
pub fn render_corrections(corrections: &[Correction]) -> String {
    let mut out = String::new();
    for correction in corrections {
        let _ = write!(
            out,
            "## {}\n\n```diff\n{}\n```\n\n",
            correction.path,
            correction.diff.trim_end()
        );
    }
    out.trim_end().to_owned()
}

/// The path relative to the project root with `/` separators.
fn relative_path(cwd: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(cwd).ok()?;
    let parts: Vec<_> = relative
        .components()
        .map(|part| part.as_os_str().to_str())
        .collect::<Option<_>>()?;
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// Lowercase hex SHA-256 of some bytes.
fn hash_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::new(), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// A unified diff of one file between what anvil left and what is there now.
fn unified_diff(path: &str, left: &str, now: &str) -> String {
    TextDiff::from_lines(left, now)
        .unified_diff()
        .context_radius(3)
        .header(&format!("anvil/{path}"), &format!("owner/{path}"))
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, relative: &str, text: &str) -> PathBuf {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, text).expect("write");
        path
    }

    #[test]
    fn an_edited_file_becomes_a_correction_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = write(dir.path(), "src/a.py", "def f():\n    return 1\n");
        let skip = Skip::for_project(dir.path());
        let mut record = Record::default();
        assert_eq!(
            record.remember(dir.path(), std::slice::from_ref(&file), &skip),
            1
        );
        assert!(record.take_corrections(dir.path(), &skip).is_empty());
        assert_eq!(record.files.len(), 1, "unchanged files stay");

        write(dir.path(), "src/a.py", "def f():\n    return 2\n");
        let corrections = record.take_corrections(dir.path(), &skip);
        assert_eq!(corrections.len(), 1);
        assert_eq!(corrections[0].path, "src/a.py");
        assert!(corrections[0].diff.contains("-    return 1"));
        assert!(corrections[0].diff.contains("+    return 2"));
        assert!(record.files.is_empty(), "processed corrections leave");
    }

    #[test]
    fn missing_lock_and_ignored_files_are_never_corrections() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), ".gitignore", "secret.txt\n");
        let changed = vec![
            write(dir.path(), "Cargo.lock", "a\n"),
            write(dir.path(), "web/app.min.js", "a\n"),
            write(dir.path(), "target/x.rs", "a\n"),
            write(dir.path(), "secret.txt", "a\n"),
            write(dir.path(), "gone.rs", "a\n"),
        ];
        let skip = Skip::for_project(dir.path());
        let mut record = Record::default();
        assert_eq!(record.remember(dir.path(), &changed, &skip), 1);
        assert_eq!(record.files[0].path, "gone.rs");
        for path in &changed {
            write(
                dir.path(),
                &relative_path(dir.path(), path).expect("rel"),
                "b\n",
            );
        }
        std::fs::remove_file(dir.path().join("gone.rs")).expect("remove");
        record.files.push(LeftFile {
            path: "Cargo.lock".to_owned(),
            hash: hash_hex(
                b"a
",
            ),
            content: "a
"
            .to_owned(),
        });
        assert!(record.take_corrections(dir.path(), &skip).is_empty());
        assert!(record.files.is_empty());
    }

    #[test]
    fn big_and_binary_files_are_not_recorded_and_the_total_is_capped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let big = write(dir.path(), "big.txt", &"x".repeat(MAX_FILE_BYTES + 1));
        let binary = dir.path().join("b.bin");
        std::fs::write(&binary, [0xff, 0xfe, 0x00]).expect("write");
        let skip = Skip::for_project(dir.path());
        let mut record = Record::default();
        assert_eq!(record.remember(dir.path(), &[big, binary], &skip), 0);

        let chunk = "y".repeat(MAX_FILE_BYTES);
        let many: Vec<PathBuf> = (0..6)
            .map(|index| write(dir.path(), &format!("f{index}.txt"), &chunk))
            .collect();
        assert_eq!(record.remember(dir.path(), &many, &skip), 4);
    }

    #[test]
    fn record_round_trips_is_versioned_and_deleted_when_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("corrections").join("p.toml");
        assert_eq!(
            Record::load(&path).expect("missing is empty"),
            Record::default()
        );
        let mut record = Record::default();
        record.files.push(LeftFile {
            path: "a.rs".to_owned(),
            hash: hash_hex(b"x\n"),
            content: "x\n\"quoted\"\n".to_owned(),
        });
        record.save(&path).expect("saves");
        assert_eq!(Record::load(&path).expect("loads"), record);
        std::fs::write(&path, "version = 2\n").expect("write");
        assert!(matches!(
            Record::load(&path),
            Err(MemoryError::Parse { .. })
        ));
        Record::default().save(&path).expect("clears");
        assert!(!path.exists());
    }

    #[test]
    fn diffs_are_capped_in_total() {
        let big = Correction {
            path: "a.txt".to_owned(),
            diff: "é".repeat(MAX_DIFF_BYTES),
        };
        let second = Correction {
            path: "b.txt".to_owned(),
            diff: "+x\n".to_owned(),
        };
        let capped = cap_diffs(&[big, second]);
        assert_eq!(capped.len(), 1, "the budget is spent on the first");
        assert!(capped[0].diff.len() <= MAX_DIFF_BYTES);
        assert!(capped[0].diff.ends_with("(diff cut)\n"));

        let small = cap_diffs(&[Correction {
            path: "c".to_owned(),
            diff: "+y\n".to_owned(),
        }]);
        assert_eq!(small[0].diff, "+y\n");
        assert!(render_corrections(&small).starts_with("## c\n\n```diff\n+y\n```"));
    }
}
