//! Runs a task's `check.sh` and finds the programs it needs.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::process::Command;

use crate::error::EvalError;

/// How many trailing lines of output are kept.
pub const TAIL_LINES: usize = 30;

/// What `check.sh` reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckResult {
    /// Whether the script exited 0.
    pub passed: bool,
    /// The exit code, if the script ran to completion.
    pub exit_code: Option<i32>,
    /// Whether the harness killed the script at the timeout.
    pub timed_out: bool,
    /// The last lines of stdout and stderr.
    pub output_tail: String,
    /// Wall time.
    pub check_ms: u64,
}

impl CheckResult {
    /// A failed check that never ran.
    ///
    /// # Arguments
    ///
    /// * `message` - Why it did not run.
    ///
    /// # Returns
    ///
    /// A result with `passed = false` and the message as output.
    pub fn not_run(message: String) -> Self {
        CheckResult {
            passed: false,
            exit_code: None,
            timed_out: false,
            output_tail: message,
            check_ms: 0,
        }
    }
}

/// Runs `script` with bash inside `work_dir`.
///
/// # Arguments
///
/// * `bash` - The bash to use; see [`find_bash`].
/// * `script` - The `check.sh`; it stays in the task folder and is passed
///   with forward slashes so Git Bash accepts it.
/// * `work_dir` - The fixture copy the script inspects.
/// * `timeout` - When to kill the script.
/// * `env` - Extra environment for the script.
///
/// # Returns
///
/// The result; a timeout is a failed check, not an error.
///
/// # Errors
///
/// When bash cannot be started.
pub async fn run_check(
    bash: &Path,
    script: &Path,
    work_dir: &Path,
    timeout: Duration,
    env: &[(String, String)],
) -> Result<CheckResult, EvalError> {
    let clock = Instant::now();
    let child = Command::new(bash)
        .arg(forward_slashes(script))
        .current_dir(work_dir)
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|source| EvalError::Spawn {
            program: bash.to_owned(),
            source,
        })?;
    let waited = tokio::time::timeout(timeout, child.wait_with_output()).await;
    let check_ms = clock.elapsed().as_millis() as u64;
    match waited {
        Ok(Ok(output)) => {
            let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&output.stderr));
            Ok(CheckResult {
                passed: output.status.success(),
                exit_code: output.status.code(),
                timed_out: false,
                output_tail: tail(&text, TAIL_LINES),
                check_ms,
            })
        }
        Ok(Err(source)) => Err(EvalError::Spawn {
            program: bash.to_owned(),
            source,
        }),
        Err(_elapsed) => Ok(CheckResult {
            passed: false,
            exit_code: None,
            timed_out: true,
            output_tail: format!("check.sh killed after {}s", timeout.as_secs()),
            check_ms,
        }),
    }
}

/// Finds a bash that runs the check scripts.
///
/// On Windows the Git for Windows bash is tried first, because the
/// `bash.exe` in `System32` is the WSL launcher and cannot see Windows paths.
///
/// # Returns
///
/// The bash executable.
///
/// # Errors
///
/// [`EvalError::BashNotFound`].
pub fn find_bash() -> Result<PathBuf, EvalError> {
    if cfg!(windows) {
        for base in ["ProgramFiles", "ProgramW6432", "LOCALAPPDATA"] {
            let Some(root) = std::env::var_os(base) else {
                continue;
            };
            let root = PathBuf::from(root);
            let candidates = [
                root.join("Git").join("bin").join("bash.exe"),
                root.join("Git").join("usr").join("bin").join("bash.exe"),
                root.join("Programs")
                    .join("Git")
                    .join("bin")
                    .join("bash.exe"),
            ];
            if let Some(found) = candidates.into_iter().find(|p| p.is_file()) {
                return Ok(found);
            }
        }
    }
    find_on_path("bash")
        .filter(|path| !is_wsl_launcher(path))
        .ok_or(EvalError::BashNotFound)
}

