//! Command-line definition for `forge`.

use clap::{Parser, Subcommand};

/// A CLI coding agent.
#[derive(Debug, Parser)]
#[command(name = "forge", version, about, arg_required_else_help = true)]
pub struct Cli {
    /// What to do.
    #[command(subcommand)]
    pub command: Command,
}

/// Top-level subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Manage forge configuration.
    Config {
        /// Which configuration action to take.
        #[command(subcommand)]
        command: ConfigCommand,
    },
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
}
