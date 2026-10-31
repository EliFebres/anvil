//! Task folders: `fixture/`, `task.md`, `check.sh` and `meta.toml`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::EvalError;

/// Where the tasks live, relative to the repository root.
pub const TASKS_DIR: &str = "eval/tasks";

/// The tiny repository the task starts from.
pub const FIXTURE_DIR: &str = "fixture";

/// The prompt anvil receives.
pub const TASK_FILE: &str = "task.md";

/// The script whose exit code decides pass or fail.
pub const CHECK_FILE: &str = "check.sh";

/// Tags, timeout and token cap.
pub const META_FILE: &str = "meta.toml";

/// The longest prompt accepted; it travels as one command-line argument.
pub const MAX_PROMPT_BYTES: usize = 8 * 1024;

/// The `meta.toml` of a task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskMeta {
    /// Labels such as the language and the kind of change; `--tags` filters on them.
    pub tags: Vec<String>,
    /// Wall-clock limit for the anvil run, and separately for `check.sh`.
    pub timeout_s: u64,
    /// Session token budget handed to anvil.
    pub max_tokens: u64,
}

/// One loaded task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    /// The folder name, kebab-case.
    pub name: String,
    /// The task folder.
    pub dir: PathBuf,
    /// The contents of `task.md`.
    pub prompt: String,
    /// The contents of `meta.toml`.
    pub meta: TaskMeta,
}

impl Task {
    /// Loads and validates one task folder.
    ///
    /// # Arguments
    ///
    /// * `dir` - The task folder.
    ///
    /// # Returns
    ///
    /// The task.
    ///
    /// # Errors
    ///
    /// [`EvalError::InvalidTask`] when the name is not kebab-case, a part is
    /// missing, the prompt is empty or over [`MAX_PROMPT_BYTES`], a limit is
    /// zero, there are no tags, or the fixture carries a `.git` or `.anvil`
    /// folder (the harness owns both).
    pub fn load(dir: &Path) -> Result<Task, EvalError> {
        let invalid = |reason: String| EvalError::InvalidTask {
            dir: dir.to_owned(),
            reason,
        };
        let name = dir
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| invalid("folder name is not valid UTF-8".to_owned()))?
            .to_owned();
        if !is_kebab_case(&name) {
            return Err(invalid(format!("name `{name}` is not kebab-case")));
        }
        for part in [FIXTURE_DIR, TASK_FILE, CHECK_FILE, META_FILE] {
            if !dir.join(part).exists() {
                return Err(invalid(format!("missing {part}")));
            }
        }
        for owned in [".git", ".anvil"] {
            if dir.join(FIXTURE_DIR).join(owned).exists() {
                return Err(invalid(format!("fixture must not contain {owned}")));
            }
        }
        let prompt_path = dir.join(TASK_FILE);
        let prompt = std::fs::read_to_string(&prompt_path)
            .map_err(|source| EvalError::io("reading", &prompt_path, source))?;
        if prompt.trim().is_empty() {
            return Err(invalid(format!("{TASK_FILE} is empty")));
        }
        if prompt.len() > MAX_PROMPT_BYTES {
            return Err(invalid(format!(
                "{TASK_FILE} is {} bytes; the limit is {MAX_PROMPT_BYTES}",
                prompt.len()
            )));
        }
        let meta_path = dir.join(META_FILE);
        let meta_text = std::fs::read_to_string(&meta_path)
            .map_err(|source| EvalError::io("reading", &meta_path, source))?;
        let meta: TaskMeta =
            toml::from_str(&meta_text).map_err(|source| EvalError::toml(&meta_path, source))?;
        if meta.timeout_s == 0 {
            return Err(invalid("timeout_s must be at least 1".to_owned()));
        }
        if meta.max_tokens == 0 {
            return Err(invalid("max_tokens must be at least 1".to_owned()));
        }
        if meta.tags.is_empty() || meta.tags.iter().any(|tag| tag.trim().is_empty()) {
            return Err(invalid(
                "tags must hold at least one non-empty tag".to_owned(),
            ));
        }
        Ok(Task {
            name,
            dir: dir.to_owned(),
            prompt,
            meta,
        })
    }

    /// The fixture folder.
    ///
    /// # Returns
    ///
    /// `<dir>/fixture`.
    pub fn fixture_dir(&self) -> PathBuf {
        self.dir.join(FIXTURE_DIR)
    }

    /// The check script.
    ///
    /// # Returns
    ///
    /// `<dir>/check.sh`.
    pub fn check_script(&self) -> PathBuf {
        self.dir.join(CHECK_FILE)
    }

    /// Whether the task carries any of `tags`.
    ///
    /// # Arguments
    ///
    /// * `tags` - The filter; empty matches every task.
    ///
    /// # Returns
    ///
    /// `true` when no filter is given or at least one tag matches.
    pub fn matches(&self, tags: &[String]) -> bool {
        tags.is_empty() || tags.iter().any(|tag| self.meta.tags.contains(tag))
    }

    /// The task's wall-clock limit.
    ///
    /// # Returns
    ///
    /// `timeout_s` as a duration.
    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.meta.timeout_s)
    }
}

