//! Provider and role configuration: which model does which job.
//!
//! Lives in the user's own `~/.anvil/config.toml`, optionally with a named
//! profile on top. A repository's `.anvil/config.toml` is not part of it:
//! a repository can only add permission rules that tighten (see
//! [`crate::project`]). Models are swapped by editing these files, never by
//! changing code.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{ConfigError, ValidationError};
use crate::shell::ShellConfig;

/// Directory that holds anvil configuration, in the home directory and in a repository.
pub const CONFIG_DIR: &str = ".anvil";

/// File name of the configuration file inside [`CONFIG_DIR`].
pub const CONFIG_FILE: &str = "config.toml";

/// Folder of named profiles under the home directory: `~/.anvil/profiles/<name>.toml`.
pub const PROFILES_DIR: &str = ".anvil/profiles";

/// The commented template written by `anvil config init --global`.
pub const CONFIG_TEMPLATE: &str = r#"# anvil configuration: ~/.anvil/config.toml.
# A repository's own .anvil/config.toml cannot change anything here; it can
# only add permission rules that refuse calls or make them ask first.
# Swap models by editing this file; no code changes are needed.

# One example provider: a self-hosted, OpenAI-compatible model server on your
# own network (vLLM, llama.cpp, an internal gateway). Replace base_url and
# model, or run `anvil setup --endpoint <url>` to fill them in from the server.
# Hosted APIs (protocol "anthropic" or "openai") work the same way;
# docs/USAGE.md shows an example.
[providers.endpoint]
protocol = "openai"          # "openai" or "anthropic"
base_url = "https://models.internal.example/v1"
api_key_env = "ANVIL_ENDPOINT_KEY"   # environment variable that holds the API key
model = "<model name as served>"
context_window = 32768
supports_tools = true
supports_reasoning = false
# max_output_tokens = 16384   # optional; Anthropic requires one and defaults to 16384
# USD per million tokens, used by `anvil stats` to estimate cost. Optional;
# `cached` falls back to `input`.
# price_per_mtoken = { input = 3.0, output = 15.0, cached = 0.3 }

# Each role names a provider above, so the coder, the summarizer and the
# reviewer can each be a different model.
[roles]
coder = "endpoint"
summarizer = "endpoint"
# reviewer = "endpoint"
# Optional: a model that plans, and takes a build round when the coder is
# stuck. The coder plans when this is absent.
# planner = "endpoint"
# Optional: a model for delegated work, the `explore` and `implement`
# subagents and any agent file with `model: worker`. The coder when absent.
# worker = "endpoint"

# Optional. The shell program behind the `shell` tool: "powershell", "pwsh",
# "sh", "bash", "zsh" or "cmd". Default: PowerShell on Windows, sh elsewhere.
# [tools]
# shell = "pwsh"
# Folders outside the project the file tools may open; nothing else outside is.
# extra_dirs = ["C:/shared/specs"]
# Variables shell commands and hooks receive on top of the built-in list
# (PATH, HOME, TEMP, the toolchain folders). Others, keys included, are not passed.
# pass_env = ["JAVA_HOME"]

# Optional. How the `shell` tool runs commands. "ask" (the default) runs
# each command in the shell above after asking; matching command text there
# is a guard rail, not a boundary. "allowlist" starts only commands that
# match a pattern, as one program with plain arguments and no shell. "off"
# removes the tool. executor = "container" runs commands in a container
# without network access.
# [shell]
# mode = "allowlist"
# allow = ["cargo test *", "cargo check *", "git diff *", "git status"]

# Every key is optional; these are the defaults.
# [limits]
# request_timeout_s = 600      # wall clock for one completion, streaming included
# max_retries = 4              # on HTTP 429 and 5xx, with exponential backoff
# session_tokens = 2000000     # abort the session once this many tokens are used
#
# Token budgets count input plus output plus cached input at a tenth, which is
# roughly what is paid. Per-state budgets (plan, build, review, verify) are
# summed over every visit to the state. Exceeding one ends the session as
# incomplete.
# [limits.build]
# max_turns = 100
# max_tokens = 1000000

# After a run, a summarizer turn proposes facts for memory. It is skipped
# when the run finished with no reviewer rejection and no failed check and
# changed fewer than 50 lines. Set this to run it after every session.
# [memory]
# always_summarize = false

# Hosts a provider may be reached on over plain http, such as a GPU server on
# your own network. Everything else must use https, except localhost.
# [network]
# allow_http_hosts = ["gpu.lan"]

# Lines added to the coder's system prompt, one instruction each. Profiles use
# this to explain their own rules.
# [prompt]
# notes = ["Shell commands are a single program with arguments."]
# delegate_guidance = true   # tell the model when delegating to a subagent pays off
"#;

/// Sections older versions of anvil read, each with the reason it is gone.
///
/// A file that still sets one fails to load with that reason, so nobody
/// believes a feature is on when it no longer exists.
const REMOVED_SECTIONS: [(&str, &str); 5] = [
    (
        "update",
        "self-update was removed; updates come from whoever distributes the anvil binary",
    ),
    (
        "notify",
        "webhook notifications were removed; anvil no longer posts session updates anywhere",
    ),
    (
        "tickets",
        "tickets were removed; anvil no longer drafts or submits tickets",
    ),
    (
        "telemetry",
        "the ticket signal thresholds were removed with ticket drafting",
    ),
    (
        "integrations",
        "herdr reporting was removed; anvil reports a session only in its status.json",
    ),
];

