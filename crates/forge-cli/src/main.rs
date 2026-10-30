//! `forge`: the CLI coding agent. Argument parsing and wiring only.

mod chat;
mod cli;
mod commands;
mod config_init;
mod config_load;
mod logging;
mod render;
mod skills;
mod terminal;
mod wiring;

use clap::Parser;

use crate::cli::{Cli, Command, ConfigCommand};
use crate::wiring::Wiring;

/// Exit code for configuration and setup errors.
const SETUP_FAILURE: i32 = 1;

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let code = match dispatch(cli).await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error:#}");
            SETUP_FAILURE
        }
    };
    std::process::exit(code);
}

/// Runs the chosen command and returns its exit code.
async fn dispatch(cli: Cli) -> anyhow::Result<i32> {
    let cwd = std::env::current_dir()?;
    let _log_guard = logging::init(&cwd);
    let home = std::env::home_dir();
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
            let loaded = config_load::load(home.as_deref(), &cwd)?;
            commands::config_show(&loaded);
            Ok(0)
        }
        Some(Command::Sessions) => {
            commands::sessions(&cwd)?;
            Ok(0)
        }
        Some(Command::Skill { command }) => skills::run(command, home.as_deref(), &cwd).await,
        Some(Command::Doctor) => {
            let wiring = Wiring::new(config_load::load(home.as_deref(), &cwd)?.config)?;
            commands::doctor(&wiring).await
        }
        Some(Command::Chat) => {
            let wiring = Wiring::new(config_load::load(home.as_deref(), &cwd)?.config)?;
            chat::chat(&wiring).await
        }
        Some(Command::Resume { id }) => {
            let wiring = Wiring::new(config_load::load(home.as_deref(), &cwd)?.config)?;
            commands::resume(&wiring, &id, cli.yes).await
        }
        Some(Command::Run { task }) => run(home.as_deref(), &cwd, task, cli.yes).await,
        None if !cli.task.is_empty() => run(home.as_deref(), &cwd, cli.task, cli.yes).await,
        None => anyhow::bail!("nothing to do; pass a task or a subcommand (see --help)"),
    }
}

/// Runs a task given as words on the command line.
async fn run(
    home: Option<&std::path::Path>,
    cwd: &std::path::Path,
    words: Vec<String>,
    yes: bool,
) -> anyhow::Result<i32> {
    let task = words.join(" ");
    let wiring = Wiring::new(config_load::load(home, cwd)?.config)?;
    commands::run_task(&wiring, task, yes).await
}
