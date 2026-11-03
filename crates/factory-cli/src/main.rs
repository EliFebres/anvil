//! `factory`: the unattended improvement service. Argument parsing and wiring only.

mod review;

use std::path::{Path, PathBuf};
use std::time::Duration;

use anvil_types::factory_config::FACTORY_FILE;
use anvil_types::{ContainerRuntime, dotenv};
use anyhow::Context;
use clap::{Parser, Subcommand};
use factory_cli::install::{InstallOptions, config_home, install};
use factory_cli::selftest::{SelftestOptions, selftest};
use factory_cli::update::{UpdateOptions, update};
use factory_cli::watch::{WatchOptions, watch};
use factory_cli::{logging, wiring};
use factory_core::{FactoryHome, ServiceState, SystemRunner, Tick, preflight, service};

/// The Dark Factory: works tickets from `tickets/open/` into reviewed pull requests.
#[derive(Debug, Parser)]
#[command(name = "factory", version, about, arg_required_else_help = true)]
struct Cli {
    /// What to do.
    #[command(subcommand)]
    command: Command,
}

/// Top-level subcommands.
#[derive(Debug, Subcommand)]
enum Command {
    /// Run the service: poll factory/state every few minutes and work tickets.
    ///
    /// Secrets come from ~/.factory/env (mode 600). Logs go to stderr (journald)
    /// and ~/.factory/logs/factory.jsonl. Needs FACTORY_GITHUB_TOKEN.
    Run {
        /// Do one pass and exit instead of looping.
        #[arg(long)]
        once: bool,
    },
    /// Print the queue, the ticket in progress and the last 10 outcomes.
    Status,
    /// Show a full-screen dashboard of what the service is doing, refreshed every 2s.
    ///
    /// Reads ~/.factory/status.json, which the service writes, so it also
    /// works over ssh. `p` pauses after the current ticket, `r` resumes, `q`
    /// quits.
    Watch {
        /// Print the screen once as plain text and exit.
        #[arg(long)]
        once: bool,
        /// Show this status file instead of the service's; the pause keys then
        /// only change the screen.
        #[arg(long, value_name = "PATH")]
        file: Option<PathBuf>,
    },
    /// Stop taking tickets after the current one; `factory resume` undoes it.
    Pause {
        /// Why, shown by `factory status`.
        #[arg(default_value = "paused by hand")]
        reason: String,
    },
    /// Clear the pause flag.
    Resume,
    /// Install deploy/nuc/factory.service as a systemd user unit and start it (Linux only).
    InstallService,
    /// Stop the service, pull main, rebuild the binaries (and the worker image when its recipe changed), and start it again (Linux only).
    ///
    /// Builds anvil twice: for the host, and as a static musl binary that the
    /// worker container mounts (checked with ldd). A ticket in progress
    /// finishes first: the pause flag stops the loop from claiming another
    /// one and the update waits until the worker is idle. Runs in the
    /// repository that holds the current folder, else ~/factory-repo.
    Update {
        /// Stop at once; a ticket in progress stays claimed on factory/state.
        #[arg(long)]
        now: bool,
        /// Rebuild the worker image even when its recipe (rust-toolchain.toml,
        /// deploy/nuc/Containerfile, entrypoint.sh) has not changed.
        #[arg(long)]
        rebuild_image: bool,
    },
    /// Run the whole worker pipeline once on this machine, against a local bare repository.
    ///
    /// Uses the real worker image, the resolved [worker] anvil_bin, the egress
    /// proxy (empty allowlist), worktrees, Gate A, Gate C and the merge, with
    /// a scripted provider instead of models and a bare repository instead of
    /// GitHub. Prints PASS or FAIL per step; exits 1 at the first failure.
    /// `factory update` runs it before starting the service again.
    Selftest {
        /// Gate A's command line.
        #[arg(long, default_value = "true")]
        check: String,
        /// Keep the temp folder for inspection.
        #[arg(long)]
        keep: bool,
        /// Run anvil and Gate A on the host instead of in the container (tests only).
        #[arg(long, hide = true)]
        no_container: bool,
    },
    /// Say whether an environment variable is set after the env files load, and where it came from.
    ///
    /// The value is never printed. Files load in this order, first definition
    /// wins: the real environment, ~/.factory/env, ~/.anvil/.env, .anvil/.env.
    Env {
        /// The variable name.
        name: String,
    },
    /// Run the review swarm on a pull request and post its minor findings as comments.
    ///
    /// Prints the aggregate verdict with every reviewer's raw result. Exits 0
    /// when the swarm approves and 2 when it asks for changes; blocker and
    /// major findings are printed, never posted. Needs GITHUB_TOKEN.
    Review {
        /// The pull request number in the repository named by factory.toml.
        number: u64,
    },
}