/// The provider and roles `anvil setup` adds to `~/.anvil/config.toml`: one
/// example of a self-hosted, OpenAI-compatible model server.
///
/// Hosted APIs are a documented option, never something setup writes.
pub const SETUP_PROVIDERS: &str = r#"# anvil configuration, written by `anvil setup`: ~/.anvil/config.toml.
# A repository's own .anvil/config.toml cannot change providers or roles.
# Keys never live here: each provider names the environment variable that
# holds its key, read from the environment or ~/.anvil/.env.

# One example provider: a self-hosted, OpenAI-compatible model server on your
# own network (vLLM, llama.cpp, an internal gateway). Replace base_url and
# model, or run `anvil setup --endpoint <url>` to fill them in from the server.
[providers.endpoint]
protocol = "openai"
base_url = "https://models.internal.example/v1"
api_key_env = "ANVIL_ENDPOINT_KEY"
model = "<model name as served>"
context_window = 32768
supports_tools = true
supports_reasoning = false

# Each role names a provider above.
[roles]
coder = "endpoint"
summarizer = "endpoint"
# reviewer = "endpoint"
# Optional: a model that plans, and takes a build round when the coder is
# stuck. The coder plans when this is absent.
# planner = "endpoint"
# Optional: a model for delegated work (the `explore` and `implement`
# subagents). The coder when absent.
# worker = "endpoint"
"#;

/// The optional, commented sections `anvil setup` appends to a new config file.
pub const OPTIONAL_SECTIONS: &str = r#"
# Optional. The shell program behind the `shell` tool: "powershell", "pwsh",
# "sh", "bash", "zsh" or "cmd". Default: PowerShell on Windows, sh elsewhere.
# [tools]
# shell = "pwsh"
# Folders outside the project the file tools may open; nothing else outside is.
# extra_dirs = ["C:/shared/specs"]
# Variables shell commands and hooks receive on top of the built-in list
# (PATH, HOME, TEMP, the toolchain folders). Others, keys included, are not passed.
# pass_env = ["JAVA_HOME"]

# Optional. How the `shell` tool runs commands. "ask" (the default) runs
# each command in the shell above after asking; matching command text there
# is a guard rail, not a boundary. "allowlist" starts only commands that
# match a pattern, as one program with plain arguments and no shell. "off"
# removes the tool. executor = "container" runs commands in a container
# without network access.
# [shell]
# mode = "allowlist"
# allow = ["cargo test *", "cargo check *", "git diff *", "git status"]

# Every key is optional; these are the defaults.
# [limits]
# request_timeout_s = 600      # wall clock for one completion, streaming included
# max_retries = 4              # on HTTP 429 and 5xx, with exponential backoff
# session_tokens = 2000000     # abort the session once this many tokens are used
#
# Token budgets count input plus output plus cached input at a tenth, which is
# roughly what is paid. Per-state budgets (plan, build, review, verify) are
# summed over every visit to the state. Exceeding one ends the session as
# incomplete.
# [limits.build]
# max_turns = 100
# max_tokens = 1000000

# After a run, a summarizer turn proposes facts for memory. It is skipped
# when the run finished with no reviewer rejection and no failed check and
# changed fewer than 50 lines. Set this to run it after every session.
# [memory]
# always_summarize = false

"#;

/// Wire protocol a provider speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    /// OpenAI chat-completions style API.
    Openai,
    /// Anthropic messages style API.
    Anthropic,
    /// A scripted provider that replays `script` and calls no API: for
    /// tests and other offline runs.
    Fake,
}

/// What a provider charges, in USD per million tokens.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Price {
    /// Price of uncached input tokens.
    pub input: f64,
    /// Price of output tokens.
    pub output: f64,
    /// Price of cached input tokens; `input` when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached: Option<f64>,
}

impl Price {
    /// Estimates the cost of a token count.
    ///
    /// # Arguments
    ///
    /// * `input` - Uncached input tokens.
    /// * `output` - Output tokens.
    /// * `cached` - Cached input tokens.
    ///
    /// # Returns
    ///
    /// The cost in USD.
    pub fn cost(&self, input: u64, output: u64, cached: u64) -> f64 {
        let per_token = |price: f64| price / 1_000_000.0;
        input as f64 * per_token(self.input)
            + output as f64 * per_token(self.output)
            + cached as f64 * per_token(self.cached.unwrap_or(self.input))
    }

    /// Whether every price is zero or positive.
    fn is_valid(&self) -> bool {
        self.input >= 0.0 && self.output >= 0.0 && self.cached.is_none_or(|c| c >= 0.0)
    }
}

/// One model endpoint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    /// Which API shape to speak.
    pub protocol: Protocol,
    /// Base URL of the API, including any version prefix such as `/v1`.
    /// Empty for `protocol = "fake"`.
    #[serde(default)]
    pub base_url: String,
    /// Name of the environment variable that holds the API key. Optional for
    /// `protocol = "fake"`, which never reads it.
    #[serde(default)]
    pub api_key_env: String,
    /// Model name exactly as the server expects it.
    pub model: String,
    /// Context window in tokens.
    pub context_window: u32,
    /// Whether the model accepts tool definitions and emits tool calls.
    pub supports_tools: bool,
    /// Whether the model emits reasoning content.
    pub supports_reasoning: bool,
    /// Cap on output tokens per completion. Anthropic requires one and falls
    /// back to 16384; OpenAI-style servers use their own default when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    /// What the provider charges, for cost estimates. Unset means unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_per_mtoken: Option<Price>,
    /// The JSON script a `protocol = "fake"` provider replays; a relative
    /// path starts at the working directory. Only fake providers take one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
}

