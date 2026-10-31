//! Drives the real `anvil` binary on a fixture copy.
//!
//! Each run gets a temporary `ANVIL_HOME` seeded from the user's real
//! `~/.anvil` (configuration, `.env`, rules), so API keys and prices flow in
//! while memory and telemetry written by the run never reach the real home.
//! The provider and token budget are set through a project
//! `.anvil/config.toml` written into the fixture copy, because the CLI has
//! no flags for them.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Instant;

use anvil_types::ProviderConfig;
use anvil_types::config::{CONFIG_DIR, CONFIG_FILE};
use serde::Serialize;
use tokio::process::Command;

use crate::check::{self, forward_slashes, tail};
use crate::error::EvalError;
use crate::results::AttemptStatus;
use crate::runner::{AttemptContext, AttemptFuture, AttemptOutcome, EvalRunner};
use crate::telemetry;

/// Files copied from the real `~/.anvil` into the temporary home.
const SEEDED_FILES: [&str; 3] = ["config.toml", ".env", "rules.toml"];

/// Folders copied from the real `~/.anvil` into the temporary home.
const SEEDED_DIRS: [&str; 1] = ["checks"];

/// Lines of stderr kept per attempt.
const STDERR_TAIL_LINES: usize = 20;

/// Where to find anvil and the user's configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnvilOptions {
    /// The anvil binary. A path ending in `.sh` runs through bash, which lets
    /// a script stand in for the binary in tests.
    pub anvil_bin: PathBuf,
    /// The home whose `.anvil` folder seeds each run; `None` seeds nothing.
    pub real_home: Option<PathBuf>,
}

/// Runs `anvil --yes run <prompt>` on fixture copies.
pub struct AnvilRunner {
    home: tempfile::TempDir,
    providers: BTreeMap<String, ProviderConfig>,
    options: AnvilOptions,
    provider: Option<String>,
    bash: Option<PathBuf>,
}

impl AnvilRunner {
    /// Prepares the temporary home and reads the providers for pricing.
    ///
    /// # Arguments
    ///
    /// * `options` - The binary and the real home.
    /// * `provider` - The coder provider to force, or `None` for the
    ///   configured `roles.coder`.
    ///
    /// # Returns
    ///
    /// The runner.
    ///
    /// # Errors
    ///
    /// When the home cannot be seeded, the configuration does not load, the
    /// provider is not configured, or a `.sh` binary is given and bash is
    /// missing.
    pub fn new(options: AnvilOptions, provider: Option<String>) -> Result<Self, EvalError> {
        let home = tempfile::Builder::new()
            .prefix("anvil-eval-home-")
            .tempdir()
            .map_err(|source| EvalError::io("creating", &std::env::temp_dir(), source))?;
        seed_home(options.real_home.as_deref(), home.path())?;
        let empty = home.path().join("empty");
        std::fs::create_dir_all(&empty)
            .map_err(|source| EvalError::io("creating", &empty, source))?;
        let loaded = anvil_types::config::load(Some(home.path()), &empty)?;
        if let Some(name) = &provider
            && !loaded.config.providers.contains_key(name)
        {
            return Err(EvalError::UnknownProvider { name: name.clone() });
        }
        let bash = if is_script(&options.anvil_bin) {
            Some(check::find_bash()?)
        } else {
            None
        };
        Ok(AnvilRunner {
            home,
            providers: loaded.config.providers,
            options,
            provider,
            bash,
        })
    }

    /// The temporary home every run of this runner uses.
    ///
    /// # Returns
    ///
    /// The folder that holds `.anvil`.
    pub fn home(&self) -> &Path {
        self.home.path()
    }

    /// Runs anvil once and recovers usage from the copy's telemetry.
    async fn attempt(&self, attempt: &AttemptContext<'_>) -> Result<AttemptOutcome, EvalError> {
        write_project_config(
            attempt.work_dir,
            self.provider.as_deref(),
            attempt.task.meta.max_tokens,
        )?;
        let clock = Instant::now();
        let child = self
            .command(attempt)
            .spawn()
            .map_err(|source| EvalError::Spawn {
                program: self.options.anvil_bin.clone(),
                source,
            })?;
        let waited = tokio::time::timeout(attempt.task.timeout(), child.wait_with_output()).await;
        let anvil_ms = clock.elapsed().as_millis() as u64;
        let (status, stderr) = match waited {
            Ok(Ok(output)) => {
                let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
                (status_from(output.status.code(), &stderr), stderr)
            }
            Ok(Err(source)) => {
                return Err(EvalError::Spawn {
                    program: self.options.anvil_bin.clone(),
                    source,
                });
            }
            Err(_elapsed) => (AttemptStatus::Timeout, String::new()),
        };
        let session = session_id(&stderr);
        let telemetry_dir = attempt.work_dir.join(CONFIG_DIR).join("telemetry");
        let usage = telemetry::read_session_usage(&telemetry_dir, session.as_deref())?;
        let cost_usd = telemetry::cost(&self.providers, &usage);
        Ok(AttemptOutcome {
            status,
            session,
            provider: usage.coder_provider,
            model: usage.coder_model,
            tokens: usage.total,
            cost_usd,
            anvil_ms,
            stderr_tail: tail(&stderr, STDERR_TAIL_LINES),
        })
    }

