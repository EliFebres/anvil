//! Provider and role configuration: which model does which job.
//!
//! Lives at `~/.forge/config.toml` (global) and `.forge/config.toml`
//! (project, overrides global). Models are swapped by editing these files,
//! never by changing code.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::{ConfigError, ValidationError};

/// Directory that holds forge configuration, both globally and per project.
pub const CONFIG_DIR: &str = ".forge";

/// File name of the configuration file inside [`CONFIG_DIR`].
pub const CONFIG_FILE: &str = "config.toml";

/// The commented template written by `forge config init`.
pub const CONFIG_TEMPLATE: &str = r#"# forge configuration.
# Global: ~/.forge/config.toml   Project: .forge/config.toml (project overrides global).
# Swap models by editing this file; no code changes are needed.

[providers.runpod]
protocol = "openai"          # "openai" or "anthropic"
base_url = "https://<pod-id>-8000.proxy.runpod.net/v1"
api_key_env = "FORGE_RUNPOD_KEY"   # environment variable that holds the API key
model = "<model name as served>"
context_window = 131072
supports_tools = true
supports_reasoning = true
# max_output_tokens = 16384   # optional; Anthropic requires one and defaults to 16384
# USD per million tokens, used by `forge stats` to estimate cost. Optional;
# `cached` falls back to `input`.
# price_per_mtoken = { input = 3.0, output = 15.0, cached = 0.3 }

# Each role names a provider above, so the coder, the summarizer and (in the
# factory) the reviewer can each be a different model.
[roles]
coder = "runpod"
summarizer = "runpod"
# reviewer = "runpod"

# Optional. The shell program behind the `shell` tool: "powershell", "pwsh",
# "sh", "bash", "zsh" or "cmd". Default: PowerShell on Windows, sh elsewhere.
# [tools]
# shell = "pwsh"

# Every key is optional; these are the defaults.
# [limits]
# request_timeout_s = 600      # wall clock for one completion, streaming included
# max_retries = 4              # on HTTP 429 and 5xx, with exponential backoff
# session_tokens = 2000000     # abort the session once this many tokens are used
#
# Per-state budgets (plan, build, review, optimize, verify), summed over every
# visit to the state. Exceeding one ends the session as incomplete.
# [limits.build]
# max_turns = 100
# max_tokens = 1000000

# Lines that turn provider statistics into tickets. Defaults shown.
# [telemetry]
# slow_median_ms = 60000       # median completion latency over the last 20 sessions
# error_rate_percent = 20      # provider errors as a share of completions

# Where `forge ticket submit` pushes the `tickets/<id>` branch.
# [tickets]
# remote = "origin"
"#;

/// The provider entries and roles `forge setup` adds to `~/.forge/config.toml`.
///
/// Model ids and prices were taken from each vendor's own pages on
/// 2026-10-30; `forge setup` prints them so they can be changed.
pub const SETUP_PROVIDERS: &str = r#"# forge configuration, written by `forge setup`.
# Global: ~/.forge/config.toml   Project: .forge/config.toml (project overrides global).
# Keys never live here: each provider names the environment variable that
# holds its key, and `forge` loads ~/.forge/.env (then .forge/.env) at start.

# Claude Opus 5.5: platform.claude.com -> pricing (cache reads 0.20).
[providers.anthropic]
protocol = "anthropic"
base_url = "https://api.anthropic.com/v1"
api_key_env = "ANTHROPIC_API_KEY"
model = "claude-opus-5-5"
context_window = 1000000
supports_tools = true
supports_reasoning = true
price_per_mtoken = { input = 4.0, output = 20.0, cached = 0.2 }

# GPT-6 Astra: developers.openai.com -> pricing (standard tier, <= 272K input).
[providers.openai]
protocol = "openai"
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"
model = "gpt-6-astra"
context_window = 1048576
supports_tools = true
supports_reasoning = true
price_per_mtoken = { input = 10.0, output = 50.0 }

# Gemini 4 Argon through Gemini's OpenAI-compatible endpoint: ai.google.dev -> pricing.
# Launch price until 2026-11-30; from 2026-12-01 it is input = 4.0, output = 20.0.
[providers.google]
protocol = "openai"
base_url = "https://generativelanguage.googleapis.com/v1beta/openai"
api_key_env = "GEMINI_API_KEY"
model = "gemini-4-argon"
context_window = 1000000
supports_tools = true
supports_reasoning = true
price_per_mtoken = { input = 2.0, output = 10.0 }

# Each role names a provider above.
[roles]
coder = "anthropic"
reviewer = "openai"
summarizer = "anthropic"
"#;