/// Loads every task under `tasks_dir` that matches `tags`, sorted by name.
///
/// # Arguments
///
/// * `tasks_dir` - The folder holding one sub-folder per task.
/// * `tags` - The filter; empty keeps every task.
///
/// # Returns
///
/// The tasks, sorted by name.
///
/// # Errors
///
/// When the folder cannot be read, a task is invalid, or nothing matches.
pub fn load_tasks(tasks_dir: &Path, tags: &[String]) -> Result<Vec<Task>, EvalError> {
    let entries = std::fs::read_dir(tasks_dir)
        .map_err(|source| EvalError::io("reading", tasks_dir, source))?;
    let mut tasks = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| EvalError::io("reading", tasks_dir, source))?;
        if !entry.path().is_dir() {
            continue;
        }
        let task = Task::load(&entry.path())?;
        if task.matches(tags) {
            tasks.push(task);
        }
    }
    if tasks.is_empty() {
        return Err(EvalError::NoTasks {
            dir: tasks_dir.to_owned(),
            tags: tags.join(","),
        });
    }
    tasks.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(tasks)
}

/// Copies a task's fixture into `dest`, creating it.
///
/// # Arguments
///
/// * `task` - The task.
/// * `dest` - The destination folder; created if missing.
///
/// # Errors
///
/// When any file cannot be copied.
pub fn copy_fixture(task: &Task, dest: &Path) -> Result<(), EvalError> {
    copy_dir(&task.fixture_dir(), dest)
}

/// Recursively copies `src` into `dest`.
fn copy_dir(src: &Path, dest: &Path) -> Result<(), EvalError> {
    std::fs::create_dir_all(dest).map_err(|source| EvalError::io("creating", dest, source))?;
    let entries = std::fs::read_dir(src).map_err(|source| EvalError::io("reading", src, source))?;
    for entry in entries {
        let entry = entry.map_err(|source| EvalError::io("reading", src, source))?;
        let from = entry.path();
        let to = dest.join(entry.file_name());
        if from.is_dir() {
            copy_dir(&from, &to)?;
        } else {
            std::fs::copy(&from, &to).map_err(|source| EvalError::io("copying", &from, source))?;
        }
    }
    Ok(())
}

