//! Provider and role configuration: which model does which job.
//!
//! Lives at `~/.forge/config.toml` (global) and `.forge/config.toml`
//! (project, overrides global). Models are swapped by editing these files,
//! never by changing code.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::ValidationError;

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

#[cfg(test)]
mod tests {
    use super::*;

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