/// The optional, commented sections `forge setup` appends to a new config file.
pub const OPTIONAL_SECTIONS: &str = r#"
# Optional. The shell program behind the `shell` tool: "powershell", "pwsh",
# "sh", "bash", "zsh" or "cmd". Default: PowerShell on Windows, sh elsewhere.
# [tools]
# shell = "pwsh"

# Every key is optional; these are the defaults.
# [limits]
# request_timeout_s = 600      # wall clock for one completion, streaming included
# max_retries = 4              # on HTTP 429 and 5xx, with exponential backoff
# session_tokens = 2000000     # abort the session once this many tokens are used
#
# Per-state budgets (plan, build, review, optimize, verify), summed over every
# visit to the state. Exceeding one ends the session as incomplete.
# [limits.build]
# max_turns = 100
# max_tokens = 1000000

# Lines that turn provider statistics into tickets. Defaults shown.
# [telemetry]
# slow_median_ms = 60000       # median completion latency over the last 20 sessions
# error_rate_percent = 20      # provider errors as a share of completions

# Where `forge ticket submit` pushes the `tickets/<id>` branch.
# [tickets]
# remote = "origin"
"#;

/// Wire protocol a provider speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    /// OpenAI chat-completions style API.
    Openai,
    /// Anthropic messages style API.
    Anthropic,
}

/// What a provider charges, in USD per million tokens.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    /// Which API shape to speak.
    pub protocol: Protocol,
    /// Base URL of the API, including any version prefix such as `/v1`.
    pub base_url: String,
    /// Name of the environment variable that holds the API key.
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Roles {
    /// Provider that writes code in the agent loop.
    pub coder: String,
    /// Provider that compacts context and summarizes.
    pub summarizer: String,
    /// Provider that reviews factory pull requests. Only the factory needs it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer: Option<String>,
}

/// What one agent-loop state may spend before the session ends as incomplete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct StateBudget {
    /// Model completions allowed in the state, summed over every visit.
    pub max_turns: u32,
    /// Tokens (input, output and cached) allowed in the state, summed over every visit.
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct LimitsConfig {
    /// Wall-clock seconds allowed for one completion, including streaming and retries.
    pub request_timeout_s: u64,
    /// How many times a request is retried on HTTP 429 or 5xx before giving up.
    pub max_retries: u32,
    /// Tokens (input, output and cached) a session may consume before it aborts.
    pub session_tokens: u64,
    /// Budget of the Plan state.
    pub plan: StateBudget,
    /// Budget of the Build state.
    pub build: StateBudget,
    /// Budget of the Review state.
    pub review: StateBudget,
    /// Budget of the Optimize state.
    pub optimize: StateBudget,
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
            optimize: StateBudget::default(),
            verify: StateBudget::default(),
        }
    }
}

/// Settings for the built-in tools.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct ToolsConfig {
    /// Program behind the `shell` tool, such as `pwsh` or `bash`. `None`
    /// means PowerShell on Windows and `sh` elsewhere.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
}

/// Lines above which provider statistics become tickets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct TelemetryConfig {
    /// Median completion latency, in milliseconds, over the last 20 sessions
    /// above which a provider counts as slow.
    pub slow_median_ms: u64,
    /// Provider errors as a percentage of completions over the last 20
    /// sessions above which a provider counts as failing.
    pub error_rate_percent: u8,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            slow_median_ms: 60_000,
            error_rate_percent: 20,
        }
    }
}

/// Where submitted tickets go.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct TicketsConfig {
    /// The git remote `forge ticket submit` pushes `tickets/<id>` branches to.
    pub remote: String,
}

impl Default for TicketsConfig {
    fn default() -> Self {
        Self {
            remote: "origin".to_owned(),
        }
    }
}

/// The whole forge configuration file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ForgeConfig {
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
    /// Lines that turn provider statistics into tickets.
    #[serde(default)]
    pub telemetry: TelemetryConfig,
    /// Where submitted tickets go.
    #[serde(default)]
    pub tickets: TicketsConfig,
}

impl ForgeConfig {
    /// Checks every provider and that every role names a declared provider.
    ///
    /// # Returns
    ///
    /// `Ok(())` when all providers validate and `coder`, `summarizer` and
    /// (if set) `reviewer` each name a key in `providers`.
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
        Ok(())
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
    pub config: ForgeConfig,
    /// Files that contributed, global first.
    pub sources: Vec<PathBuf>,
    /// The merged TOML, for `forge config show`.
    pub merged: toml::Table,
}