impl ProviderConfig {
    /// Checks one provider entry.
    ///
    /// # Arguments
    ///
    /// * `name` - The entry's key under `[providers]`, for error messages.
    ///
    /// # Returns
    ///
    /// `Ok(())` when `base_url` is http or https, `api_key_env` and `model`
    /// are non-empty, `context_window` is positive and no price is negative.
    /// A fake provider needs a `script` instead of a base URL and a key; any
    /// other provider must not have one.
    ///
    /// # Errors
    ///
    /// The first [`ValidationError`] found.
    pub fn validate(&self, name: &str) -> Result<(), ValidationError> {
        if self.price_per_mtoken.is_some_and(|price| !price.is_valid()) {
            return Err(ValidationError::NegativePrice {
                name: name.to_owned(),
            });
        }
        let is_fake = self.protocol == Protocol::Fake;
        let has_script = self.script.as_deref().is_some_and(|s| !s.trim().is_empty());
        if is_fake != has_script {
            return Err(ValidationError::FakeScript {
                name: name.to_owned(),
            });
        }
        if is_fake {
            return self.validate_model(name);
        }
        let is_http = self.base_url.starts_with("http://") || self.base_url.starts_with("https://");
        if !is_http {
            return Err(ValidationError::InvalidBaseUrl {
                name: name.to_owned(),
                base_url: self.base_url.clone(),
            });
        }
        if self.api_key_env.trim().is_empty() {
            return Err(ValidationError::EmptyProviderField {
                name: name.to_owned(),
                field: "api_key_env",
            });
        }
        self.validate_model(name)
    }

    /// Checks the fields every protocol needs: `model` and `context_window`.
    fn validate_model(&self, name: &str) -> Result<(), ValidationError> {
        if self.model.trim().is_empty() {
            return Err(ValidationError::EmptyProviderField {
                name: name.to_owned(),
                field: "model",
            });
        }
        if self.context_window == 0 {
            return Err(ValidationError::ZeroContextWindow {
                name: name.to_owned(),
            });
        }
        Ok(())
    }
}

/// Which provider handles each job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Roles {
    /// Provider that writes code in the agent loop.
    pub coder: String,
    /// Provider that compacts context and summarizes.
    pub summarizer: String,
    /// Provider that reviews the coder's work in the loop's Review state;
    /// the coder when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer: Option<String>,
    /// Provider that plans; the coder when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planner: Option<String>,
    /// Provider that runs delegated subagents; the coder when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker: Option<String>,
}

impl Roles {
    /// The provider that plans.
    ///
    /// # Returns
    ///
    /// The `planner` provider name, or the `coder` one when none is set.
    pub fn planner_or_coder(&self) -> &str {
        self.planner.as_deref().unwrap_or(&self.coder)
    }

    /// The provider that runs delegated subagents.
    ///
    /// # Returns
    ///
    /// The `worker` provider name, or the `coder` one when none is set.
    pub fn worker_or_coder(&self) -> &str {
        self.worker.as_deref().unwrap_or(&self.coder)
    }
}

/// What one agent-loop state may spend before the session ends as incomplete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct StateBudget {
    /// Model completions allowed in the state, summed over every visit.
    pub max_turns: u32,
    /// Weighted tokens (input, output and a tenth of cached input) allowed in
    /// the state, summed over every visit.
    pub max_tokens: u64,
}

impl Default for StateBudget {
    fn default() -> Self {
        Self {
            max_turns: 100,
            max_tokens: 1_000_000,
        }
    }
}

/// Retry, timeout and budget limits. Every field has a default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LimitsConfig {
    /// Wall-clock seconds allowed for one completion, including streaming and retries.
    pub request_timeout_s: u64,
    /// How many times a request is retried on HTTP 429 or 5xx before giving up.
    pub max_retries: u32,
    /// Weighted tokens (input, output and a tenth of cached input) a session
    /// may consume before it aborts.
    pub session_tokens: u64,
    /// Budget of the Plan state.
    pub plan: StateBudget,
    /// Budget of the Build state.
    pub build: StateBudget,
    /// Budget of the Review state.
    pub review: StateBudget,
    /// Budget of the Verify state.
    pub verify: StateBudget,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            request_timeout_s: 600,
            max_retries: 4,
            session_tokens: 2_000_000,
            plan: StateBudget::default(),
            build: StateBudget::default(),
            review: StateBudget::default(),
            verify: StateBudget::default(),
        }
    }
}

/// Settings for the built-in tools.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ToolsConfig {
    /// Program behind the `shell` tool, such as `pwsh` or `bash`. `None`
    /// means PowerShell on Windows and `sh` elsewhere.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
    /// The command the Verify state runs, such as `just check`. `None` means
    /// detect it from the project files.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub check: Option<String>,
    /// Folders outside the project the file tools may read and write. The
    /// tools refuse every other path outside the project.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub extra_dirs: Vec<PathBuf>,
    /// Environment variables shell commands, hooks, checks and MCP servers
    /// receive on top of the built-in list. A name here is passed even when
    /// it looks like a secret (`*TOKEN*`, `*KEY*` and so on).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub pass_env: Vec<String>,
}