/// Exit code for configuration and setup errors.
const SETUP_FAILURE: i32 = 1;

fn main() {
    let cli = Cli::parse();
    let code = match start(cli) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error:#}");
            SETUP_FAILURE
        }
    };
    std::process::exit(code);
}

/// The home directory: `FACTORY_HOME` when set, otherwise the OS home.
///
/// The override exists so tests can point `~/.factory` at a temp folder.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os(factory_core::paths::HOME_ENV)
        .map(PathBuf::from)
        .or_else(std::env::home_dir)
}

/// Loads the env files, then runs the command.
///
/// The files are loaded before any thread exists: filling the environment is
/// only sound while the process is single-threaded.
fn start(cli: Cli) -> anyhow::Result<i32> {
    let cwd = std::env::current_dir()?;
    let home = home_dir();
    let factory_home = home.as_deref().map(FactoryHome::under);
    let env_files = dotenv::env_files(home.as_deref(), &cwd, true);
    if matches!(cli.command, Command::Run { .. })
        && let Some(factory_home) = &factory_home
    {
        check_env_mode(&factory_home.env_file())?;
    }
    dotenv::load(&env_files)?;
    // The selftest keeps its log in its own temp home, away from the service's.
    let _log_guard = factory_home
        .as_ref()
        .filter(|_| !matches!(cli.command, Command::Selftest { .. }))
        .and_then(|factory_home| logging::init(&factory_home.logs_dir()));
    match cli.command {
        Command::Env { name } => {
            println!("{}", describe_variable(&name, &env_files)?);
            Ok(0)
        }
        Command::Selftest {
            check,
            keep,
            no_container,
        } => {
            let runtime = tokio::runtime::Runtime::new().context("starting the async runtime")?;
            let repo_root = runtime.block_on(wiring::repo_root(&cwd));
            let options = SelftestOptions {
                repo_root,
                home,
                check,
                keep,
                no_container,
            };
            runtime.block_on(selftest(&options))?;
            Ok(0)
        }
        Command::Review { number } => {
            let runtime = tokio::runtime::Runtime::new().context("starting the async runtime")?;
            runtime.block_on(review::run(review::Options { number, cwd, home }))
        }
        Command::Run { once } => {
            let factory_home = factory_home.context("no home directory; set FACTORY_HOME")?;
            let runtime = tokio::runtime::Runtime::new().context("starting the async runtime")?;
            runtime.block_on(run(&cwd, home.as_deref(), factory_home, once))
        }
        Command::Status => {
            let factory_home = factory_home.context("no home directory; set FACTORY_HOME")?;
            let runtime = tokio::runtime::Runtime::new().context("starting the async runtime")?;
            runtime.block_on(status(&cwd, factory_home))
        }
        Command::Pause { reason } => {
            let factory_home = factory_home.context("no home directory; set FACTORY_HOME")?;
            service::pause(&factory_home, &reason)?;
            println!("paused: {reason}");
            Ok(0)
        }
        Command::Watch { once, file } => {
            let home = factory_home.context("no home directory; set FACTORY_HOME")?;
            watch(&WatchOptions { home, file, once })?;
            Ok(0)
        }
        Command::Resume => {
            let factory_home = factory_home.context("no home directory; set FACTORY_HOME")?;
            service::resume(&factory_home)?;
            println!("resumed");
            Ok(0)
        }
        Command::InstallService => {
            if !cfg!(target_os = "linux") {
                anyhow::bail!("install-service needs systemd; run it on the NUC");
            }
            let home = home.context("no home directory")?;
            let lines = install(&InstallOptions {
                repo_root: cwd,
                config_home: config_home(&home),
                run_systemctl: true,
            })?;
            for line in lines {
                println!("{line}");
            }
            Ok(0)
        }
        Command::Update { now, rebuild_image } => {
            if !cfg!(target_os = "linux") {
                anyhow::bail!("update needs systemd; run it on the NUC");
            }
            let factory_home = factory_home.context("no home directory; set FACTORY_HOME")?;
            let home = home.context("no home directory")?;
            let runtime = tokio::runtime::Runtime::new().context("starting the async runtime")?;
            let repo_root = runtime.block_on(update_root(&cwd, &home));
            println!("== repository: {}", repo_root.display());
            update(&UpdateOptions {
                repo_root,
                home: factory_home,
                config_home: config_home(&home),
                now,
                poll: UPDATE_POLL,
                rebuild_image,
            })?;
            Ok(0)
        }
    }
}

