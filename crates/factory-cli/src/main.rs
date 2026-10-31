//! `factory`: the unattended improvement service. Argument parsing and wiring only.

use std::path::{Path, PathBuf};

use anvil_types::dotenv;
use clap::{Parser, Subcommand};

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
    /// Say whether an environment variable is set after the env files load, and where it came from.
    ///
    /// The value is never printed. Files load in this order, first definition
    /// wins: the real environment, ~/.factory/env, ~/.anvil/.env, .anvil/.env.
    Env {
        /// The variable name.
        name: String,
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
    std::env::var_os("FACTORY_HOME")
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
    let env_files = dotenv::env_files(home.as_deref(), &cwd, true);
    dotenv::load(&env_files)?;
    match cli.command {
        Command::Env { name } => {
            println!("{}", describe_variable(&name, &env_files)?);
            Ok(0)
        }
    }
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
