//! Command-line definition for `forge`.

use clap::{Parser, Subcommand};

/// A CLI coding agent.
#[derive(Debug, Parser)]
#[command(name = "forge", version, about, arg_required_else_help = true)]
pub struct Cli {
    /// Skip plan approval and run unattended.
    #[arg(long, global = true)]
    pub yes: bool,

    /// What to do.
    #[command(subcommand)]
    pub command: Option<Command>,

    /// The task to work on, when no subcommand is given.
    #[arg(trailing_var_arg = true)]
    pub task: Vec<String>,
}

/// Top-level subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Work on a task: plan, build, review, optimize, verify.
    Run {
        /// The task.
        #[arg(required = true, trailing_var_arg = true)]
        task: Vec<String>,
    },
    /// Talk to the model with tools, without the state machine.
    Chat,
    /// Continue a saved session from where it stopped.
    Resume {
        /// The session id, from `forge sessions`.
        id: String,
    },
    /// List saved sessions, newest first.
    Sessions,
    /// Manage forge configuration.
    Config {
        /// Which configuration action to take.
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Check the configuration and ping every provider.
    Doctor,
}

/// Subcommands under `forge config`.
#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Write a template config file. Refuses to overwrite an existing one.
    Init {
        /// Write the global `~/.forge/config.toml` instead of the project `.forge/config.toml`.
        #[arg(long)]
        global: bool,
    },
    /// Print the merged configuration (project over global) as TOML.
    Show,
}