/// The global or project configuration path.
///
/// # Arguments
///
/// * `home` - The home directory, if known.
/// * `global` - When true, `~/.forge/config.toml`; otherwise `.forge/config.toml`
///   relative to the current directory.
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

/// Loads `~/.forge/config.toml` and `.forge/config.toml` and merges them.
///
/// Tables merge key by key, so a project file can override one provider
/// field or one limit without repeating the rest. Scalars and arrays from
/// the project file replace the global ones.
///
/// # Arguments
///
/// * `home` - The home directory, if known.
/// * `cwd` - The project root.
///
/// # Returns
///
/// The merged configuration.
///
/// # Errors
///
/// When neither file exists, a file does not parse, or validation fails.
pub fn load(home: Option<&Path>, cwd: &Path) -> Result<LoadedConfig, ConfigError> {
    let mut merged = toml::Table::new();
    let mut sources = Vec::new();
    let candidates = [
        home.map(|home| home.join(CONFIG_DIR).join(CONFIG_FILE)),
        Some(cwd.join(CONFIG_DIR).join(CONFIG_FILE)),
    ];
    for path in candidates.into_iter().flatten() {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let table: toml::Table = toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.clone(),
            source,
        })?;
        merge_tables(&mut merged, table);
        sources.push(path);
    }
    if sources.is_empty() {
        return Err(ConfigError::NotFound);
    }
    let config: ForgeConfig = merged
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
    fn setup_providers_parse_and_validate() {
        let config: ForgeConfig = toml::from_str(SETUP_PROVIDERS).expect("parses");
        config.validate().expect("validates");
        assert_eq!(config.roles.coder, "anthropic");
        assert_eq!(config.roles.reviewer.as_deref(), Some("openai"));
        assert_eq!(config.providers["google"].protocol, Protocol::Openai);
        assert_eq!(config.providers["anthropic"].protocol, Protocol::Anthropic);
        let with_optional = format!("{SETUP_PROVIDERS}{OPTIONAL_SECTIONS}");
        let same: ForgeConfig = toml::from_str(&with_optional).expect("parses with comments");
        assert_eq!(same, config);
    }

    #[test]
    fn project_overrides_global_key_by_key() {
        let home = tempfile::tempdir().expect("home");
        let cwd = tempfile::tempdir().expect("cwd");
        write(home.path(), CONFIG_TEMPLATE);
        write(
            cwd.path(),
            "[providers.runpod]\nmodel = \"local-model\"\n\n[limits]\nmax_retries = 1\n",
        );
        let loaded = load(Some(home.path()), cwd.path()).expect("loads");
        assert_eq!(loaded.sources.len(), 2);
        let runpod = &loaded.config.providers["runpod"];
        assert_eq!(runpod.model, "local-model");
        assert_eq!(runpod.context_window, 131_072, "global field kept");
        assert_eq!(loaded.config.limits.max_retries, 1);
        assert_eq!(loaded.config.limits.request_timeout_s, 600);
        assert_eq!(loaded.config.roles.coder, "runpod");
    }

    #[test]
    fn missing_everything_and_invalid_files_fail_clearly() {
        let cwd = tempfile::tempdir().expect("cwd");
        let error = load(None, cwd.path()).expect_err("no config");
        assert!(error.to_string().contains("forge config init"));
        assert!(error.to_string().contains("forge setup"));

        write(
            cwd.path(),
            "[roles]\ncoder = \"nope\"\nsummarizer = \"nope\"\n",
        );
        let error = load(None, cwd.path()).expect_err("unknown provider");
        assert!(matches!(error, ConfigError::Invalid { .. }), "{error}");
        assert!(error.to_string().contains("validating"), "{error}");

        write(cwd.path(), "not = [toml");
        assert!(matches!(
            load(None, cwd.path()),
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
            Some(home.join(".forge").join("config.toml"))
        );
        assert_eq!(config_path(None, true), None);
        assert_eq!(
            config_path(None, false),
            Some(Path::new(".forge").join("config.toml"))
        );
    }

    fn template() -> ForgeConfig {
        toml::from_str(CONFIG_TEMPLATE).expect("template parses")
    }

    #[test]
    fn template_parses_and_validates() {
        let config = template();
        config.validate().expect("template validates");
        assert_eq!(config.roles.coder, "runpod");
        assert_eq!(config.roles.reviewer, None);
        let runpod = &config.providers["runpod"];
        assert_eq!(runpod.protocol, Protocol::Openai);
        assert_eq!(runpod.context_window, 131_072);
        assert!(runpod.supports_tools && runpod.supports_reasoning);
    }

    #[test]
    fn toml_round_trip() {
        let mut config = template();
        config.roles.reviewer = Some("runpod".to_owned());
        let text = toml::to_string_pretty(&config).expect("serializes");
        let back: ForgeConfig = toml::from_str(&text).expect("parses back");
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
        config.providers.get_mut("runpod").expect("exists").base_url = "ftp://x".to_owned();
        assert!(matches!(
            config.validate(),
            Err(ValidationError::InvalidBaseUrl { .. })
        ));

        let mut config = template();
        config
            .providers
            .get_mut("runpod")
            .expect("exists")
            .context_window = 0;
        assert!(matches!(
            config.validate(),
            Err(ValidationError::ZeroContextWindow { .. })
        ));

        let mut config = template();
        config.providers.get_mut("runpod").expect("exists").model = String::new();
        assert_eq!(
            config.validate(),
            Err(ValidationError::EmptyProviderField {
                name: "runpod".to_owned(),
                field: "model"
            })
        );
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let text = format!("{CONFIG_TEMPLATE}\n[roles]\nplanner = \"runpod\"\n");
        assert!(toml::from_str::<ForgeConfig>(&text).is_err());
    }

    #[test]
    fn limits_default_when_absent() {
        let config = template();
        assert_eq!(config.limits, LimitsConfig::default());
        assert_eq!(config.limits.request_timeout_s, 600);
        assert_eq!(config.limits.max_retries, 4);
        assert_eq!(config.limits.session_tokens, 2_000_000);
        assert_eq!(config.providers["runpod"].max_output_tokens, None);
    }

    #[test]
    fn limits_accept_partial_tables_and_reject_unknown_keys() {
        let text = format!("{CONFIG_TEMPLATE}\n[limits]\nmax_retries = 1\n");
        let config: ForgeConfig = toml::from_str(&text).expect("parses");
        assert_eq!(config.limits.max_retries, 1);
        assert_eq!(config.limits.request_timeout_s, 600);

        let text = format!("{CONFIG_TEMPLATE}\n[limits]\nmax_turns = 1\n");
        assert!(toml::from_str::<ForgeConfig>(&text).is_err());
    }

    #[test]
    fn state_budgets_accept_partial_tables() {
        let text = format!("{CONFIG_TEMPLATE}\n[limits.build]\nmax_turns = 7\n");
        let config: ForgeConfig = toml::from_str(&text).expect("parses");
        assert_eq!(config.limits.build.max_turns, 7);
        assert_eq!(config.limits.build.max_tokens, 1_000_000);
        assert_eq!(config.limits.plan, StateBudget::default());

        let text = format!("{CONFIG_TEMPLATE}\n[limits.build]\nmax_rounds = 7\n");
        assert!(toml::from_str::<ForgeConfig>(&text).is_err());
    }

    #[test]
    fn price_is_optional_and_costs_tokens() {
        assert_eq!(template().providers["runpod"].price_per_mtoken, None);
        let text = format!(
            "{CONFIG_TEMPLATE}\n[providers.runpod.price_per_mtoken]\ninput = 3.0\noutput = 15.0\n"
        );
        let config: ForgeConfig = toml::from_str(&text).expect("parses");
        let price = config.providers["runpod"]
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
            .get_mut("runpod")
            .expect("exists")
            .price_per_mtoken = Some(Price {
            input: -1.0,
            output: 1.0,
            cached: None,
        });
        assert_eq!(
            config.validate(),
            Err(ValidationError::NegativePrice {
                name: "runpod".to_owned()
            })
        );
    }

    #[test]
    fn telemetry_and_tickets_tables_default() {
        let config = template();
        assert_eq!(config.telemetry, TelemetryConfig::default());
        assert_eq!(config.telemetry.slow_median_ms, 60_000);
        assert_eq!(config.telemetry.error_rate_percent, 20);
        assert_eq!(config.tickets.remote, "origin");
        let text = format!(
            "{CONFIG_TEMPLATE}\n[telemetry]\nslow_median_ms = 5\n[tickets]\nremote = \"anvil\"\n"
        );
        let config: ForgeConfig = toml::from_str(&text).expect("parses");
        assert_eq!(config.telemetry.slow_median_ms, 5);
        assert_eq!(config.telemetry.error_rate_percent, 20);
        assert_eq!(config.tickets.remote, "anvil");
    }

    #[test]
    fn tools_shell_is_optional() {
        assert_eq!(template().tools.shell, None);
        let text = format!("{CONFIG_TEMPLATE}\n[tools]\nshell = \"bash\"\n");
        let config: ForgeConfig = toml::from_str(&text).expect("parses");
        assert_eq!(config.tools.shell.as_deref(), Some("bash"));
    }
}
