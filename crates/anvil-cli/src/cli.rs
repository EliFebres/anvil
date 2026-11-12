//! Command-line definition for `anvil`.

use clap::{ArgAction, Args, Parser, Subcommand};

/// A CLI coding agent.
#[derive(Debug, Parser)]
#[command(name = "anvil", version, about, arg_required_else_help = true)]
pub struct Cli {
    /// Skip plan approval and every permission prompt; run unattended.
    #[arg(long, global = true)]
    pub yes: bool,

    /// A named profile, `~/.anvil/profiles/<name>.toml`, layered over the global and
    /// project configuration; its `[permissions]` table replaces `.anvil/permissions.toml`.
    #[arg(long, global = true, value_name = "NAME")]
    pub profile: Option<String>,

    /// Use the line renderer and stdin prompts instead of the full-screen UI.
    /// Chosen automatically with --yes or when a standard stream is not a terminal.
    #[arg(long, global = true)]
    pub plain: bool,

    /// Pre-approve a tool for this session, so it never prompts; repeat for
    /// more tools. Adds to the config's `permissions.allow_tools`.
    #[arg(long = "allow-tool", global = true, value_name = "NAME", action = ArgAction::Append)]
    pub allow_tools: Vec<String>,

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
    /// Work on a task: plan, build, verify, review.
    Run {
        /// The task.
        #[arg(required = true, trailing_var_arg = true)]
        task: Vec<String>,
    },
    /// Work through a specification across many sessions until its acceptance checks pass.
    Build(BuildArgs),
    /// Talk to the model with tools, without the state machine.
    Chat,
    /// Continue a saved session from where it stopped.
    Resume {
        /// The session id, from `anvil sessions`.
        id: String,
    },
    /// List saved sessions, newest first.
    Sessions,
    /// Manage anvil configuration.
    Config {
        /// Which configuration action to take.
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Write ~/.anvil/.env, ~/.anvil/config.toml, factory.toml and deploy/nuc/env.template
    /// without overwriting anything already filled in; exit 2 while placeholders remain.
    Setup,
    /// Check keys, providers, factory.toml, GitHub access, branch protection, tools and
    /// tracked files, one row per check with the fix for each failure.
    Doctor {
        /// Set the `main` branch protection and squash merging through an authenticated `gh`.
        #[arg(long)]
        fix_github: bool,
    },
    /// Fail if any tracked file contains an API key (`just check` runs this).
    ScanKeys,
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
    /// Manage the standing coding rules in `~/.anvil/rules.toml`.
    Rules {
        /// Which rules action to take.
        #[command(subcommand)]
        command: RulesCommand,
    },
    /// Create a project in an empty directory from a template saved with `anvil template save`.
    New {
        /// The template name, from `~/.anvil/templates`.
        template: String,
        /// The project directory; created when missing, otherwise it must be empty.
        dir: std::path::PathBuf,
    },
    /// Save directories as project templates under `~/.anvil/templates`.
    Template {
        /// Which template action to take.
        #[command(subcommand)]
        command: TemplateCommand,
    },
    /// Work with what anvil remembers about you.
    Memory {
        /// Which memory action to take.
        #[command(subcommand)]
        command: MemoryCommand,
    },
    /// Summarize this project's telemetry: sessions, completion rate, tokens and cost.
    Stats {
        /// How far back to look: a number followed by m, h, d or w.
        #[arg(long, default_value = "7d")]
        since: String,
    },
    /// List this project's sessions, newest first: state, loop state, age, spend, task, id.
    /// A session that says `working` but whose process is gone shows as `stale`.
    Status {
        /// Print a JSON array instead of lines.
        #[arg(long)]
        json: bool,
        /// Include every project that has run a session.
        #[arg(long)]
        all: bool,
    },
    /// Download the latest release for this platform, verify its SHA256, swap the binary
    /// and print the changes since the installed version.
    Update {
        /// The binary to replace instead of this executable (for tests).
        #[arg(long, hide = true)]
        exe: Option<std::path::PathBuf>,
    },
    /// Draft, review and submit improvement tickets from this project's telemetry.
    Ticket {
        /// Which ticket action to take.
        #[command(subcommand)]
        command: TicketCommand,
    },
}

/// Arguments of `anvil build`.
#[derive(Debug, Args)]
#[command(args_conflicts_with_subcommands = true)]
pub struct BuildArgs {
    /// The specification, a Markdown file. The planner splits it into work items; each
    /// runs as a fresh session, and is done when its acceptance command and the project's
    /// check both exit 0. Needs a git repository with a clean tree.
    pub spec: Option<std::path::PathBuf>,

