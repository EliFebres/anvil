//! `forge`: the CLI coding agent. Argument parsing and wiring only.

mod chat;
mod cli;
mod commands;
mod config_init;
mod doctor;
mod logging;
mod memory;
mod render;
mod rules;
mod secrets;
mod setup;
mod skills;
mod stats;
mod telemetry;
mod terminal;
mod ticket;
mod wiring;

use std::path::{Path, PathBuf};

use clap::Parser;
use forge_types::config::load as load_config;
use forge_types::dotenv;

use crate::cli::{Cli, Command, CompatCommand, ConfigCommand};
use crate::wiring::{Wiring, home_dir};

/// Exit code for configuration and setup errors.
const SETUP_FAILURE: i32 = 1;

/// Where the process runs and which `.env` files it loaded.
struct Env {
    /// The project root.
    cwd: PathBuf,
    /// The home directory, if known.
    home: Option<PathBuf>,
    /// Every `.env` file in loading order, present or not.
    env_files: Vec<PathBuf>,
    /// The `.env` files that existed and were loaded.
    loaded_env: Vec<PathBuf>,
}

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

/// Loads the `.env` files, then starts the runtime and runs the command.
///
/// The files are loaded before any thread exists: filling the environment
/// is only sound while the process is single-threaded, and the log filter
/// (`FORGE_LOG`) may come from a `.env` file.
fn start(cli: Cli) -> anyhow::Result<i32> {
    let cwd = std::env::current_dir()?;
    let home = home_dir();
    let env_files = dotenv::env_files(home.as_deref(), &cwd, false);
    let loaded_env = dotenv::load(&env_files)?;
    let _log_guard = logging::init(&cwd);
    let env = Env {
        cwd,
        home,
        env_files,
        loaded_env,
    };
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(dispatch(cli, env))
}

/// Runs the chosen command and returns its exit code.
async fn dispatch(cli: Cli, env: Env) -> anyhow::Result<i32> {
    let cwd = &env.cwd;
    let home = env.home.as_deref();
    match cli.command {
        Some(Command::Config {
            command: ConfigCommand::Init { global },
        }) => {
            let path = config_init::run(global)?;
            println!("wrote {}", path.display());
            Ok(0)
        }
        Some(Command::Config {
            command: ConfigCommand::Show,
        }) => {
            let loaded = load_config(home, cwd)?;
            commands::config_show(&loaded);
            Ok(0)
        }
        Some(Command::Setup) => setup::run(home, cwd),
        Some(Command::ScanKeys) => secrets::run(cwd, &env.env_files),
        Some(Command::Sessions) => {
            commands::sessions(cwd)?;
            Ok(0)
        }
        Some(Command::Skill { command }) => skills::run(command, home, cwd).await,
        Some(Command::Rules { command }) => rules::run(command, home, cwd),
        Some(Command::Stats { since }) => {
            let config = load_config(home, cwd).ok().map(|loaded| loaded.config);
            stats::run(config.as_ref(), cwd, &since)
        }
        Some(Command::Ticket { command }) => ticket::run(command, home, cwd).await,
        Some(Command::Compat {
            command: CompatCommand::Check { path },
        }) => {
            let config = load_config(home, cwd).ok().map(|loaded| loaded.config);
            commands::compat_check(&path, config.as_ref())
        }
        Some(Command::Doctor { fix_github }) => {
            let wiring = Wiring::new(load_config(home, cwd)?.config)?;
            let options = doctor::Options {
                env_files: &env.env_files,
                loaded_env: &env.loaded_env,
                fix_github,
            };
            doctor::run(&wiring, options).await
        }
        Some(Command::Chat) => {
            let mut wiring = Wiring::new(load_config(home, cwd)?.config)?;
            wiring.load_mcp().await;
            chat::chat(&wiring, cli.yes).await
        }
        Some(Command::Resume { id }) => {
            let mut wiring = Wiring::new(load_config(home, cwd)?.config)?;
            wiring.load_mcp().await;
            commands::resume(&wiring, &id, cli.yes).await
        }
        Some(Command::Run { task }) => run(home, cwd, task, cli.yes).await,
        None if !cli.task.is_empty() => run(home, cwd, cli.task, cli.yes).await,
        None => anyhow::bail!("nothing to do; pass a task or a subcommand (see --help)"),
    }
}

/// Runs a task given as words on the command line.
async fn run(
    home: Option<&Path>,
    cwd: &Path,
    words: Vec<String>,
    yes: bool,
) -> anyhow::Result<i32> {
    let task = words.join(" ");
    let mut wiring = Wiring::new(load_config(home, cwd)?.config)?;
    wiring.load_mcp().await;
    commands::run_task(&wiring, task, yes).await
}
