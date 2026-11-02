//! `factory update`: stop the service, pull `main`, rebuild, refresh the
//! worker image and the unit, and start the service again.
//!
//! This is what `deploy/nuc/bootstrap.sh` does after its first run, as one
//! command to type on the NUC. A ticket in progress is allowed to finish
//! first: the pause flag keeps the loop from claiming another one, and the
//! state file says when the worker is idle.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anvil_types::{ContainerRuntime, FactoryConfig};
use anyhow::Context;
use factory_core::{FactoryHome, InProgress, ServiceState, service};

use crate::install::{InstallOptions, UNIT_NAME, install};

/// The pause reason the update sets while it waits.
pub const PAUSE_REASON: &str = "factory update";

/// Where the worker image is built from.
const IMAGE_DIR: &str = "deploy/nuc";

/// The image recipe.
const CONTAINERFILE: &str = "deploy/nuc/Containerfile";

/// What the update needs.
pub struct UpdateOptions {
    /// The clone of the monorepo, `~/factory-repo` on the NUC.
    pub repo_root: PathBuf,
    /// `~/.factory`.
    pub home: FactoryHome,
    /// `$XDG_CONFIG_HOME`, normally `~/.config`.
    pub config_home: PathBuf,
    /// Stop at once instead of waiting for the ticket in progress.
    pub now: bool,
    /// How often the state file is read while waiting.
    pub poll: Duration,
}

/// One external command of the rebuild, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// What it does, for the console.
    pub label: String,
    /// The program.
    pub program: String,
    /// Its arguments.
    pub args: Vec<String>,
}

/// Runs the whole update, printing each stage as it goes.
///
/// # Arguments
///
/// * `options` - Where the repository and the home are, and how to stop.
///
/// # Returns
///
/// Nothing; the console shows every stage and the child processes' output.
///
/// # Errors
///
/// When `systemctl`, `git`, `cargo` or the container runtime fails, or
/// `factory.toml` cannot be read after the pull. The pause flag the update
/// set is cleared on every path once the service was stopped.
pub fn update(options: &UpdateOptions) -> anyhow::Result<()> {
    let root = &options.repo_root;
    let running = systemctl_ok(&["--user", "is-active", "--quiet", UNIT_NAME]);
    if running {
        // A pause set by hand or by the service itself stays as it is; only a
        // flag this update sets is cleared again.
        let was_paused = service::pause_reason(&options.home);
        if was_paused.is_none() {
            service::pause(&options.home, PAUSE_REASON).context("setting the pause flag")?;
        }
        let stranded = wait_for_idle(&options.home, options.poll, options.now, &mut |line| {
            println!("{line}")
        })?;
        let stopped = run(
            &Step::new("service: stop", "systemctl", &["--user", "stop", UNIT_NAME]),
            root,
        );
        if was_paused.is_none() {
            service::resume(&options.home).context("clearing the pause flag")?;
        }
        stopped?;
        if let Some(ticket) = stranded {
            println!(
                "warning: ticket {} ({}) was at `{}` and stays in tickets/claimed/ on factory/state; to retry it, move the file to tickets/open/, set status = \"open\" and drop claimed_at and claimed_by",
                ticket.id, ticket.title, ticket.stage
            );
        }
    } else {
        println!("== service: not running");
    }

    let before = git_head(root);
    run(
        &Step::new("repo: pull", "git", &["pull", "--ff-only", "--quiet"]),
        root,
    )?;
    let after = git_head(root);
    if before == after {
        println!("repo: already at {after}");
    } else {
        println!("repo: {before} -> {after}");
    }

    let factory = crate::wiring::read_factory(root)?;
    for step in rebuild_steps(&factory) {
        run(&step, root)?;
    }

    println!("== service: install and start");
    for line in install(&InstallOptions {
        repo_root: root.clone(),
        config_home: options.config_home.clone(),
        run_systemctl: true,
    })? {
        println!("{line}");
    }
    Ok(())
}

/// The build commands, in order: the binaries, then the worker image unless
/// the worker runs on the host.
///
/// # Arguments
///
/// * `factory` - `factory.toml`, which names the runtime and the image tag.
///
/// # Returns
///
/// The steps; `cargo build` first.
pub fn rebuild_steps(factory: &FactoryConfig) -> Vec<Step> {
    let mut steps = vec![Step::new(
        "build: anvil and factory (release)",
        "cargo",
        &["build", "--release", "-p", "anvil-cli", "-p", "factory-cli"],
    )];
    let runtime = match factory.worker.runtime {
        ContainerRuntime::Podman => "podman",
        ContainerRuntime::Docker => "docker",
        ContainerRuntime::None => return steps,
    };
    steps.push(Step::new(
        &format!("image: {}", factory.worker.image),
        runtime,
        &[
            "build",
            "-q",
            "-t",
            &factory.worker.image,
            "-f",
            CONTAINERFILE,
            IMAGE_DIR,
        ],
    ));
    steps
}