    /// Stop between work items once the build's estimated cost reaches this many USD.
    #[arg(long, global = true, value_name = "USD")]
    pub budget_usd: Option<f64>,

    /// Stop between work items once the build's weighted tokens reach this number.
    #[arg(long, global = true, value_name = "N")]
    pub budget_tokens: Option<u64>,

    /// Resume or inspect a build instead of starting one.
    #[command(subcommand)]
    pub command: Option<BuildCommand>,
}

/// Subcommands under `anvil build`.
#[derive(Debug, Subcommand)]
pub enum BuildCommand {
    /// Continue a build from its first work item that is not done.
    Resume {
        /// The build id; the newest unfinished build by default.
        id: Option<String>,
    },
    /// Print a build's work items, their state, attempts, spend and commits.
    Status {
        /// The build id; the newest build by default.
        id: Option<String>,
    },
}

/// Subcommands under `anvil template`.
#[derive(Debug, Subcommand)]
pub enum TemplateCommand {
    /// Copy a directory into `~/.anvil/templates/<name>`, honoring `.gitignore` and skipping `.git/`.
    Save {
        /// The template name: a-z, 0-9, `-` and `_`.
        name: String,
        /// The directory to save; the current one by default.
        dir: Option<std::path::PathBuf>,
        /// One line saying what the template is for.
        #[arg(long)]
        description: Option<String>,
        /// Replace a template of the same name.
        #[arg(long)]
        force: bool,
    },
}

/// Subcommands under `anvil memory`.
#[derive(Debug, Subcommand)]
pub enum MemoryCommand {
    /// Find files you edited after anvil wrote them and propose rules from the changes; you
    /// keep or drop each one, and nothing reaches `rules.toml` until you keep it.
    Learn,
}

/// Subcommands under `anvil ticket`.
#[derive(Debug, Subcommand)]
pub enum TicketCommand {
    /// Turn recorded signals into ticket drafts under `tickets/draft/` with the summarizer.
    Draft,
    /// List drafts and open tickets.
    List,
    /// Print one ticket.
    Show {
        /// A ticket id or a unique prefix of one.
        id: String,
    },
    /// Validate a draft, move it to `tickets/open/`, commit it on a `tickets/<id>` branch and push.
    ///
    /// This push is the only thing that ever leaves this machine toward the
    /// factory. The ticket carries no project code, paths or session text;
    /// the validator refuses one that does. Nothing is fetched.
    Submit {
        /// A ticket id or a unique prefix of one.
        #[arg(required_unless_present = "all")]
        id: Option<String>,
        /// Submit every draft.
        #[arg(long)]
        all: bool,
    },
    /// Delete a draft.
    Drop {
        /// A ticket id or a unique prefix of one.
        id: String,
    },
    /// Walk through every draft and submit, drop or edit each one.
    Review,
}

/// Subcommands under `anvil rules`.
#[derive(Debug, Subcommand)]
pub enum RulesCommand {
    /// List every rule, seeding the file with the defaults on first use.
    List,
    /// Add a rule. Options go before the rule text.
    Add {
        /// The rule id, kebab-case.
        id: String,
        /// Languages the rule applies to, comma-separated; `*` for all.
        #[arg(long, default_value = "*")]
        languages: String,
        /// A regex that must not match any line of a changed file.
        #[arg(long, conflicts_with = "command")]
        regex: Option<String>,
        /// A command that gets the changed files as arguments and must exit zero.
        #[arg(long)]
        command: Option<String>,
        /// The rule, one sentence.
        #[arg(required = true, num_args = 1..)]
        rule: Vec<String>,
    },
    /// Remove a rule by id.
    Remove {
        /// The rule id.
        id: String,
    },
    /// Run every rule check on the given files, or on the files changed since HEAD.
    Check {
        /// Files to check; relative paths resolve against the current directory.
        paths: Vec<std::path::PathBuf>,
    },
}

/// Subcommands under `anvil compat`.
#[derive(Debug, Subcommand)]
pub enum CompatCommand {
    /// Report what a `.claude` folder (or a plugin repository) would load in anvil.
    Check {
        /// The folder to check, such as `~/.claude` or a project's `.claude`.
        path: std::path::PathBuf,
    },
}

/// Subcommands under `anvil skill`.
#[derive(Debug, Subcommand)]
pub enum SkillCommand {
    /// Copy a skill folder into `~/.anvil/skills/<name>` (or the project with --project).
    Add {
        /// A directory, a git URL, or a GitHub `owner/repo[/subdir]` shorthand.
        source: String,
        /// Install into the project's `.anvil/skills` instead of the home directory.
        #[arg(long)]
        project: bool,
        /// Replace a skill of the same name that is already installed.
        #[arg(long)]
        force: bool,
    },
    /// List every skill anvil would load from here, highest priority first.
    List,
    /// Delete an installed skill by name.
    Remove {
        /// The skill name.
        name: String,
        /// Remove from the project's `.anvil/skills` instead of the home directory.
        #[arg(long)]
        project: bool,
    },
    /// Print a skill's metadata, supporting files and full instructions.
    Show {
        /// The skill name.
        name: String,
    },
}

/// Subcommands under `anvil config`.
#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Write a template config file. Refuses to overwrite an existing one.
    Init {
        /// Write the global `~/.anvil/config.toml` instead of the project `.anvil/config.toml`.
        #[arg(long)]
        global: bool,
    },
    /// Print the merged configuration (project over global) as TOML.
    Show,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allow_tool_repeats_before_or_after_the_subcommand() {
        let before = Cli::try_parse_from([
            "anvil",
            "--allow-tool",
            "edit_file",
            "--allow-tool",
            "write_file",
            "run",
            "fix it",
        ])
        .expect("parses");
        assert_eq!(before.allow_tools, vec!["edit_file", "write_file"]);
        let after = Cli::try_parse_from([
            "anvil",
            "chat",
            "--allow-tool",
            "edit_file",
            "--allow-tool",
            "shell",
        ])
        .expect("parses");
        assert_eq!(after.allow_tools, vec!["edit_file", "shell"]);
        let none = Cli::try_parse_from(["anvil", "fix it"]).expect("parses");
        assert!(none.allow_tools.is_empty());
    }

    #[test]
    fn build_takes_a_spec_or_a_subcommand_with_budgets_either_way() {
        let start = Cli::try_parse_from(["anvil", "build", "spec.md", "--budget-usd", "2.5"])
            .expect("parses");
        let Some(Command::Build(args)) = start.command else {
            panic!("a build command");
        };
        assert_eq!(args.spec.as_deref(), Some(std::path::Path::new("spec.md")));
        assert_eq!(args.budget_usd, Some(2.5));

        let resume = Cli::try_parse_from(["anvil", "build", "resume", "--budget-tokens", "9"])
            .expect("parses");
        let Some(Command::Build(args)) = resume.command else {
            panic!("a build command");
        };
        assert!(matches!(
            args.command,
            Some(BuildCommand::Resume { id: None })
        ));
        assert_eq!(args.budget_tokens, Some(9));

        let status = Cli::try_parse_from(["anvil", "build", "status", "01ABC"]).expect("parses");
        let Some(Command::Build(args)) = status.command else {
            panic!("a build command");
        };
        assert!(
            matches!(args.command, Some(BuildCommand::Status { id: Some(ref id) }) if id == "01ABC")
        );
    }
}
