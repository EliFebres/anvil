//! Builds the core objects from configuration: providers, tools, paths.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use forge_compat::{
    AgentSet, CommandSet, DelegateTool, HookConfig, HookRunner, McpTools, ServerSet, SkillRoots,
    SkillSet, SkillTool, SubagentEnv,
};
use forge_core::agent::{
    AutoApprove, CheckCommand, Instruction, LoopConfig, LoopProviders, PlanGate,
};
use forge_core::context;
use forge_core::provider::{self, Provider, SharedBudget, TokenBudget};
use forge_core::tools::{
    PermissionPrompt, Permissions, PromptAnswer, Shell, StaticPrompt, ToolContext, ToolRegistry,
};
use forge_types::ForgeConfig;

use crate::terminal::{TerminalGate, TerminalPrompt};

/// Everything a run or a chat needs, built once from config.
///
/// Cloning is cheap (shared sets, one config copy) and lets the `delegate`
/// tool carry a copy into subagent tasks.
#[derive(Clone)]
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
    /// Subagents from the `agents/` directories.
    pub agents: Arc<AgentSet>,
    /// Hooks from the settings files.
    pub hooks: Arc<HookConfig>,
    /// MCP server definitions from `.mcp.json`, settings and `~/.forge/mcp.toml`.
    pub mcp_servers: ServerSet,
    /// MCP tools, resolved by [`Wiring::load_mcp`]; empty until then.
    pub mcp: Arc<McpTools>,
    /// The session this wiring serves, once one exists.
    session: Option<SessionInfo>,
}