/// Waits until the state file shows no ticket in progress.
///
/// # Arguments
///
/// * `home` - `~/.factory`, which holds the state file.
/// * `poll` - How long to sleep between reads.
/// * `now` - Do not wait; report the ticket in progress instead.
/// * `log` - Receives one line per wait.
///
/// # Returns
///
/// The ticket that is still in progress when `now` cut the wait short,
/// otherwise `None`.
///
/// # Errors
///
/// When the state file exists but cannot be parsed.
pub fn wait_for_idle(
    home: &FactoryHome,
    poll: Duration,
    now: bool,
    log: &mut dyn FnMut(String),
) -> anyhow::Result<Option<InProgress>> {
    loop {
        let state = ServiceState::load(&home.state_file()).context("reading the state file")?;
        let Some(current) = state.in_progress else {
            return Ok(None);
        };
        if now {
            return Ok(Some(current));
        }
        log(format!(
            "waiting for ticket {} ({}) at `{}`, claimed {}; `factory update --now` stops at once",
            current.id,
            current.title,
            current.stage,
            current.since.format("%Y-%m-%d %H:%M UTC")
        ));
        std::thread::sleep(poll);
    }
}

impl Step {
    /// A step from borrowed parts.
    fn new(label: &str, program: &str, args: &[&str]) -> Self {
        Self {
            label: label.to_owned(),
            program: program.to_owned(),
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
        }
    }
}

/// Runs one step in `dir` with the console attached, so build output shows.
fn run(step: &Step, dir: &Path) -> anyhow::Result<()> {
    println!("== {}", step.label);
    let status = Command::new(&step.program)
        .args(&step.args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .status()
        .with_context(|| format!("running {}", step.program))?;
    if !status.success() {
        anyhow::bail!("{} {} failed ({status})", step.program, step.args.join(" "));
    }
    Ok(())
}

/// Whether a `systemctl` query exits 0; `false` when systemctl is missing.
fn systemctl_ok(args: &[&str]) -> bool {
    Command::new("systemctl")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// The short sha of `HEAD`, or `?` when git cannot say.
fn git_head(dir: &Path) -> String {
    Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(dir)
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .unwrap_or_else(|| "?".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(runtime: ContainerRuntime) -> FactoryConfig {
        let mut factory: FactoryConfig =
            toml::from_str(&FactoryConfig::template("owner/repo")).expect("template");
        factory.worker.runtime = runtime;
        factory.worker.image = "localhost/test:latest".to_owned();
        factory
    }

    #[test]
    fn the_rebuild_builds_the_binaries_then_the_image_for_the_configured_runtime() {
        let steps = rebuild_steps(&config(ContainerRuntime::Podman));
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].program, "cargo");
        assert_eq!(steps[0].args[..3], ["build", "--release", "-p"]);
        assert_eq!(steps[1].program, "podman");
        assert_eq!(
            steps[1].args,
            [
                "build",
                "-q",
                "-t",
                "localhost/test:latest",
                "-f",
                CONTAINERFILE,
                IMAGE_DIR
            ]
        );
        assert_eq!(
            rebuild_steps(&config(ContainerRuntime::Docker))[1].program,
            "docker"
        );
        assert_eq!(rebuild_steps(&config(ContainerRuntime::None)).len(), 1);
    }

    fn in_progress(stage: &str) -> ServiceState {
        ServiceState {
            in_progress: Some(InProgress {
                id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".to_owned(),
                title: "Add a hello skill".to_owned(),
                stage: stage.to_owned(),
                since: "2026-10-30T10:00:00Z".parse().expect("rfc3339"),
            }),
            ..ServiceState::default()
        }
    }

    #[test]
    fn waits_until_the_state_file_shows_nothing_in_progress() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = FactoryHome::under(dir.path());
        std::fs::create_dir_all(home.root()).expect("mkdir");
        let path = home.state_file();
        in_progress("gate B").save(&path).expect("save");
        let clearer = {
            let path = path.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(150));
                ServiceState::default().save(&path).expect("save");
            })
        };
        let mut lines = Vec::new();
        let stranded = wait_for_idle(&home, Duration::from_millis(20), false, &mut |line| {
            lines.push(line)
        })
        .expect("waits");
        clearer.join().expect("thread");
        assert!(stranded.is_none());
        assert!(!lines.is_empty());
        assert!(
            lines[0].contains("01ARZ3NDEKTSV4RRFFQ69G5FAV"),
            "{}",
            lines[0]
        );
        assert!(lines[0].contains("gate B"), "{}", lines[0]);
        assert!(lines[0].contains("--now"), "{}", lines[0]);
    }

    #[test]
    fn now_reports_the_ticket_in_progress_without_waiting() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = FactoryHome::under(dir.path());
        std::fs::create_dir_all(home.root()).expect("mkdir");
        in_progress("implementer attempt 1")
            .save(&home.state_file())
            .expect("save");
        let mut lines = Vec::new();
        let stranded = wait_for_idle(&home, Duration::from_secs(60), true, &mut |line| {
            lines.push(line)
        })
        .expect("returns")
        .expect("the ticket");
        assert_eq!(stranded.stage, "implementer attempt 1");
        assert!(lines.is_empty());
    }

    #[test]
    fn no_state_file_means_idle() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = FactoryHome::under(dir.path());
        let stranded = wait_for_idle(&home, Duration::from_secs(60), false, &mut |_| {})
            .expect("missing file is idle");
        assert!(stranded.is_none());
    }
}