/// Whether `name` is lowercase letters, digits and single hyphens.
fn is_kebab_case(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--")
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    const META: &str = "tags = [\"py\", \"flag\"]\ntimeout_s = 60\nmax_tokens = 1000\n";

    fn write_task(root: &Path, name: &str, meta: &str) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(dir.join(FIXTURE_DIR)).expect("fixture");
        std::fs::write(dir.join(FIXTURE_DIR).join("README.md"), "hi\n").expect("file");
        std::fs::write(dir.join(TASK_FILE), "Do the thing.\n").expect("task");
        std::fs::write(dir.join(CHECK_FILE), "#!/usr/bin/env bash\nexit 0\n").expect("check");
        std::fs::write(dir.join(META_FILE), meta).expect("meta");
        dir
    }

    fn reason_of(result: Result<Task, EvalError>) -> String {
        match result {
            Err(EvalError::InvalidTask { reason, .. }) => reason,
            other => panic!("expected InvalidTask, got {other:?}"),
        }
    }

    #[test]
    fn loads_a_well_formed_task() {
        let root = tempfile::tempdir().expect("tmp");
        let dir = write_task(root.path(), "py-add-flag", META);
        let task = Task::load(&dir).expect("loads");
        assert_eq!(task.name, "py-add-flag");
        assert_eq!(task.prompt, "Do the thing.\n");
        assert_eq!(task.meta.tags, vec!["py", "flag"]);
        assert_eq!(task.timeout(), Duration::from_secs(60));
        assert!(task.matches(&[]));
        assert!(task.matches(&["flag".to_owned()]));
        assert!(!task.matches(&["rs".to_owned()]));
    }

    #[test]
    fn rejects_malformed_tasks() {
        let root = tempfile::tempdir().expect("tmp");
        let bad_name = write_task(root.path(), "Bad_Name", META);
        assert!(reason_of(Task::load(&bad_name)).contains("kebab"));

        let missing = write_task(root.path(), "no-check", META);
        std::fs::remove_file(missing.join(CHECK_FILE)).expect("rm");
        assert!(reason_of(Task::load(&missing)).contains("check.sh"));

        let zero = write_task(
            root.path(),
            "zero",
            "tags = [\"py\"]\ntimeout_s = 0\nmax_tokens = 1\n",
        );
        assert!(reason_of(Task::load(&zero)).contains("timeout_s"));

        let no_tags = write_task(
            root.path(),
            "no-tags",
            "tags = []\ntimeout_s = 1\nmax_tokens = 1\n",
        );
        assert!(reason_of(Task::load(&no_tags)).contains("tags"));

        let owned = write_task(root.path(), "owned", META);
        std::fs::create_dir_all(owned.join(FIXTURE_DIR).join(".anvil")).expect("dir");
        assert!(reason_of(Task::load(&owned)).contains(".anvil"));

        let empty = write_task(root.path(), "empty-prompt", META);
        std::fs::write(empty.join(TASK_FILE), "  \n").expect("task");
        assert!(reason_of(Task::load(&empty)).contains("empty"));

        let unknown = write_task(
            root.path(),
            "unknown-key",
            "tags = [\"py\"]\ntimeout_s = 1\nmax_tokens = 1\nextra = 1\n",
        );
        assert!(matches!(Task::load(&unknown), Err(EvalError::Toml { .. })));
    }

    #[test]
    fn load_tasks_filters_and_sorts() {
        let root = tempfile::tempdir().expect("tmp");
        write_task(root.path(), "b-task", META);
        write_task(
            root.path(),
            "a-task",
            "tags = [\"rs\"]\ntimeout_s = 1\nmax_tokens = 1\n",
        );
        std::fs::write(root.path().join("notes.txt"), "ignored").expect("file");
        let all = load_tasks(root.path(), &[]).expect("tasks");
        let names: Vec<&str> = all.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["a-task", "b-task"]);
        let py = load_tasks(root.path(), &["py".to_owned()]).expect("tasks");
        assert_eq!(py.len(), 1);
        assert!(matches!(
            load_tasks(root.path(), &["none".to_owned()]),
            Err(EvalError::NoTasks { tags, .. }) if tags == "none"
        ));
    }

    #[test]
    fn copy_fixture_copies_nested_files() {
        let root = tempfile::tempdir().expect("tmp");
        let dir = write_task(root.path(), "copy-me", META);
        std::fs::create_dir_all(dir.join(FIXTURE_DIR).join("src")).expect("dir");
        std::fs::write(dir.join(FIXTURE_DIR).join("src/main.py"), "print(1)\n").expect("file");
        let task = Task::load(&dir).expect("loads");
        let dest = root.path().join("work");
        copy_fixture(&task, &dest).expect("copies");
        assert_eq!(
            std::fs::read_to_string(dest.join("src/main.py")).expect("read"),
            "print(1)\n"
        );
        assert!(dest.join("README.md").exists());
    }
}