/// Finds a Python 3 that answers `python -c`.
///
/// `python3` is tried before `python`. Each candidate is run once, which
/// rejects the Microsoft Store stub that only opens the store.
///
/// # Returns
///
/// The interpreter.
///
/// # Errors
///
/// [`EvalError::PythonNotFound`].
pub fn find_python() -> Result<PathBuf, EvalError> {
    ["python3", "python"]
        .iter()
        .filter_map(|name| find_on_path(name))
        .find(|path| answers_python3(path))
        .ok_or(EvalError::PythonNotFound)
}

/// The first `name` (with `.exe` on Windows) in `PATH`.
///
/// # Arguments
///
/// * `name` - The program name without extension.
///
/// # Returns
///
/// The path, if found.
pub fn find_on_path(name: &str) -> Option<PathBuf> {
    let file = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    };
    std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .map(|dir| dir.join(&file))
        .find(|candidate| candidate.is_file())
}

/// Whether a `bash.exe` is Windows' WSL launcher.
fn is_wsl_launcher(path: &Path) -> bool {
    path.components().any(|part| {
        part.as_os_str()
            .to_str()
            .is_some_and(|s| s.eq_ignore_ascii_case("system32"))
    })
}

/// Whether running `path -c ...` reports a Python 3.
fn answers_python3(path: &Path) -> bool {
    std::process::Command::new(path)
        .args(["-c", "import sys; print(sys.version_info[0])"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .is_ok_and(|out| out.status.success() && String::from_utf8_lossy(&out.stdout).trim() == "3")
}

/// The path as a string with forward slashes, which Git Bash and Windows
/// both accept.
///
/// # Arguments
///
/// * `path` - Any path.
///
/// # Returns
///
/// The text form.
pub fn forward_slashes(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// The last `lines` lines of `text`.
///
/// # Arguments
///
/// * `text` - The output.
/// * `lines` - How many to keep.
///
/// # Returns
///
/// The tail, joined with newlines.
pub fn tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(lines);
    all[start..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_keeps_the_last_lines() {
        assert_eq!(tail("a\nb\nc\n", 2), "b\nc");
        assert_eq!(tail("a\n", 5), "a");
        assert_eq!(tail("", 5), "");
    }

    #[test]
    fn forward_slashes_replaces_backslashes() {
        assert_eq!(
            forward_slashes(Path::new("C:\\x\\y\\check.sh")),
            "C:/x/y/check.sh"
        );
    }

    #[test]
    fn wsl_launcher_is_recognised() {
        assert!(is_wsl_launcher(Path::new(
            "C:\\Windows\\System32\\bash.exe"
        )));
        assert!(!is_wsl_launcher(Path::new(
            "C:\\Program Files\\Git\\bin\\bash.exe"
        )));
    }

    #[tokio::test]
    async fn runs_scripts_and_reports_exit_codes_and_timeouts() {
        let bash = find_bash().expect("bash is installed");
        let dir = tempfile::tempdir().expect("tmp");
        let script = dir.path().join("check.sh");
        std::fs::write(
            &script,
            "#!/usr/bin/env bash\necho \"task=$EVAL_TASK_DIR\"\ntest -f marker\n",
        )
        .expect("write");
        let env = vec![("EVAL_TASK_DIR".to_owned(), "/tasks/x".to_owned())];
        let timeout = Duration::from_secs(30);

        let failed = run_check(&bash, &script, dir.path(), timeout, &env)
            .await
            .expect("runs");
        assert!(!failed.passed);
        assert_eq!(failed.exit_code, Some(1));
        assert!(failed.output_tail.contains("task=/tasks/x"));

        std::fs::write(dir.path().join("marker"), "").expect("write");
        let passed = run_check(&bash, &script, dir.path(), timeout, &env)
            .await
            .expect("runs");
        assert!(passed.passed);
        assert_eq!(passed.exit_code, Some(0));

        std::fs::write(&script, "#!/usr/bin/env bash\nsleep 20\n").expect("write");
        let slow = run_check(&bash, &script, dir.path(), Duration::from_millis(300), &env)
            .await
            .expect("runs");
        assert!(slow.timed_out);
        assert!(!slow.passed);
        assert_eq!(slow.exit_code, None);
    }

    #[test]
    fn finds_a_python_three() {
        let python = find_python().expect("python is installed");
        assert!(python.is_file());
    }
}