/// When the end-of-session memory summarizer runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MemoryConfig {
    /// Run the summarizer after every session, including an uneventful one
    /// with a small diff, which is skipped otherwise.
    pub always_summarize: bool,
}

/// Rules for anvil's outbound requests.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct NetworkConfig {
    /// Hosts a provider may be reached on over plain http. Every other
    /// endpoint must use https, except loopback (`localhost`, `127.0.0.1`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub allow_http_hosts: Vec<String>,
}

/// Lines a profile adds to the coder's system prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PromptConfig {
    /// One instruction per entry, shown under a "Profile notes" heading.
    pub notes: Vec<String>,
    /// Whether the Plan and Build prompts say when delegating to a subagent
    /// pays off. Only has an effect when the `delegate` tool is available.
    pub delegate_guidance: bool,
}

impl Default for PromptConfig {
    fn default() -> Self {
        Self {
            notes: Vec::new(),
            delegate_guidance: true,
        }
    }
}

/// What a call gets when no permission rule matches it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Unmatched {
    /// Run it.
    Allow,
    /// Prompt first.
    Ask,
    /// Refuse it.
    Deny,
}

/// Tool permission rules: `tool` or `tool:glob`, where the glob matches the
/// call's path or command. `deny` wins, then `allow`, then `ask`, then
/// `default`, then the tool's own default (allow for reads, ask for writes).
///
/// A project keeps them in `.anvil/permissions.toml`; a profile under
/// `~/.anvil/profiles/` carries them as its `[permissions]` table, which
/// then replaces the project file.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PermissionRules {
    /// Calls that run without asking.
    pub allow: Vec<String>,
    /// Calls that prompt first.
    pub ask: Vec<String>,
    /// Calls that are refused.
    pub deny: Vec<String>,
    /// What an unmatched call gets instead of the tool's own default.
    #[serde(rename = "default", skip_serializing_if = "Option::is_none")]
    pub unmatched: Option<Unmatched>,
    /// Tools pre-approved for every session, by name; the same as passing
    /// `--allow-tool <name>`. Kept in memory, never written back.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub allow_tools: Vec<String>,
    /// When not empty, a `shell` call must be one program from this list
    /// with plain arguments: no `&&`, `;`, `|`, redirection, substitution or
    /// their PowerShell and cmd forms. No rule, answer or `--yes` lifts it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub shell_programs: Vec<String>,
}

impl PermissionRules {
    /// Whether the table sets any rule besides `allow_tools`.
    ///
    /// # Returns
    ///
    /// `false` for a table that only pre-approves tools, which therefore
    /// should not replace the project's `.anvil/permissions.toml`.
    pub fn has_rules(&self) -> bool {
        !self.allow.is_empty()
            || !self.ask.is_empty()
            || !self.deny.is_empty()
            || self.unmatched.is_some()
            || !self.shell_programs.is_empty()
    }
}

/// The whole anvil configuration file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnvilConfig {
    /// Named model endpoints.
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
    /// Which provider does which job.
    pub roles: Roles,
    /// Retry, timeout and budget limits.
    #[serde(default)]
    pub limits: LimitsConfig,
    /// Built-in tool settings.
    #[serde(default)]
    pub tools: ToolsConfig,
    /// When the end-of-session memory summarizer runs.
    #[serde(default)]
    pub memory: MemoryConfig,
    /// Extra lines for the coder's system prompt.
    #[serde(default)]
    pub prompt: PromptConfig,
    /// Tool permission rules that replace the project's `.anvil/permissions.toml`.
    /// Normally set by a profile, not by hand.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permissions: Option<PermissionRules>,
    /// Rules for outbound requests.
    #[serde(default)]
    pub network: NetworkConfig,
    /// How the `shell` tool runs commands: its mode, allowlist and executor.
    #[serde(default)]
    pub shell: ShellConfig,
}

impl AnvilConfig {
    /// Checks every provider, that every role names a declared provider,
    /// and that the `[shell]` table's keys fit together.
    ///
    /// # Returns
    ///
    /// `Ok(())` when all providers validate, `coder`, `summarizer` and
    /// (if set) `reviewer`, `planner` and `worker` each name a key in
    /// `providers`, and `[shell]` is consistent.
    ///
    /// # Errors
    ///
    /// The first [`ValidationError`] found, providers first.
    pub fn validate(&self) -> Result<(), ValidationError> {
        for (name, provider) in &self.providers {
            provider.validate(name)?;
        }
        self.check_role("coder", &self.roles.coder)?;
        self.check_role("summarizer", &self.roles.summarizer)?;
        if let Some(reviewer) = &self.roles.reviewer {
            self.check_role("reviewer", reviewer)?;
        }
        if let Some(planner) = &self.roles.planner {
            self.check_role("planner", planner)?;
        }
        if let Some(worker) = &self.roles.worker {
            self.check_role("worker", worker)?;
        }
        self.shell
            .validate()
            .map_err(|reason| ValidationError::Shell { reason })
    }

    /// Fails unless `provider` is a key in `providers`.
    fn check_role(&self, role: &'static str, provider: &str) -> Result<(), ValidationError> {
        if self.providers.contains_key(provider) {
            Ok(())
        } else {
            Err(ValidationError::UnknownProvider {
                role,
                provider: provider.to_owned(),
            })
        }
    }
}

/// Where the merged configuration came from.
#[derive(Debug, Clone)]
pub struct LoadedConfig {
    /// The merged, validated configuration.
    pub config: AnvilConfig,
    /// Files that contributed, global first.
    pub sources: Vec<PathBuf>,
    /// The merged TOML, for `anvil config show`.
    pub merged: toml::Table,
}

