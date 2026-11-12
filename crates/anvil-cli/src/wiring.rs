//! Builds the core objects from configuration: providers, tools, paths.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anvil_compat::{
    AgentSet, CommandSet, DelegateTool, HookConfig, HookRunner, McpTools, ServerSet, SkillRoots,
    SkillSet, SkillTool, SubagentEnv,
};
use anvil_core::agent::{
    ChangeChecks, CheckCommand, Instruction, LoopConfig, LoopProviders, PromptSection,
};
use anvil_core::context;
use anvil_core::provider::{self, Provider, SharedBudget, TokenBudget};
use anvil_core::tools::{
    PermissionObserver, PermissionPrompt, Permissions, PromptAnswer, SessionApprovals, Shell,
    StaticPrompt, ToolContext, ToolRegistry,
};
use anvil_types::{AnvilConfig, PromptConfig};
use anyhow::Context;

use crate::memory::{self, RuleChecks};
use crate::terminal::TerminalPrompt;

/// The home directory: `ANVIL_HOME` when set, otherwise the OS home.
///
/// The override exists so tests can point `~/.anvil` at a temp folder;
/// Windows finds the real home without `HOME` or `USERPROFILE`.
///
/// # Returns
///
/// The directory, if any.
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("ANVIL_HOME")
        .map(PathBuf::from)
        .or_else(std::env::home_dir)
}

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
    pub config: AnvilConfig,
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
    /// MCP server definitions from `.mcp.json`, settings and `~/.anvil/mcp.toml`.
    pub mcp_servers: ServerSet,
    /// MCP tools, resolved by [`Wiring::load_mcp`]; empty until then.
    pub mcp: Arc<McpTools>,
    /// The session this wiring serves, once one exists.
    session: Option<SessionInfo>,
    /// Session-scoped tool approvals, shared by every registry and clone so
    /// sub-agents see the approvals the user gave the main loop.
    approvals: Arc<Mutex<SessionApprovals>>,
    /// The session's permission observer, shared by every clone so
    /// sub-agent registries built later report to it too.
    observer: Arc<Mutex<Option<Arc<dyn PermissionObserver>>>>,
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
/// only present on a machine that builds anvil from the monorepo.
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
    /// When the current directory cannot be determined, or when
    /// `permissions.allow_tools` names an invalid tool.
    pub fn new(config: AnvilConfig) -> anyhow::Result<Self> {
        let cwd = std::env::current_dir().context("cannot determine the current directory")?;
        let home = home_dir();
        let budget = TokenBudget::shared(config.limits.session_tokens);
        let skills = Arc::new(SkillSet::discover(&skill_roots(&cwd, home.as_deref())));
        let commands = CommandSet::discover(&cwd, home.as_deref());
        let agents = Arc::new(AgentSet::discover(&cwd, home.as_deref()));
        let hooks = Arc::new(HookConfig::load(&cwd, home.as_deref()));
        let mcp_servers = ServerSet::load(&cwd, home.as_deref());
        let approvals = Arc::new(Mutex::new(
            SessionApprovals::preapproved(config_allow_tools(&config))
                .context("permissions.allow_tools in the configuration")?,
        ));
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
            approvals,
            observer: Arc::new(Mutex::new(None)),
        })
    }

    /// Pre-approves tools for this session on top of `permissions.allow_tools`,
    /// as `--allow-tool` does. Call it before building any registry.
    ///
    /// # Arguments
    ///
    /// * `tools` - Tool names.
    ///
    /// # Errors
    ///
    /// When a name is empty or contains whitespace or a `:`.
    pub fn allow_tools(&mut self, tools: &[String]) -> anyhow::Result<()> {
        let all = config_allow_tools(&self.config).chain(tools.iter().map(String::as_str));
        let approvals = SessionApprovals::preapproved(all).context("--allow-tool")?;
        self.approvals = Arc::new(Mutex::new(approvals));
        Ok(())
    }

    /// Reports every permission decision of this session, in the main
    /// registry and in every sub-agent registry, to `observer`. Call it
    /// before [`Wiring::registry`]; clones share the setting.
    ///
    /// # Arguments
    ///
    /// * `observer` - The observer, such as the session telemetry.
    pub fn set_permission_observer(&self, observer: Arc<dyn PermissionObserver>) {
        if let Ok(mut slot) = self.observer.lock() {
            *slot = Some(observer);
        }
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
    /// A role name (`coder`, `planner`, `reviewer`, `summarizer`) maps through
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
            "planner" => Some(roles.planner_or_coder().to_owned()),
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
    /// The providers; the reviewer falls back to the coder, and the planner is
    /// `None` unless it names a different provider than the coder.
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
        let planner = match &roles.planner {
            Some(name) if name != &roles.coder => Some(self.provider(name)?),
            _ => None,
        };
        Ok(LoopProviders {
            coder,
            planner,
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
    /// When `.anvil/permissions.toml` exists but cannot be read.
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
        let permissions = match &self.config.permissions {
            Some(rules) if rules.has_rules() => {
                Permissions::from_rules(rules).context("permissions in the configuration")?
            }
            _ => Permissions::load(&self.cwd).context("loading permissions")?,
        };
        let mut registry = ToolRegistry::with_builtins(permissions, prompt)
            .with_session_approvals(Arc::clone(&self.approvals));
        let observer = self.observer.lock().ok().and_then(|slot| slot.clone());
        if let Some(observer) = observer {
            registry.set_observer(observer);
        }
        registry.register(Box::new(SkillTool::new(Arc::clone(&self.skills))));
        self.mcp.register(&mut registry);
        if let Some(runner) = self.hook_runner() {
            registry.set_hook(runner);
        }
        Ok(registry)
    }

    /// Prompt sections that come from outside anvil-core: the Claude Code
    /// instruction files and `@imports` anvil-core does not load itself,
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
            anvil_compat::instructions::discover_extra(self.home.as_deref(), &self.cwd, &base);
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

    /// The loop settings for a task, with the standing rules and the
    /// remembered facts as prompt sections.
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
    ///
    /// # Errors
    ///
    /// When `~/.anvil/rules.toml` is malformed.
    pub fn loop_config(
        &self,
        task: String,
        session_dir: PathBuf,
        registry: &ToolRegistry,
    ) -> anyhow::Result<LoopConfig> {
        let mut sections = memory::prompt_sections(self.home.as_deref(), &self.cwd)?;
        sections.coder.extend(profile_section(&self.config.prompt));
        Ok(LoopConfig {
            task,
            cwd: self.cwd.clone(),
            session_dir,
            limits: self.config.limits.clone(),
            check: self
                .config
                .tools
                .check
                .clone()
                .map_or(CheckCommand::Detect, CheckCommand::Command),
            context_window: self.context_window(),
            home_dir: self.home.clone(),
            extra_instructions: self.extra_instructions(&registry.names()),
            sections: sections.coder,
            review_sections: sections.review,
        })
    }

    /// The standing rule checks for the Verify state.
    ///
    /// # Returns
    ///
    /// The checks, or `None` without a home directory.
    ///
    /// # Errors
    ///
    /// When `~/.anvil/rules.toml` is malformed.
    pub fn change_checks(&self) -> anyhow::Result<Option<Arc<dyn ChangeChecks>>> {
        match &self.home {
            Some(home) => Ok(Some(Arc::new(RuleChecks::load(home)?))),
            None => Ok(None),
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

/// The tools `permissions.allow_tools` pre-approves, if any.
fn config_allow_tools(config: &AnvilConfig) -> impl Iterator<Item = &str> {
    config
        .permissions
        .iter()
        .flat_map(|rules| rules.allow_tools.iter().map(String::as_str))
}

/// The heading the configured `[prompt] notes` appear under.
const PROFILE_NOTES_HEADING: &str = "Profile notes";

/// The `[prompt] notes` as one coder section, or nothing when there are none.
fn profile_section(prompt: &PromptConfig) -> Option<PromptSection> {
    if prompt.notes.is_empty() {
        return None;
    }
    let text = prompt
        .notes
        .iter()
        .map(|note| format!("- {}", note.trim()))
        .collect::<Vec<_>>()
        .join("\n");
    Some(PromptSection {
        heading: PROFILE_NOTES_HEADING.to_owned(),
        text,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_notes_no_section() {
        assert_eq!(profile_section(&PromptConfig::default()), None);
    }

    #[test]
    fn notes_become_one_bulleted_section() {
        let prompt = PromptConfig {
            notes: vec![
                "one program per command".to_owned(),
                " no pipes ".to_owned(),
            ],
        };
        let section = profile_section(&prompt).expect("a section");
        assert_eq!(section.heading, "Profile notes");
        assert_eq!(section.text, "- one program per command\n- no pipes");
    }

    /// Five `edit_file` calls in `dir` through `registry`; returns how many
    /// completed.
    async fn completed_edits(registry: &ToolRegistry, dir: &Path) -> usize {
        let ctx = ToolContext::new(dir);
        let mut completed = 0;
        for i in 0..5 {
            let name = format!("f{i}.txt");
            std::fs::write(dir.join(&name), "old\n").expect("write");
            let call = anvil_core::provider::ToolCall {
                id: format!("e{i}"),
                name: "edit_file".to_owned(),
                arguments_json: serde_json::json!({"path": name, "old": "old", "new": "new"})
                    .to_string(),
            };
            let execution = registry.execute(&call, &ctx).await;
            if execution.outcome == anvil_core::tools::ToolOutcome::Completed {
                completed += 1;
            }
        }
        completed
    }

    fn config(extra: &str) -> AnvilConfig {
        toml::from_str(&format!(
            "{}\n{extra}",
            anvil_types::config::CONFIG_TEMPLATE
        ))
        .expect("config parses")
    }

    fn refusing() -> Box<dyn PermissionPrompt> {
        Box::new(StaticPrompt(PromptAnswer::No))
    }

    #[tokio::test]
    async fn allow_tool_flag_and_config_key_pre_approve_edit_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut flagged = Wiring::new(config("")).expect("wiring");
        flagged
            .allow_tools(&["edit_file".to_owned()])
            .expect("valid names");
        let registry = flagged.registry(refusing()).expect("registry");
        assert_eq!(completed_edits(&registry, dir.path()).await, 5);

        let configured =
            Wiring::new(config("[permissions]\nallow_tools = [\"edit_file\"]\n")).expect("wiring");
        let registry = configured.registry(refusing()).expect("registry");
        assert_eq!(completed_edits(&registry, dir.path()).await, 5);

        let neither = Wiring::new(config("")).expect("wiring");
        let registry = neither.registry(refusing()).expect("registry");
        assert_eq!(completed_edits(&registry, dir.path()).await, 0);
    }

    #[test]
    fn the_planner_is_a_role_name_for_model_keys() {
        let mut config = config("");
        let coder = config.roles.coder.clone();
        let wiring = Wiring::new(config.clone()).expect("wiring");
        assert_eq!(wiring.resolve_model(Some("planner")), (coder.clone(), None));

        let big = config.providers[&coder].clone();
        config.providers.insert("big".to_owned(), big);
        config.roles.planner = Some("big".to_owned());
        let wiring = Wiring::new(config).expect("wiring");
        assert_eq!(
            wiring.resolve_model(Some("planner")),
            ("big".to_owned(), None)
        );
        assert_eq!(wiring.resolve_model(Some("coder")), (coder, None));
    }

    #[test]
    fn invalid_allow_tool_names_are_errors() {
        let mut wiring = Wiring::new(config("")).expect("wiring");
        let error = wiring
            .allow_tools(&["edit file".to_owned()])
            .expect_err("whitespace");
        assert!(format!("{error:#}").contains("--allow-tool"), "{error:#}");
        let configured = Wiring::new(config("[permissions]\nallow_tools = [\"\"]\n"));
        assert!(configured.is_err());
    }

    #[tokio::test]
    async fn subagent_registry_shares_the_session_approvals() {
        let dir = tempfile::tempdir().expect("tempdir");
        let wiring = Wiring::new(config("")).expect("wiring");
        let main = wiring.registry(refusing()).expect("registry");
        main.session_approvals()
            .lock()
            .expect("lock")
            .approve("edit_file", dir.path())
            .expect("approve");
        let subagent = SubagentEnv::registry(&wiring.clone()).expect("registry");
        assert_eq!(completed_edits(&subagent, dir.path()).await, 5);
    }
}
