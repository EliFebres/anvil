//! Builds the core objects from configuration: providers, tools, paths.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use forge_compat::{CommandSet, SkillRoots, SkillSet, SkillTool};
use forge_core::agent::{
    AutoApprove, CheckCommand, Instruction, LoopConfig, LoopProviders, PlanGate,
};
use forge_core::context;
use forge_core::provider::{self, Provider, SharedBudget, TokenBudget};
use forge_core::tools::{PermissionPrompt, Permissions, Shell, ToolContext, ToolRegistry};
use forge_types::ForgeConfig;

use crate::terminal::{TerminalGate, TerminalPrompt};

/// Everything a run or a chat needs, built once from config.
pub struct Wiring {
    /// The project root.
    pub cwd: PathBuf,
    /// The home directory, if known.
    pub home: Option<PathBuf>,
    /// The merged configuration.
    pub config: ForgeConfig,
    /// The session's token budget, shared by every provider.
    pub budget: SharedBudget,
    /// Skills discovered from the project, the home directory and the bundle.
    pub skills: Arc<SkillSet>,
    /// Slash commands from the `commands/` directories.
    pub commands: CommandSet,
}

/// The directories skills are discovered from.
///
/// The bundled `skills/` directory is the one next to this source tree; it is
/// only present on a machine that builds forge from the monorepo.
///
/// # Arguments
///
/// * `cwd` - The project root.
/// * `home` - The home directory, if known.
///
/// # Returns
///
/// The roots.
pub fn skill_roots(cwd: &Path, home: Option<&Path>) -> SkillRoots {
    let bundled = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../skills"));
    SkillRoots {
        project: cwd.to_path_buf(),
        home: home.map(Path::to_path_buf),
        bundled: bundled.is_dir().then_some(bundled),
    }
}

impl Wiring {
    /// Captures the environment.
    ///
    /// # Arguments
    ///
    /// * `config` - The merged configuration.
    ///
    /// # Returns
    ///
    /// The wiring.
    ///
    /// # Errors
    ///
    /// When the current directory cannot be determined.
    pub fn new(config: ForgeConfig) -> anyhow::Result<Self> {
        let cwd = std::env::current_dir().context("cannot determine the current directory")?;
        let home = std::env::home_dir();
        let budget = TokenBudget::shared(config.limits.session_tokens);
        let skills = Arc::new(SkillSet::discover(&skill_roots(&cwd, home.as_deref())));
        let commands = CommandSet::discover(&cwd, home.as_deref());
        Ok(Self {
            cwd,
            home,
            config,
            budget,
            skills,
            commands,
        })
    }

    /// Expands a `/<name> [args]` line into the message to send: a skill
    /// invocation first, then a slash command.
    ///
    /// # Arguments
    ///
    /// * `line` - The user's line.
    ///
    /// # Returns
    ///
    /// The message, or `None` when the line is not a known slash command.
    pub fn expand_slash(&self, line: &str) -> Option<String> {
        self.skills
            .expand_slash(line)
            .or_else(|| self.commands.expand_slash(line))
    }

    /// Builds the provider for a config entry.
    ///
    /// # Arguments
    ///
    /// * `name` - The key under `[providers]`.
    ///
    /// # Returns
    ///
    /// The provider stack.
    ///
    /// # Errors
    ///
    /// When the name is unknown or the provider cannot be built.
    pub fn provider(&self, name: &str) -> anyhow::Result<Arc<dyn Provider>> {
        let entry = self
            .config
            .providers
            .get(name)
            .with_context(|| format!("no provider named `{name}` in [providers]"))?;
        let provider =
            provider::from_config(name, entry, &self.config.limits, Arc::clone(&self.budget))
                .with_context(|| format!("building provider `{name}`"))?;
        Ok(Arc::from(provider))
    }

    /// The coder, reviewer and summarizer named in `[roles]`.
    ///
    /// # Returns
    ///
    /// The providers; the reviewer falls back to the coder.
    ///
    /// # Errors
    ///
    /// When a provider cannot be built.
    pub fn loop_providers(&self) -> anyhow::Result<LoopProviders> {
        let roles = &self.config.roles;
        let coder = self.provider(&roles.coder)?;
        let reviewer = match &roles.reviewer {
            Some(name) if name != &roles.coder => self.provider(name)?,
            _ => Arc::clone(&coder),
        };
        let summarizer = if roles.summarizer == roles.coder {
            Arc::clone(&coder)
        } else {
            self.provider(&roles.summarizer)?
        };
        Ok(LoopProviders {
            coder,
            reviewer,
            summarizer,
        })
    }