/// The user's or the repository's configuration path.
///
/// # Arguments
///
/// * `home` - The home directory, if known.
/// * `global` - When true, `~/.anvil/config.toml`; otherwise `.anvil/config.toml`
///   relative to the current directory, which holds only tightening rules.
///
/// # Returns
///
/// The path, or `None` when the global path is wanted but `home` is unknown.
pub fn config_path(home: Option<&Path>, global: bool) -> Option<PathBuf> {
    let base = if global {
        home?.to_path_buf()
    } else {
        PathBuf::new()
    };
    Some(base.join(CONFIG_DIR).join(CONFIG_FILE))
}

/// Loads `~/.anvil/config.toml`.
///
/// # Arguments
///
/// * `home` - The home directory, if known.
///
/// # Returns
///
/// The configuration.
///
/// # Errors
///
/// When the file does not exist, does not parse, or validation fails.
pub fn load(home: Option<&Path>) -> Result<LoadedConfig, ConfigError> {
    load_profile(home, None)
}

/// The path of a named profile.
///
/// # Arguments
///
/// * `home` - The home directory.
/// * `name` - The profile name, as given to `--profile`.
///
/// # Returns
///
/// `<home>/.anvil/profiles/<name>.toml`.
pub fn profile_path(home: &Path, name: &str) -> PathBuf {
    home.join(PROFILES_DIR).join(format!("{name}.toml"))
}

/// Loads `~/.anvil/config.toml`, then a named profile on top.
///
/// A profile is a second layer, `~/.anvil/profiles/<name>.toml`, with the
/// same shape. Tables merge key by key, so a profile can override one
/// provider field or one limit without repeating the rest; scalars and
/// arrays replace. Unlike the config file a profile is required when named:
/// running with a profile that does not exist would silently drop the
/// restrictions the caller asked for.
///
/// # Arguments
///
/// * `home` - The home directory, if known.
/// * `profile` - The profile name, if any.
///
/// # Returns
///
/// The merged configuration.
///
/// # Errors
///
/// [`ConfigError::ProfileNotFound`] when a profile is named but missing (or
/// `home` is unknown), and the errors of [`load`].
pub fn load_profile(
    home: Option<&Path>,
    profile: Option<&str>,
) -> Result<LoadedConfig, ConfigError> {
    let mut merged = toml::Table::new();
    let mut sources = Vec::new();
    let profile_file = match profile {
        Some(name) => {
            let path = match home {
                Some(home) => profile_path(home, name),
                None => PathBuf::from("~")
                    .join(PROFILES_DIR)
                    .join(format!("{name}.toml")),
            };
            if home.is_none() || !path.is_file() {
                return Err(ConfigError::ProfileNotFound { path });
            }
            Some(path)
        }
        None => None,
    };
    let candidates = [
        home.map(|home| home.join(CONFIG_DIR).join(CONFIG_FILE)),
        profile_file,
    ];
    for path in candidates.into_iter().flatten() {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let table: toml::Table = toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.clone(),
            source,
        })?;
        reject_removed_sections(&table, &path)?;
        merge_tables(&mut merged, table);
        sources.push(path);
    }
    if sources.is_empty() {
        return Err(ConfigError::NotFound);
    }
    let config: AnvilConfig = merged
        .clone()
        .try_into()
        .map_err(|source| ConfigError::Shape {
            sources: describe(&sources),
            source,
        })?;
    config.validate().map_err(|source| ConfigError::Invalid {
        sources: describe(&sources),
        source,
    })?;
    Ok(LoadedConfig {
        config,
        sources,
        merged,
    })
}

/// Merges `overlay` into `base`: nested tables recurse, everything else replaces.
///
/// # Arguments
///
/// * `base` - The table that receives the merge.
/// * `overlay` - The table whose values win.
pub fn merge_tables(base: &mut toml::Table, overlay: toml::Table) {
    for (key, value) in overlay {
        match (base.get_mut(&key), value) {
            (Some(toml::Value::Table(existing)), toml::Value::Table(incoming)) => {
                merge_tables(existing, incoming);
            }
            (_, value) => {
                base.insert(key, value);
            }
        }
    }
}

/// Fails when `table` sets a section listed in [`REMOVED_SECTIONS`].
fn reject_removed_sections(table: &toml::Table, path: &Path) -> Result<(), ConfigError> {
    for (section, reason) in REMOVED_SECTIONS {
        if table.contains_key(section) {
            return Err(ConfigError::RemovedSection {
                path: path.to_path_buf(),
                section,
                reason,
            });
        }
    }
    Ok(())
}