/// How often `factory update` re-reads the state file while a ticket finishes.
const UPDATE_POLL: Duration = Duration::from_secs(30);

/// The repository `factory update` works in: the one around `cwd` when it
/// holds a `factory.toml`, otherwise `~/factory-repo`, where bootstrap put it.
async fn update_root(cwd: &Path, home: &Path) -> PathBuf {
    let root = wiring::repo_root(cwd).await;
    if root.join(FACTORY_FILE).is_file() {
        root
    } else {
        home.join("factory-repo")
    }
}

/// `factory run`: the loop, or one pass with `--once`.
async fn run(
    cwd: &Path,
    home: Option<&Path>,
    factory_home: FactoryHome,
    once: bool,
) -> anyhow::Result<i32> {
    let loaded = wiring::load(cwd, home).await?;
    let service = wiring::build_service(&loaded, factory_home).await?;
    if once {
        let tick = service.run_once().await?;
        println!("{}", describe_tick(&tick));
        return Ok(0);
    }
    tracing::info!(
        poll_minutes = loaded.factory.service.poll_minutes,
        repo = loaded.factory.github.repo,
        "factory running"
    );
    service.run_forever().await?;
    Ok(0)
}

/// `factory status`.
async fn status(cwd: &Path, factory_home: FactoryHome) -> anyhow::Result<i32> {
    let root = wiring::repo_root(cwd).await;
    let state = ServiceState::load(&factory_home.state_file())?;
    let paused = service::pause_reason(&factory_home);
    let queue = factory_core::TicketQueue::open(
        &wiring::repo(&root),
        &factory_home.state_dir(),
        wiring::REMOTE,
    )
    .await?;
    let open = queue.open_tickets()?;
    print!("{}", state.render(&open.tickets, paused.as_deref()));
    for (path, problem) in open.unusable {
        println!("unusable: {}: {problem}", path.display());
    }
    println!("\n{}", sandbox_line(&root).await);
    Ok(0)
}

/// The `sandbox:` line of `factory status`: whether the runtime could run a
/// ticket right now. A problem is reported, never fatal.
async fn sandbox_line(root: &Path) -> String {
    let factory = match wiring::read_factory(root) {
        Ok(factory) => factory,
        Err(error) => return format!("sandbox: {error:#}"),
    };
    let worker = &factory.worker;
    let checked = preflight(
        &SystemRunner,
        worker.runtime,
        &worker.image,
        &worker.anvil_bin,
        root,
    )
    .await;
    describe_sandbox(worker.runtime, &worker.image, checked)
}

