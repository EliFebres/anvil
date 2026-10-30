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
    /// Check the configuration, ping every provider and report loaded skills.
    Doctor,
    /// Install, list, inspect and remove skills (Claude Code `SKILL.md` folders).
    Skill {
        /// Which skill action to take.
        #[command(subcommand)]
        command: SkillCommand,
    },
    /// Claude Code compatibility checks.
    Compat {
        /// Which check to run.
        #[command(subcommand)]
        command: CompatCommand,
    },
}

/// Subcommands under `forge compat`.
#[derive(Debug, Subcommand)]
pub enum CompatCommand {
    /// Report what a `.claude` folder (or a plugin repository) would load in forge.
    Check {
        /// The folder to check, such as `~/.claude` or a project's `.claude`.
        path: std::path::PathBuf,
    },
}

/// Subcommands under `forge skill`.
#[derive(Debug, Subcommand)]
pub enum SkillCommand {
    /// Copy a skill folder into `~/.forge/skills/<name>` (or the project with --project).
    Add {
        /// A directory, a git URL, or a GitHub `owner/repo[/subdir]` shorthand.
        source: String,
        /// Install into the project's `.forge/skills` instead of the home directory.
        #[arg(long)]
        project: bool,
        /// Replace a skill of the same name that is already installed.
        #[arg(long)]
        force: bool,
    },
    /// List every skill forge would load from here, highest priority first.
    List,
    /// Delete an installed skill by name.
    Remove {
        /// The skill name.
        name: String,
        /// Remove from the project's `.forge/skills` instead of the home directory.
        #[arg(long)]
        project: bool,
    },
    /// Print a skill's metadata, supporting files and full instructions.
    Show {
        /// The skill name.
        name: String,
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
    /// Print the merged configuration (project over global) as TOML.
    Show,
}
