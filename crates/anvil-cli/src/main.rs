//! `anvil`: the CLI coding agent. Argument parsing and wiring only.

mod build;
mod chat;
mod cli;
mod commands;
mod config_init;
mod console;
mod doctor;
mod frontend;
mod learn;
mod legacy;
mod logging;
mod memory;
mod render;
mod rules;
mod secrets;
mod setup;
mod skills;
mod stats;
mod status;
mod telemetry;
mod templates;
mod terminal;
mod ticket;
mod tui;
mod update;
mod wiring;

use std::path::{Path, PathBuf};

use anvil_types::config::load_profile;
use anvil_types::dotenv;
use clap::Parser;

use crate::cli::{
    BuildCommand, Cli, Command, CompatCommand, ConfigCommand, MemoryCommand, TemplateCommand,
};
use crate::frontend::UiKind;
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

/// Moves old-name directories aside, loads the `.env` files, picks the
/// front end, then starts the runtime and runs the command.
///
/// The migration runs before anything creates `.anvil/`, so an old
/// directory is moved rather than reported as a duplicate. The files are loaded before any thread exists: filling the environment
/// is only sound while the process is single-threaded, and the log filter
/// (`ANVIL_LOG`) may come from a `.env` file. The front end is chosen
/// before logging starts, because the full-screen UI must not share
/// stderr with the console log.
fn start(cli: Cli) -> anyhow::Result<i32> {
    let cwd = std::env::current_dir()?;
    let home = home_dir();
    for (parent, prefix) in [(home.as_deref(), "~/"), (Some(cwd.as_path()), "")] {
        if let Some(parent) = parent
            && let Some(line) = legacy::migrate(parent, prefix)?
        {
            eprintln!("{line}");
        }
    }
    let env_files = dotenv::env_files(home.as_deref(), &cwd, false);
    let loaded_env = dotenv::load(&env_files)?;
    let interactive = matches!(
        cli.command,
        Some(Command::Chat | Command::Run { .. } | Command::Resume { .. })
    ) || (cli.command.is_none() && !cli.task.is_empty());
    let ui = UiKind::choose(cli.plain, cli.yes, interactive);
    let _log_guard = logging::init(&cwd, ui == UiKind::Plain);
    let env = Env {
        cwd,
        home,
        env_files,
        loaded_env,
    };
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(dispatch(cli, env, ui))
}

/// Runs the chosen command and returns its exit code.
async fn dispatch(cli: Cli, env: Env, ui: UiKind) -> anyhow::Result<i32> {
    let cwd = &env.cwd;
    let home = env.home.as_deref();
    let profile = cli.profile.as_deref();
    let load_config = |home: Option<&Path>, cwd: &Path| load_profile(home, cwd, profile);
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
        Some(Command::New { template, dir }) => templates::new_project(home, cwd, &template, &dir),
        Some(Command::Template {
            command:
                TemplateCommand::Save {
                    name,
                    dir,
                    description,
                    force,
                },
        }) => templates::save(
            home,
            cwd,
            &name,
            dir.as_deref(),
            description.as_deref(),
            force,
        ),
        Some(Command::Memory {
            command: MemoryCommand::Learn,
        }) => {
            let wiring = Wiring::new(load_config(home, cwd)?.config)?;
            learn::run(&wiring).await
        }
        Some(Command::Stats { since }) => {
            let config = load_config(home, cwd).ok().map(|loaded| loaded.config);
            stats::run(config.as_ref(), cwd, &since)
        }
        Some(Command::Status { json, all }) => {
            status::list::run(
                cwd,
                home,
                all,
                json,
                chrono::Utc::now(),
                &mut std::io::stdout(),
            )?;
            Ok(0)
        }
        Some(Command::Ticket { command }) => ticket::run(command, home, cwd).await,
        Some(Command::Update { exe }) => {
            let repo = std::fs::read_to_string(cwd.join(anvil_types::factory_config::FACTORY_FILE))
                .ok()
                .and_then(|text| toml::from_str::<anvil_types::FactoryConfig>(&text).ok())
                .map(|factory| factory.github.repo);
            let options = update::UpdateOptions::for_this_binary(repo.as_deref(), exe)?;
            update::run(options).await
        }
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
        Some(Command::Build(args)) => {
            if let Some(BuildCommand::Status { id }) = &args.command {
                build::status(cwd, id.as_deref())?;
                return Ok(0);
            }
            let mut wiring = Wiring::new(load_config(home, cwd)?.config)?;
            wiring.allow_tools(&cli.allow_tools)?;
            wiring.load_mcp().await;
            build::execute(args, wiring, cli.yes).await
        }
        Some(Command::Chat) => {
            let mut wiring = Wiring::new(load_config(home, cwd)?.config)?;
            wiring.allow_tools(&cli.allow_tools)?;
            wiring.load_mcp().await;
            chat::chat(&wiring, cli.yes, ui).await
        }
        Some(Command::Resume { id }) => {
            let mut wiring = Wiring::new(load_config(home, cwd)?.config)?;
            wiring.allow_tools(&cli.allow_tools)?;
            wiring.load_mcp().await;
            commands::resume(&wiring, &id, cli.yes, ui).await
        }
        Some(Command::Run { task }) => {
            run(home, cwd, profile, task, &cli.allow_tools, cli.yes, ui).await
        }
        None if !cli.task.is_empty() => {
            run(home, cwd, profile, cli.task, &cli.allow_tools, cli.yes, ui).await
        }
        None => anyhow::bail!("nothing to do; pass a task or a subcommand (see --help)"),
    }
}

/// Runs a task given as words on the command line.
async fn run(
    home: Option<&Path>,
    cwd: &Path,
    profile: Option<&str>,
    words: Vec<String>,
    allow_tools: &[String],
    yes: bool,
    ui: UiKind,
) -> anyhow::Result<i32> {
    let task = words.join(" ");
    let mut wiring = Wiring::new(load_profile(home, cwd, profile)?.config)?;
    wiring.allow_tools(allow_tools)?;
    wiring.load_mcp().await;
    commands::run_task(&wiring, task, yes, ui).await
}