/// One line for a preflight result: the runtime, the image and the resolved
/// anvil binary, or the problem.
fn describe_sandbox(
    runtime: ContainerRuntime,
    image: &str,
    checked: factory_core::Result<PathBuf>,
) -> String {
    match checked {
        Ok(anvil) => match runtime {
            ContainerRuntime::Podman => {
                format!("sandbox: ready (podman {image}; anvil {})", anvil.display())
            }
            ContainerRuntime::Docker => {
                format!("sandbox: ready (docker {image}; anvil {})", anvil.display())
            }
            ContainerRuntime::None => format!(
                "sandbox: the host (runtime = \"none\"; anvil {})",
                anvil.display()
            ),
        },
        Err(error) => {
            // The error already says `sandbox:`; keep the line's prefix single.
            let detail = error.to_string();
            let detail = detail.strip_prefix("sandbox: ").unwrap_or(&detail);
            format!("sandbox: {detail}")
        }
    }
}

/// One line for `factory run --once`.
fn describe_tick(tick: &Tick) -> String {
    match tick {
        Tick::Paused(reason) => format!("paused: {reason}"),
        Tick::Idle => "idle: no open tickets".to_owned(),
        Tick::Worked(id) => format!("worked {id}"),
    }
}

/// Refuses to run when the secrets file is readable by others (unix only).
fn check_env_mode(path: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = std::fs::metadata(path) {
            let mode = metadata.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                anyhow::bail!(
                    "{} has mode {mode:o}; it holds secrets and must be 600 (run `chmod 600 {}`)",
                    path.display(),
                    path.display()
                );
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

/// Where a variable's value came from, without the value.
///
/// # Arguments
///
/// * `name` - The variable.
/// * `env_files` - The files in loading order.
///
/// # Returns
///
/// `NAME: unset`, `NAME: set (from <file>)` or `NAME: set (from the environment)`.
/// The source is the first file whose value for `name` is the one in the
/// environment; when no file matches, the real environment supplied it.
///
/// # Errors
///
/// When an env file exists but is malformed.
fn describe_variable(name: &str, env_files: &[PathBuf]) -> anyhow::Result<String> {
    let Ok(current) = std::env::var(name) else {
        return Ok(format!("{name}: unset"));
    };
    let source = file_supplying(name, &current, env_files)?
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "the environment".to_owned());
    Ok(format!("{name}: set (from {source})"))
}

/// The first env file that defines `name` with exactly `value`, if any.
fn file_supplying<'a>(
    name: &str,
    value: &str,
    env_files: &'a [PathBuf],
) -> anyhow::Result<Option<&'a Path>> {
    for path in env_files {
        let supplies = dotenv::read_values(path)?
            .iter()
            .any(|(key, file_value)| key == name && file_value == value);
        if supplies {
            return Ok(Some(path.as_path()));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticks_read_as_one_line() {
        assert_eq!(describe_tick(&Tick::Idle), "idle: no open tickets");
        assert_eq!(describe_tick(&Tick::Paused("x".to_owned())), "paused: x");
    }

    #[test]
    fn the_sandbox_line_names_the_runtime_or_the_problem() {
        let image = "localhost/anvil-factory:latest";
        let anvil =
            PathBuf::from("/home/eli/factory-repo/target/x86_64-unknown-linux-musl/release/anvil");
        assert_eq!(
            describe_sandbox(ContainerRuntime::Podman, image, Ok(anvil.clone())),
            "sandbox: ready (podman localhost/anvil-factory:latest; anvil /home/eli/factory-repo/target/x86_64-unknown-linux-musl/release/anvil)"
        );
        assert_eq!(
            describe_sandbox(ContainerRuntime::None, image, Ok(anvil)),
            "sandbox: the host (runtime = \"none\"; anvil /home/eli/factory-repo/target/x86_64-unknown-linux-musl/release/anvil)"
        );
        let missing = factory_core::FactoryError::Sandbox("worker image x is not built".to_owned());
        assert_eq!(
            describe_sandbox(ContainerRuntime::Podman, image, Err(missing)),
            "sandbox: worker image x is not built"
        );
    }
}
