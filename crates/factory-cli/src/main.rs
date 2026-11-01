//! `factory`: the unattended improvement service. Argument parsing and wiring only.

mod review;

use std::path::{Path, PathBuf};

use anvil_types::dotenv;
use anyhow::Context;
use clap::{Parser, Subcommand};
use factory_cli::install::{InstallOptions, config_home, install};
use factory_cli::{logging, wiring};
use factory_core::{FactoryHome, ServiceState, Tick, service};

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
    let _log_guard = factory_home
        .as_ref()
        .and_then(|factory_home| logging::init(&factory_home.logs_dir()));
    match cli.command {
        Command::Env { name } => {
            println!("{}", describe_variable(&name, &env_files)?);
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
        &factory_core::Git::new(&root),
        &factory_home.state_dir(),
        wiring::REMOTE,
    )
    .await?;
    let open = queue.open_tickets()?;
    print!("{}", state.render(&open.tickets, paused.as_deref()));
    for (path, problem) in open.unusable {
        println!("unusable: {}: {problem}", path.display());
    }
    Ok(0)
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
}