    /// `anvil --yes run <prompt>` in the fixture copy with the temp home.
    fn command(&self, attempt: &AttemptContext<'_>) -> Command {
        let mut command = match &self.bash {
            Some(bash) => {
                let mut command = Command::new(bash);
                command.arg(forward_slashes(&self.options.anvil_bin));
                command
            }
            None => Command::new(&self.options.anvil_bin),
        };
        command
            .arg("--yes")
            .arg("run")
            .arg(&attempt.task.prompt)
            .current_dir(attempt.work_dir)
            .env("ANVIL_HOME", forward_slashes(self.home.path()))
            .envs(attempt.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
    }
}

impl EvalRunner for AnvilRunner {
    fn label(&self) -> String {
        forward_slashes(&self.options.anvil_bin)
    }

    fn run_attempt<'a>(&'a self, attempt: &'a AttemptContext<'a>) -> AttemptFuture<'a> {
        Box::pin(self.attempt(attempt))
    }
}

/// The anvil to drive when `--anvil-bin` is not given.
///
/// # Arguments
///
/// * `repo_root` - The repository, whose `target/` is searched after `PATH`.
///
/// # Returns
///
/// `anvil` on `PATH`, else `target/release/anvil`, else `target/debug/anvil`.
///
/// # Errors
///
/// [`EvalError::AnvilNotFound`].
pub fn default_anvil_bin(repo_root: &Path) -> Result<PathBuf, EvalError> {
    if let Some(found) = check::find_on_path("anvil") {
        return Ok(found);
    }
    let file = if cfg!(windows) { "anvil.exe" } else { "anvil" };
    ["release", "debug"]
        .iter()
        .map(|profile| repo_root.join("target").join(profile).join(file))
        .find(|candidate| candidate.is_file())
        .ok_or(EvalError::AnvilNotFound)
}

/// Copies the user's configuration files into the temporary home.
fn seed_home(real_home: Option<&Path>, home: &Path) -> Result<(), EvalError> {
    let dest = home.join(CONFIG_DIR);
    std::fs::create_dir_all(&dest).map_err(|source| EvalError::io("creating", &dest, source))?;
    let Some(real_home) = real_home else {
        return Ok(());
    };
    let src = real_home.join(CONFIG_DIR);
    for name in SEEDED_FILES {
        let from = src.join(name);
        if from.is_file() {
            std::fs::copy(&from, dest.join(name))
                .map_err(|source| EvalError::io("copying", &from, source))?;
        }
    }
    for name in SEEDED_DIRS {
        let from = src.join(name);
        if from.is_dir() {
            copy_flat(&from, &dest.join(name))?;
        }
    }
    Ok(())
}

/// Copies the files directly under `src` into `dest`.
fn copy_flat(src: &Path, dest: &Path) -> Result<(), EvalError> {
    std::fs::create_dir_all(dest).map_err(|source| EvalError::io("creating", dest, source))?;
    let entries = std::fs::read_dir(src).map_err(|source| EvalError::io("reading", src, source))?;
    for entry in entries {
        let from = entry
            .map_err(|source| EvalError::io("reading", src, source))?
            .path();
        if from.is_file() {
            let to = dest.join(from.file_name().unwrap_or_default());
            std::fs::copy(&from, &to).map_err(|source| EvalError::io("copying", &from, source))?;
        }
    }
    Ok(())
}

/// The project configuration written into each fixture copy.
#[derive(Serialize)]
struct ProjectConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    roles: Option<RolesOverride>,
    limits: LimitsOverride,
}

/// `[roles] coder = ...`.
#[derive(Serialize)]
struct RolesOverride {
    coder: String,
}

/// `[limits] session_tokens = ...`.
#[derive(Serialize)]
struct LimitsOverride {
    session_tokens: u64,
}

/// Writes `<work_dir>/.anvil/config.toml` with the provider and budget.
fn write_project_config(
    work_dir: &Path,
    provider: Option<&str>,
    max_tokens: u64,
) -> Result<(), EvalError> {
    let dir = work_dir.join(CONFIG_DIR);
    std::fs::create_dir_all(&dir).map_err(|source| EvalError::io("creating", &dir, source))?;
    let config = ProjectConfig {
        roles: provider.map(|coder| RolesOverride {
            coder: coder.to_owned(),
        }),
        limits: LimitsOverride {
            session_tokens: max_tokens,
        },
    };
    let text = toml::to_string(&config).map_err(|source| EvalError::InvalidTask {
        dir: work_dir.to_owned(),
        reason: format!("rendering the project config: {source}"),
    })?;
    let path = dir.join(CONFIG_FILE);
    std::fs::write(&path, text).map_err(|source| EvalError::io("writing", &path, source))
}

