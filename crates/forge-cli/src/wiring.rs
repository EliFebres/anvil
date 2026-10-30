//! Builds the core objects from configuration: providers, tools, paths.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use forge_core::agent::{AutoApprove, CheckCommand, LoopConfig, LoopProviders, PlanGate};
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
        let budget = TokenBudget::shared(config.limits.session_tokens);
        Ok(Self {
            cwd,
            home: std::env::home_dir(),
            config,
            budget,
        })
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

    /// The tool registry with the project's permissions.
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
        Ok(ToolRegistry::with_builtins(permissions, prompt))
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
    ///
    /// # Returns
    ///
    /// The config.
    pub fn loop_config(&self, task: String, session_dir: PathBuf) -> LoopConfig {
        LoopConfig {
            task,
            cwd: self.cwd.clone(),
            session_dir,
            limits: self.config.limits.clone(),
            check: CheckCommand::Detect,
            context_window: self.context_window(),
            home_dir: self.home.clone(),
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