/// Lists the source paths for error messages.
fn describe(sources: &[PathBuf]) -> String {
    sources
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(" + ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, text: &str) {
        let path = dir.join(CONFIG_DIR).join(CONFIG_FILE);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, text).expect("write");
    }

    #[test]
    fn removed_sections_fail_with_the_reason() {
        let home = tempfile::tempdir().expect("home");
        for (section, expected) in [
            ("update", "self-update was removed"),
            ("notify", "webhook"),
            ("tickets", "tickets were removed"),
            ("telemetry", "ticket signal thresholds"),
            ("integrations", "herdr reporting was removed"),
        ] {
            let text = format!("{SETUP_PROVIDERS}\n[{section}]\n");
            write(home.path(), &text);
            let error = load(Some(home.path())).expect_err("removed section");
            assert!(
                matches!(error, ConfigError::RemovedSection { .. }),
                "{section}: {error:?}"
            );
            assert!(error.to_string().contains(expected), "{error}");
        }
    }

    #[test]
    fn setup_providers_parse_and_validate() {
        let config: AnvilConfig = toml::from_str(SETUP_PROVIDERS).expect("parses");
        config.validate().expect("validates");
        assert_eq!(
            config.providers.keys().collect::<Vec<_>>(),
            ["endpoint"],
            "setup writes one example provider"
        );
        let endpoint = &config.providers["endpoint"];
        assert_eq!(endpoint.protocol, Protocol::Openai);
        assert!(
            endpoint.base_url.starts_with("https://")
                && !endpoint.base_url.contains("anthropic.com")
                && !endpoint.base_url.contains("openai.com"),
            "a self-hosted example, not a hosted API: {}",
            endpoint.base_url
        );
        assert_eq!(config.roles.coder, "endpoint");
        assert_eq!(config.roles.summarizer, "endpoint");
        assert_eq!(config.roles.reviewer, None);
        let template: AnvilConfig = toml::from_str(CONFIG_TEMPLATE).expect("parses");
        assert_eq!(
            template.providers.keys().collect::<Vec<_>>(),
            ["endpoint"],
            "`anvil config init --global` writes the same single example"
        );
        let with_optional = format!("{SETUP_PROVIDERS}{OPTIONAL_SECTIONS}");
        let same: AnvilConfig = toml::from_str(&with_optional).expect("parses with comments");
        assert_eq!(same, config);
    }

    #[test]
    fn profile_overrides_the_config_key_by_key_and_must_exist() {
        let home = tempfile::tempdir().expect("home");
        write(home.path(), CONFIG_TEMPLATE);
        let profile = profile_path(home.path(), "ci");
        std::fs::create_dir_all(profile.parent().expect("parent")).expect("mkdir");
        std::fs::write(
            &profile,
            "[providers.endpoint]\nmodel = \"local-model\"\n[limits]\nsession_tokens = 7\n[permissions]\nallow = [\"shell:cargo *\"]\ndefault = \"deny\"\n",
        )
        .expect("write");
        let loaded = load_profile(Some(home.path()), Some("ci")).expect("loads");
        let endpoint = &loaded.config.providers["endpoint"];
        assert_eq!(endpoint.model, "local-model");
        assert_eq!(endpoint.context_window, 32_768, "config field kept");
        assert_eq!(loaded.sources.len(), 2);
        assert_eq!(loaded.sources[1], profile);
        assert_eq!(loaded.config.limits.session_tokens, 7);
        let rules = loaded
            .config
            .permissions
            .expect("permissions from the profile");
        assert_eq!(rules.allow, vec!["shell:cargo *".to_owned()]);
        assert_eq!(rules.unmatched, Some(Unmatched::Deny));
        assert!(
            load(Some(home.path()))
                .expect("no profile")
                .config
                .permissions
                .is_none()
        );

        let error = load_profile(Some(home.path()), Some("work")).expect_err("missing");
        assert!(
            matches!(&error, ConfigError::ProfileNotFound { path } if path.ends_with("work.toml")),
            "{error}"
        );
        assert!(matches!(
            load_profile(None, Some("work")),
            Err(ConfigError::ProfileNotFound { .. })
        ));
    }

    #[test]
    fn the_shipped_strict_profile_loads_as_a_profile() {
        let home = tempfile::tempdir().expect("home");
        write(home.path(), CONFIG_TEMPLATE);
        let profile = profile_path(home.path(), "strict");
        std::fs::create_dir_all(profile.parent().expect("parent")).expect("mkdir");
        std::fs::write(&profile, include_str!("../../../profiles/strict.toml")).expect("write");
        let loaded = load_profile(Some(home.path()), Some("strict")).expect("loads");
        let shell = &loaded.config.shell;
        assert_eq!(shell.mode, Some(crate::shell::ShellMode::Allowlist));
        assert!(shell.allow.contains(&"cargo test *".to_owned()));
        for broad in ["cargo *", "git *", "just *", "rustc *"] {
            assert!(
                !shell.allow.contains(&broad.to_owned()),
                "{broad} lets an allowed program run any program"
            );
        }
        let rules = loaded.config.permissions.expect("a [permissions] table");
        assert_eq!(rules.unmatched, Some(Unmatched::Ask));
        assert!(rules.shell_programs.is_empty(), "folded into [shell] allow");
        assert!(!rules.allow.iter().any(|rule| rule.starts_with("shell")));
        assert!(rules.has_rules());
    }

    #[test]
    fn permission_rules_reject_unknown_keys_and_round_trip() {
        let rules: PermissionRules =
            toml::from_str("allow = [\"shell\"]\ndefault = \"ask\"\n").expect("parses");
        assert_eq!(rules.unmatched, Some(Unmatched::Ask));
        assert!(toml::from_str::<PermissionRules>("block = []").is_err());
        let text = toml::to_string(&PermissionRules::default()).expect("serializes");
        assert!(!text.contains("default"), "{text}");
        assert!(!text.contains("allow_tools"), "{text}");
        assert!(!PermissionRules::default().has_rules());
    }

    #[test]
    fn permission_rules_accept_allow_tools_without_counting_them_as_rules() {
        let rules: PermissionRules =
            toml::from_str("allow_tools = [\"edit_file\", \"write_file\"]\n").expect("parses");
        assert_eq!(rules.allow_tools, vec!["edit_file", "write_file"]);
        assert!(!rules.has_rules());
        let config: PermissionRules =
            toml::from_str("allow = [\"shell\"]\nallow_tools = [\"edit_file\"]\n").expect("parses");
        assert!(config.has_rules());
    }

    #[test]
    fn missing_everything_and_invalid_files_fail_clearly() {
        let home = tempfile::tempdir().expect("home");
        let error = load(Some(home.path())).expect_err("no config");
        assert!(error.to_string().contains("anvil config init --global"));
        assert!(error.to_string().contains("anvil setup"));
        assert!(matches!(load(None), Err(ConfigError::NotFound)));

        write(
            home.path(),
            "[roles]\ncoder = \"nope\"\nsummarizer = \"nope\"\n",
        );
        let error = load(Some(home.path())).expect_err("unknown provider");
        assert!(matches!(error, ConfigError::Invalid { .. }), "{error}");
        assert!(error.to_string().contains("validating"), "{error}");

        write(home.path(), "not = [toml");
        assert!(matches!(
            load(Some(home.path())),
            Err(ConfigError::Parse { .. })
        ));
    }

    #[test]
    fn merge_replaces_arrays_and_scalars() {
        let mut base: toml::Table =
            toml::from_str("a = [1]\nb = 1\n[t]\nx = 1\ny = 2").expect("toml");
        let overlay: toml::Table = toml::from_str("a = [2, 3]\n[t]\ny = 9\nz = 3").expect("toml");
        merge_tables(&mut base, overlay);
        assert_eq!(base["a"], toml::Value::Array(vec![2.into(), 3.into()]));
        assert_eq!(base["b"], toml::Value::Integer(1));
        assert_eq!(base["t"]["x"], toml::Value::Integer(1));
        assert_eq!(base["t"]["y"], toml::Value::Integer(9));
        assert_eq!(base["t"]["z"], toml::Value::Integer(3));
    }

    #[test]
    fn config_path_is_global_or_relative() {
        let home = Path::new("/h");
        assert_eq!(
            config_path(Some(home), true),
            Some(home.join(".anvil").join("config.toml"))
        );
        assert_eq!(config_path(None, true), None);
        assert_eq!(
            config_path(None, false),
            Some(Path::new(".anvil").join("config.toml"))
        );
    }

    fn template() -> AnvilConfig {
        toml::from_str(CONFIG_TEMPLATE).expect("template parses")
    }

    #[test]
    fn template_parses_and_validates() {
        let config = template();
        config.validate().expect("template validates");
        assert_eq!(config.roles.coder, "endpoint");
        assert_eq!(config.roles.reviewer, None);
        let endpoint = &config.providers["endpoint"];
        assert_eq!(endpoint.protocol, Protocol::Openai);
        assert_eq!(endpoint.context_window, 32_768);
        assert!(endpoint.supports_tools && !endpoint.supports_reasoning);
    }

    #[test]
    fn setup_and_config_init_write_the_same_example_provider() {
        let setup: AnvilConfig = toml::from_str(SETUP_PROVIDERS).expect("parses");
        assert_eq!(setup.providers, template().providers);
    }

    #[test]
    fn toml_round_trip() {
        let mut config = template();
        config.roles.reviewer = Some("endpoint".to_owned());
        let text = toml::to_string_pretty(&config).expect("serializes");
        let back: AnvilConfig = toml::from_str(&text).expect("parses back");
        assert_eq!(back, config);
    }

    #[test]
    fn protocol_spellings_are_lowercase() {
        assert_eq!(
            serde_json::to_string(&Protocol::Anthropic).expect("serializes"),
            "\"anthropic\""
        );
        let parsed: Protocol = serde_json::from_str("\"openai\"").expect("parses");
        assert_eq!(parsed, Protocol::Openai);
        let parsed: Protocol = serde_json::from_str("\"fake\"").expect("parses");
        assert_eq!(parsed, Protocol::Fake);
    }

    #[test]
    fn a_fake_provider_needs_a_script_and_nothing_else() {
        let fake: ProviderConfig = toml::from_str(
            "protocol = \"fake\"\nmodel = \"m\"\ncontext_window = 1000\nsupports_tools = true\nsupports_reasoning = false\nscript = \"s.json\"\n",
        )
        .expect("parses without base_url or api_key_env");
        assert_eq!(fake.validate("fake"), Ok(()));
        let mut scriptless = fake.clone();
        scriptless.script = None;
        assert!(matches!(
            scriptless.validate("fake"),
            Err(ValidationError::FakeScript { .. })
        ));
        let mut real = template()
            .providers
            .into_values()
            .next()
            .expect("a provider");
        real.script = Some("s.json".to_owned());
        assert!(matches!(
            real.validate("real"),
            Err(ValidationError::FakeScript { .. })
        ));
    }

    #[test]
    fn role_must_name_a_declared_provider() {
        let mut config = template();
        config.roles.summarizer = "missing".to_owned();
        assert_eq!(
            config.validate(),
            Err(ValidationError::UnknownProvider {
                role: "summarizer",
                provider: "missing".to_owned()
            })
        );
    }

    #[test]
    fn provider_fields_are_checked() {
        let mut config = template();
        config
            .providers
            .get_mut("endpoint")
            .expect("exists")
            .base_url = "ftp://x".to_owned();
        assert!(matches!(
            config.validate(),
            Err(ValidationError::InvalidBaseUrl { .. })
        ));

        let mut config = template();
        config
            .providers
            .get_mut("endpoint")
            .expect("exists")
            .context_window = 0;
        assert!(matches!(
            config.validate(),
            Err(ValidationError::ZeroContextWindow { .. })
        ));

        let mut config = template();
        config.providers.get_mut("endpoint").expect("exists").model = String::new();
        assert_eq!(
            config.validate(),
            Err(ValidationError::EmptyProviderField {
                name: "endpoint".to_owned(),
                field: "model"
            })
        );
    }

    #[test]
    fn the_worker_is_optional_defaults_to_the_coder_and_must_exist() {
        let mut config = template();
        assert_eq!(config.roles.worker, None);
        assert_eq!(config.roles.worker_or_coder(), config.roles.coder);
        config.roles.worker = Some("endpoint".to_owned());
        config.validate().expect("a declared worker is valid");
        assert_eq!(config.roles.worker_or_coder(), "endpoint");
        config.roles.worker = Some("nope".to_owned());
        assert_eq!(
            config.validate(),
            Err(ValidationError::UnknownProvider {
                role: "worker",
                provider: "nope".to_owned()
            })
        );
    }

    #[test]
    fn delegate_guidance_defaults_on_and_can_be_turned_off() {
        assert!(template().prompt.delegate_guidance);
        let text = format!(
            "{CONFIG_TEMPLATE}
[prompt]
delegate_guidance = false
"
        );
        let off: AnvilConfig = toml::from_str(&text).expect("parses");
        assert!(!off.prompt.delegate_guidance);
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let text = format!("{CONFIG_TEMPLATE}\n[roles]\nplanner = \"endpoint\"\n");
        assert!(toml::from_str::<AnvilConfig>(&text).is_err());
    }

    #[test]
    fn limits_default_when_absent() {
        let config = template();
        assert_eq!(config.limits, LimitsConfig::default());
        assert_eq!(config.limits.request_timeout_s, 600);
        assert_eq!(config.limits.max_retries, 4);
        assert_eq!(config.limits.session_tokens, 2_000_000);
        assert_eq!(config.providers["endpoint"].max_output_tokens, None);
    }

    #[test]
    fn limits_accept_partial_tables_and_reject_unknown_keys() {
        let text = format!("{CONFIG_TEMPLATE}\n[limits]\nmax_retries = 1\n");
        let config: AnvilConfig = toml::from_str(&text).expect("parses");
        assert_eq!(config.limits.max_retries, 1);
        assert_eq!(config.limits.request_timeout_s, 600);

        let text = format!("{CONFIG_TEMPLATE}\n[limits]\nmax_turns = 1\n");
        assert!(toml::from_str::<AnvilConfig>(&text).is_err());
    }

    #[test]
    fn state_budgets_accept_partial_tables() {
        let text = format!("{CONFIG_TEMPLATE}\n[limits.build]\nmax_turns = 7\n");
        let config: AnvilConfig = toml::from_str(&text).expect("parses");
        assert_eq!(config.limits.build.max_turns, 7);
        assert_eq!(config.limits.build.max_tokens, 1_000_000);
        assert_eq!(config.limits.plan, StateBudget::default());

        let text = format!("{CONFIG_TEMPLATE}\n[limits.build]\nmax_rounds = 7\n");
        assert!(toml::from_str::<AnvilConfig>(&text).is_err());
    }

    #[test]
    fn price_is_optional_and_costs_tokens() {
        assert_eq!(template().providers["endpoint"].price_per_mtoken, None);
        let text = format!(
            "{CONFIG_TEMPLATE}\n[providers.endpoint.price_per_mtoken]\ninput = 3.0\noutput = 15.0\n"
        );
        let config: AnvilConfig = toml::from_str(&text).expect("parses");
        let price = config.providers["endpoint"]
            .price_per_mtoken
            .expect("price set");
        assert!((price.cost(1_000_000, 1_000_000, 1_000_000) - 21.0).abs() < 1e-9);
        let discounted = Price {
            cached: Some(0.5),
            ..price
        };
        assert!((discounted.cost(0, 0, 2_000_000) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn negative_prices_are_rejected() {
        let mut config = template();
        config
            .providers
            .get_mut("endpoint")
            .expect("exists")
            .price_per_mtoken = Some(Price {
            input: -1.0,
            output: 1.0,
            cached: None,
        });
        assert_eq!(
            config.validate(),
            Err(ValidationError::NegativePrice {
                name: "endpoint".to_owned()
            })
        );
    }

    #[test]
    fn memory_table_defaults_to_skipping_and_can_force_the_summarizer() {
        assert!(!template().memory.always_summarize);
        let text = format!("{CONFIG_TEMPLATE}\n[memory]\nalways_summarize = true\n");
        let config: AnvilConfig = toml::from_str(&text).expect("parses");
        assert!(config.memory.always_summarize);
        let unknown = format!("{CONFIG_TEMPLATE}\n[memory]\nsometimes = true\n");
        assert!(toml::from_str::<AnvilConfig>(&unknown).is_err());
    }

    #[test]
    fn tools_shell_is_optional() {
        assert_eq!(template().tools.shell, None);
        let text = format!("{CONFIG_TEMPLATE}\n[tools]\nshell = \"bash\"\n");
        let config: AnvilConfig = toml::from_str(&text).expect("parses");
        assert_eq!(config.tools.shell.as_deref(), Some("bash"));
    }
}