    /// The coder's context window.
    ///
    /// # Returns
    ///
    /// Tokens.
    pub fn context_window(&self) -> u32 {
        self.config
            .providers
            .get(&self.config.roles.coder)
            .map(|entry| entry.context_window)
            .unwrap_or(32_000)
    }

    /// The tool registry with the project's permissions: the built-ins plus
    /// the `skill` tool.
    ///
    /// # Arguments
    ///
    /// * `prompt` - Who answers `ask` decisions.
    ///
    /// # Returns
    ///
    /// The registry.
    ///
    /// # Errors
    ///
    /// When `.forge/permissions.toml` exists but cannot be read.
    pub fn registry(&self, prompt: Box<dyn PermissionPrompt>) -> anyhow::Result<ToolRegistry> {
        let permissions = Permissions::load(&self.cwd).context("loading permissions")?;
        let mut registry = ToolRegistry::with_builtins(permissions, prompt);
        registry.register(Box::new(SkillTool::new(Arc::clone(&self.skills))));
        Ok(registry)
    }

    /// Prompt sections that come from outside forge-core: the Claude Code
    /// instruction files and `@imports` forge-core does not load itself,
    /// then the skill list.
    ///
    /// # Arguments
    ///
    /// * `tool_names` - The registered tools, to flag skills whose
    ///   `requires` are unmet.
    ///
    /// # Returns
    ///
    /// The sections, possibly empty.
    pub fn extra_instructions(&self, tool_names: &[&str]) -> Vec<Instruction> {
        let base =
            context::discover_instructions(self.home.as_deref(), &self.cwd, &BTreeSet::new());
        let mut extra =
            forge_compat::instructions::discover_extra(self.home.as_deref(), &self.cwd, &base);
        extra.extend(self.skills.prompt_section(tool_names));
        extra
    }

    /// Where tools run.
    ///
    /// # Returns
    ///
    /// The context, with the configured shell.
    pub fn tool_context(&self) -> ToolContext {
        ToolContext {
            cwd: self.cwd.clone(),
            session_dir: None,
            shell: Shell::from_config(self.config.tools.shell.as_deref()),
        }
    }

    /// The loop settings for a task.
    ///
    /// # Arguments
    ///
    /// * `task` - The task.
    /// * `session_dir` - The session folder.
    /// * `registry` - The tools the run will have, for the skill list.
    ///
    /// # Returns
    ///
    /// The config.
    pub fn loop_config(
        &self,
        task: String,
        session_dir: PathBuf,
        registry: &ToolRegistry,
    ) -> LoopConfig {
        LoopConfig {
            task,
            cwd: self.cwd.clone(),
            session_dir,
            limits: self.config.limits.clone(),
            check: CheckCommand::Detect,
            context_window: self.context_window(),
            home_dir: self.home.clone(),
            extra_instructions: self.extra_instructions(&registry.names()),
        }
    }

    /// A fresh session folder.
    ///
    /// # Returns
    ///
    /// Its path and id.
    pub fn new_session_dir(&self) -> (PathBuf, String) {
        let id = context::new_session_id();
        (context::session_dir(&self.cwd, &id), id)
    }

    /// The plan gate for this invocation.
    ///
    /// # Arguments
    ///
    /// * `yes` - Whether `--yes` was given.
    ///
    /// # Returns
    ///
    /// Auto-approval with `--yes`, the terminal otherwise.
    pub fn gate(yes: bool) -> Box<dyn PlanGate> {
        if yes {
            Box::new(AutoApprove)
        } else {
            Box::new(TerminalGate)
        }
    }

    /// The permission prompt for this invocation.
    ///
    /// # Returns
    ///
    /// The terminal prompt.
    pub fn prompt() -> Box<dyn PermissionPrompt> {
        Box::new(TerminalPrompt)
    }
}