/// Maps anvil's exit code to a status, reading the reason off stderr.
fn status_from(code: Option<i32>, stderr: &str) -> AttemptStatus {
    match code {
        Some(0) => AttemptStatus::Done,
        Some(1) => AttemptStatus::SetupError,
        Some(2) => AttemptStatus::Incomplete {
            reason: incomplete_reason(stderr),
        },
        Some(3) => AttemptStatus::Denied,
        Some(4) => AttemptStatus::ProviderError,
        other => AttemptStatus::Crashed { code: other },
    }
}

/// The text after the last `incomplete: ` on stderr, or `unknown`.
fn incomplete_reason(stderr: &str) -> String {
    stderr
        .lines()
        .rev()
        .find_map(|line| line.trim().strip_prefix("incomplete: "))
        .map(|reason| reason.trim().to_owned())
        .unwrap_or_else(|| "unknown".to_owned())
}

/// The id from the `session <id>` line anvil prints first.
fn session_id(stderr: &str) -> Option<String> {
    stderr.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("session ")?;
        let id = rest.split_whitespace().next()?;
        (id.len() == 26 && id.chars().all(|c| c.is_ascii_alphanumeric())).then(|| id.to_owned())
    })
}

/// Whether the binary is a shell script to run through bash.
fn is_script(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext == "sh")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_map_to_statuses() {
        assert_eq!(status_from(Some(0), ""), AttemptStatus::Done);
        assert_eq!(status_from(Some(1), ""), AttemptStatus::SetupError);
        assert_eq!(
            status_from(Some(2), "== BUILD\nincomplete: turn_budget\nsummary at x"),
            AttemptStatus::Incomplete {
                reason: "turn_budget".to_owned()
            }
        );
        assert_eq!(
            status_from(Some(2), "nothing useful"),
            AttemptStatus::Incomplete {
                reason: "unknown".to_owned()
            }
        );
        assert_eq!(status_from(Some(3), ""), AttemptStatus::Denied);
        assert_eq!(status_from(Some(4), ""), AttemptStatus::ProviderError);
        assert_eq!(
            status_from(Some(9), ""),
            AttemptStatus::Crashed { code: Some(9) }
        );
        assert_eq!(status_from(None, ""), AttemptStatus::Crashed { code: None });
    }

    #[test]
    fn session_id_comes_from_the_first_session_line() {
        let stderr = "warning: x\nsession 01ARZ3NDEKTSV4RRFFQ69G5FAV\n== PLAN\n";
        assert_eq!(
            session_id(stderr).as_deref(),
            Some("01ARZ3NDEKTSV4RRFFQ69G5FAV")
        );
        assert_eq!(session_id("session short\n"), None);
        assert_eq!(session_id(""), None);
    }

    #[test]
    fn project_config_sets_coder_and_budget() {
        let dir = tempfile::tempdir().expect("tmp");
        write_project_config(dir.path(), Some("google"), 12345).expect("writes");
        let text = std::fs::read_to_string(dir.path().join(".anvil/config.toml")).expect("read");
        assert!(text.contains("coder = \"google\""), "{text}");
        assert!(text.contains("session_tokens = 12345"), "{text}");
        write_project_config(dir.path(), None, 1).expect("writes");
        let text = std::fs::read_to_string(dir.path().join(".anvil/config.toml")).expect("read");
        assert!(!text.contains("roles"), "{text}");
    }

    #[test]
    fn seed_home_copies_only_what_exists() {
        let real = tempfile::tempdir().expect("tmp");
        let home = tempfile::tempdir().expect("tmp");
        let anvil = real.path().join(CONFIG_DIR);
        std::fs::create_dir_all(anvil.join("checks")).expect("dir");
        std::fs::write(anvil.join("config.toml"), "[roles]\n").expect("write");
        std::fs::write(anvil.join("checks/docstrings.py"), "pass\n").expect("write");
        std::fs::create_dir_all(anvil.join("memory")).expect("dir");
        std::fs::write(anvil.join("memory/facts.md"), "secret\n").expect("write");
        seed_home(Some(real.path()), home.path()).expect("seeds");
        let seeded = home.path().join(CONFIG_DIR);
        assert!(seeded.join("config.toml").is_file());
        assert!(seeded.join("checks/docstrings.py").is_file());
        assert!(!seeded.join(".env").exists());
        assert!(!seeded.join("memory").exists());
        seed_home(None, home.path()).expect("seeds nothing");
    }

    #[test]
    fn default_anvil_bin_falls_back_to_target() {
        let repo = tempfile::tempdir().expect("tmp");
        if check::find_on_path("anvil").is_some() {
            return;
        }
        assert!(matches!(
            default_anvil_bin(repo.path()),
            Err(EvalError::AnvilNotFound)
        ));
        let debug = repo.path().join("target/debug");
        std::fs::create_dir_all(&debug).expect("dir");
        let file = if cfg!(windows) { "anvil.exe" } else { "anvil" };
        std::fs::write(debug.join(file), "").expect("write");
        assert_eq!(
            default_anvil_bin(repo.path()).expect("found"),
            debug.join(file)
        );
    }
}