/// The session a run or chat belongs to, for hook payloads.
#[derive(Debug, Clone)]
struct SessionInfo {
    /// The session id.
    id: String,
    /// The session folder.
    dir: PathBuf,
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
        let agents = Arc::new(AgentSet::discover(&cwd, home.as_deref()));
        let hooks = Arc::new(HookConfig::load(&cwd, home.as_deref()));
        let mcp_servers = ServerSet::load(&cwd, home.as_deref());
        Ok(Self {
            cwd,
            home,
            config,
            budget,
            skills,
            commands,
            agents,
            hooks,
            mcp_servers,
            mcp: Arc::new(McpTools::default()),
            session: None,
        })
    }

    /// Resolves every MCP server's tool list so [`Wiring::registry`] can
    /// register them: from the cache when it is current, by connecting
    /// otherwise. Servers that cannot be reached are logged and skipped.
    pub async fn load_mcp(&mut self) {
        if self.mcp_servers.is_empty() {
            return;
        }
        self.mcp = Arc::new(McpTools::load(&self.mcp_servers, &self.cwd, false).await);
        for report in self.mcp.reports() {
            if let Err(error) = &report.tools {
                eprintln!("warning: {error}");
            }
        }
    }

    /// A copy of this wiring bound to a session, so hooks can report it.
    ///
    /// # Arguments
    ///
    /// * `id` - The session id.
    /// * `dir` - The session folder.
    ///
    /// # Returns
    ///
    /// The bound wiring.
    pub fn with_session(&self, id: &str, dir: &Path) -> Self {
        let mut wiring = self.clone();
        wiring.session = Some(SessionInfo {
            id: id.to_owned(),
            dir: dir.to_path_buf(),
        });
        wiring
    }

    /// The hook runner for this session, when any hook is configured.
    ///
    /// # Returns
    ///
    /// The runner, or `None` when the settings files define no hooks.
    pub fn hook_runner(&self) -> Option<Arc<HookRunner>> {
        if self.hooks.is_empty() {
            return None;
        }
        let (id, dir) = match &self.session {
            Some(session) => (session.id.clone(), Some(session.dir.clone())),
            None => ("none".to_owned(), None),
        };
        Some(Arc::new(HookRunner::new(
            Arc::clone(&self.hooks),
            self.cwd.clone(),
            Shell::from_config(self.config.tools.shell.as_deref()),
            id,
            dir,
        )))
    }

    /// Wraps `SessionStart` hook output as a prompt section.
    ///
    /// # Arguments
    ///
    /// * `context` - What the hooks printed.
    ///
    /// # Returns
    ///
    /// The section, or `None` when they printed nothing.
    pub fn session_start_instruction(context: Vec<String>) -> Option<Instruction> {
        (!context.is_empty()).then(|| Instruction {
            source: "SessionStart hooks".to_owned(),
            text: context.join("\n"),
        })
    }

    /// Resolves a subagent's `model` key to a provider name.
    ///
    /// A role name (`coder`, `reviewer`, `summarizer`) maps through
    /// `[roles]`; a provider name is used as is; anything else falls back to
    /// the coder.
    ///
    /// # Arguments
    ///
    /// * `model` - The agent's `model` key, if any.
    ///
    /// # Returns
    ///
    /// The provider name and, when the key was unknown, a warning to show.
    pub fn resolve_model(&self, model: Option<&str>) -> (String, Option<String>) {
        let roles = &self.config.roles;
        let Some(model) = model.map(str::trim).filter(|model| !model.is_empty()) else {
            return (roles.coder.clone(), None);
        };
        let by_role = match model {
            "coder" => Some(roles.coder.clone()),
            "summarizer" => Some(roles.summarizer.clone()),
            "reviewer" => Some(
                roles
                    .reviewer
                    .clone()
                    .unwrap_or_else(|| roles.coder.clone()),
            ),
            _ => None,
        };
        if let Some(name) = by_role {
            return (name, None);
        }
        if self.config.providers.contains_key(model) {
            return (model.to_owned(), None);
        }
        (
            roles.coder.clone(),
            Some(format!(
                "model `{model}` is not a role or a provider in the config; using the coder `{}`",
                roles.coder
            )),
        )
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

    /// The tool registry with the project's permissions: the built-ins, the
    /// `skill` tool and, when any subagent is defined, the `delegate` tool.
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
        let mut registry = self.registry_without_delegate(prompt)?;
        if !self.agents.is_empty() {
            registry.register(Box::new(DelegateTool::new(
                Arc::clone(&self.agents),
                Arc::new(self.clone()),
            )));
        }
        Ok(registry)
    }

    /// The registry a subagent gets: everything but `delegate`, so
    /// delegations do not nest.
    fn registry_without_delegate(
        &self,
        prompt: Box<dyn PermissionPrompt>,
    ) -> anyhow::Result<ToolRegistry> {
        let permissions = Permissions::load(&self.cwd).context("loading permissions")?;
        let mut registry = ToolRegistry::with_builtins(permissions, prompt);
        registry.register(Box::new(SkillTool::new(Arc::clone(&self.skills))));
        self.mcp.register(&mut registry);
        if let Some(runner) = self.hook_runner() {
            registry.set_hook(runner);
        }
        Ok(registry)
    }

    /// Prompt sections that come from outside forge-core: the Claude Code
    /// instruction files and `@imports` forge-core does not load itself,
    /// then the skill list and the subagent list.
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
        extra.extend(self.agents.prompt_section());
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

impl SubagentEnv for Wiring {
    fn provider(&self, model: Option<&str>) -> Result<Arc<dyn Provider>, String> {
        let (name, warning) = self.resolve_model(model);
        if let Some(warning) = warning {
            tracing::warn!("{warning}");
        }
        Wiring::provider(self, &name).map_err(|error| format!("{error:#}"))
    }

    fn registry(&self) -> Result<ToolRegistry, String> {
        self.registry_without_delegate(Box::new(StaticPrompt(PromptAnswer::No)))
            .map_err(|error| format!("{error:#}"))
    }

    fn tool_context(&self) -> ToolContext {
        Wiring::tool_context(self)
    }

    fn context_window(&self) -> u32 {
        Wiring::context_window(self)
    }
}
