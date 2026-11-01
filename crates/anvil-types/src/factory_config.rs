//! `factory.toml`: which models the factory uses, where its pull requests go
//! and where it sends notifications.
//!
//! The file lives at the repository root and is committed. Provider names
//! refer to entries in the anvil configuration (`~/.anvil/config.toml`).

use std::collections::BTreeMap;
use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::config::AnvilConfig;
use crate::dotenv::is_placeholder;
use crate::error::ValidationError;

/// File name of the factory configuration at the repository root.
pub const FACTORY_FILE: &str = "factory.toml";

/// The placeholder `anvil setup` writes when no GitHub remote is found.
pub const REPO_PLACEHOLDER: &str = "<owner>/<repo>";

/// Eval tasks that run at once when `[eval] parallel` is not set.
pub const DEFAULT_EVAL_PARALLEL: u32 = 2;

/// The template written by `anvil setup`; `{repo}` is replaced with the
/// `owner/repo` detected from the git remote.
pub const FACTORY_TEMPLATE: &str = r#"# factory configuration, written by `anvil setup` and committed.
# Provider names refer to [providers.<name>] entries in ~/.anvil/config.toml.

[roles]
coder = "anthropic"

# Each reviewer reads the pull request with one stance: correctness, safety,
# style or simplicity. Reviewers must sit on different base URLs from the
# coder, so no model reviews its own work, and on at least two base URLs
# between them, so no single model's blind spots decide a review.
[[reviewers]]
provider = "openai"
stance = "correctness"

[[reviewers]]
provider = "google"
stance = "safety"

[[reviewers]]
provider = "openai"
stance = "style"

[[reviewers]]
provider = "google"
stance = "simplicity"

# How many eval tasks run at once. Default: 2.
# [eval]
# parallel = 2

[github]
repo = "{repo}"

# The environment variable that holds a Discord, Slack or ntfy webhook URL.
[notify]
webhook_env = "WEBHOOK_URL"
"#;

/// Which provider writes the code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FactoryRoles {
    /// Provider that works tickets.
    pub coder: String,
}

/// What one reviewer concentrates on.
///
/// Each stance has its own prompt in `factory-review`; together the four
/// cover a pull request without overlapping.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum Stance {
    /// Does the change do what the ticket's acceptance list says; edge cases; error paths.
    Correctness,
    /// Permissions, shell commands, network calls, secrets; anything that widens what anvil can do.
    Safety,
    /// The standing rules from `~/.anvil/rules.toml` and `CLAUDE.md`, cited by rule id.
    Style,
    /// What could be deleted; duplicated logic; YAGNI violations; tests that test nothing.
    Simplicity,
}

impl Stance {
    /// Every stance, in the order the template lists them.
    pub const ALL: [Stance; 4] = [
        Stance::Correctness,
        Stance::Safety,
        Stance::Style,
        Stance::Simplicity,
    ];

    /// The stance as it is written in `factory.toml`.
    ///
    /// # Returns
    ///
    /// The lowercase name.
    pub fn as_str(self) -> &'static str {
        match self {
            Stance::Correctness => "correctness",
            Stance::Safety => "safety",
            Stance::Style => "style",
            Stance::Simplicity => "simplicity",
        }
    }
}

impl fmt::Display for Stance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One reviewer of a factory pull request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Reviewer {
    /// Provider that reviews.
    pub provider: String,
    /// What the review concentrates on.
    pub stance: Stance,
}

/// Where pull requests go.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GithubConfig {
    /// The repository as `owner/repo`.
    pub repo: String,
}

/// Where notifications go.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NotifyConfig {
    /// Environment variable that holds the webhook URL.
    pub webhook_env: String,
}

/// How the eval harness runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct EvalConfig {
    /// How many eval tasks run at once.
    pub parallel: u32,
}

impl Default for EvalConfig {
    fn default() -> Self {
        Self {
            parallel: DEFAULT_EVAL_PARALLEL,
        }
    }
}

/// The whole `factory.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FactoryConfig {
    /// Which provider writes code.
    pub roles: FactoryRoles,
    /// The reviewers, in the order they run.
    pub reviewers: Vec<Reviewer>,
    /// Where pull requests go.
    pub github: GithubConfig,
    /// Where notifications go.
    pub notify: NotifyConfig,
    /// How the eval harness runs; every key has a default.
    #[serde(default)]
    pub eval: EvalConfig,
}

impl FactoryConfig {
    /// Renders the template for a repository.
    ///
    /// # Arguments
    ///
    /// * `repo` - The `owner/repo` string, or [`REPO_PLACEHOLDER`].
    ///
    /// # Returns
    ///
    /// The TOML text.
    pub fn template(repo: &str) -> String {
        FACTORY_TEMPLATE.replace("{repo}", repo)
    }

    /// Checks the shape of every field.
    ///
    /// # Returns
    ///
    /// `Ok(())` when there is at least one reviewer, no text field is empty or
    /// a placeholder, `github.repo` is `owner/repo` and `eval.parallel` is at
    /// least 1.
    ///
    /// # Errors
    ///
    /// The first [`ValidationError`] found.
    pub fn validate(&self) -> Result<(), ValidationError> {
        check_filled("roles.coder", &self.roles.coder)?;
        if self.reviewers.is_empty() {
            return Err(ValidationError::EmptyField { field: "reviewers" });
        }
        for reviewer in &self.reviewers {
            check_filled("reviewers.provider", &reviewer.provider)?;
        }
        check_filled("github.repo", &self.github.repo)?;
        let mut parts = self.github.repo.split('/');
        let well_formed = matches!(
            (parts.next(), parts.next(), parts.next()),
            (Some(owner), Some(repo), None) if !owner.is_empty() && !repo.is_empty()
        );
        if !well_formed {
            return Err(ValidationError::InvalidRepo {
                repo: self.github.repo.clone(),
            });
        }
        check_filled("notify.webhook_env", &self.notify.webhook_env)?;
        if self.eval.parallel == 0 {
            return Err(ValidationError::ZeroField {
                field: "eval.parallel",
            });
        }
        Ok(())
    }

    /// Checks that the reviewers are independent of the coder and of each other.
    ///
    /// Different training, different blind spots: that is the point of the
    /// swarm. Base URLs are compared with any trailing `/` removed.
    ///
    /// # Arguments
    ///
    /// * `config` - The merged anvil configuration whose `[providers]` the
    ///   names resolve against.
    ///
    /// # Returns
    ///
    /// `Ok(())` when the coder and every reviewer name a provider, no reviewer
    /// shares a base URL with the coder, two differently named reviewers never
    /// share a base URL, and the reviewers span at least two base URLs.
    ///
    /// # Errors
    ///
    /// The first [`ValidationError`] found.
    pub fn check_independence(&self, config: &AnvilConfig) -> Result<(), ValidationError> {
        let base_url = |role: &'static str, name: &str| {
            config
                .providers
                .get(name)
                .map(|provider| provider.base_url.trim_end_matches('/').to_owned())
                .ok_or_else(|| ValidationError::UnknownProvider {
                    role,
                    provider: name.to_owned(),
                })
        };
        let coder = &self.roles.coder;
        let coder_url = base_url("coder", coder)?;
        let mut seen: BTreeMap<String, &str> = BTreeMap::new();
        for reviewer in &self.reviewers {
            let name = reviewer.provider.as_str();
            let url = base_url("reviewer", name)?;
            if url == coder_url {
                return Err(ValidationError::ReviewerSharesCoderUrl {
                    reviewer: name.to_owned(),
                    coder: coder.clone(),
                    base_url: url,
                });
            }
            match seen.get(&url) {
                Some(other) if *other != name => {
                    return Err(ValidationError::ReviewersShareUrl {
                        first: (*other).to_owned(),
                        second: name.to_owned(),
                        base_url: url,
                    });
                }
                Some(_) => {}
                None => {
                    seen.insert(url, name);
                }
            }
        }
        if seen.len() < 2 {
            return Err(ValidationError::TooFewReviewerUrls { found: seen.len() });
        }
        Ok(())
    }

    /// The fields that still hold template placeholders.
    ///
    /// # Returns
    ///
    /// Dotted field names, such as `github.repo`.
    pub fn unfilled(&self) -> Vec<&'static str> {
        let mut fields = Vec::new();
        if is_placeholder(&self.github.repo) {
            fields.push("github.repo");
        }
        fields
    }
}

/// Fails when `value` is empty or a placeholder.
fn check_filled(field: &'static str, value: &str) -> Result<(), ValidationError> {
    if is_placeholder(value) {
        Err(ValidationError::EmptyField { field })
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filled() -> FactoryConfig {
        toml::from_str(&FactoryConfig::template("owner/repo")).expect("template parses")
    }

    #[test]
    fn template_parses_and_validates() {
        let config = filled();
        config.validate().expect("validates");
        assert_eq!(config.roles.coder, "anthropic");
        assert_eq!(config.reviewers.len(), 4);
        assert_eq!(config.reviewers[1].provider, "google");
        assert_eq!(config.reviewers[1].stance, Stance::Safety);
        assert_eq!(config.github.repo, "owner/repo");
        assert_eq!(config.notify.webhook_env, "WEBHOOK_URL");
        assert!(config.unfilled().is_empty());
    }

    #[test]
    fn placeholder_repo_is_reported_and_rejected() {
        let config: FactoryConfig =
            toml::from_str(&FactoryConfig::template(REPO_PLACEHOLDER)).expect("parses");
        assert_eq!(config.unfilled(), vec!["github.repo"]);
        assert_eq!(
            config.validate(),
            Err(ValidationError::EmptyField {
                field: "github.repo"
            })
        );
    }

    #[test]
    fn repo_must_be_owner_slash_name() {
        for bad in ["owner", "owner/", "/repo", "a/b/c"] {
            let mut config = filled();
            config.github.repo = bad.to_owned();
            assert!(
                matches!(config.validate(), Err(ValidationError::InvalidRepo { .. })),
                "{bad}"
            );
        }
    }

    #[test]
    fn reviewers_and_fields_are_required() {
        let mut config = filled();
        config.reviewers.clear();
        assert_eq!(
            config.validate(),
            Err(ValidationError::EmptyField { field: "reviewers" })
        );
        let mut config = filled();
        config.reviewers[0].provider = " ".to_owned();
        assert_eq!(
            config.validate(),
            Err(ValidationError::EmptyField {
                field: "reviewers.provider"
            })
        );
    }

    #[test]
    fn stance_round_trips_and_rejects_unknown_names() {
        for stance in Stance::ALL {
            let text = format!("\"{stance}\"");
            let parsed: Stance = serde_json::from_str(&text).expect("parses");
            assert_eq!(parsed, stance);
            assert_eq!(serde_json::to_string(&stance).expect("serializes"), text);
        }
        let text = FactoryConfig::template("o/r").replace("\"safety\"", "\"vibes\"");
        let error = toml::from_str::<FactoryConfig>(&text).expect_err("rejected");
        assert!(error.to_string().contains("vibes"), "{error}");
    }

    fn providers() -> AnvilConfig {
        toml::from_str(crate::config::SETUP_PROVIDERS).expect("parses")
    }

    fn with_reviewers(coder: &str, reviewers: &[&str]) -> FactoryConfig {
        let mut config = filled();
        config.roles.coder = coder.to_owned();
        config.reviewers = reviewers
            .iter()
            .zip(Stance::ALL.iter().cycle())
            .map(|(provider, stance)| Reviewer {
                provider: (*provider).to_owned(),
                stance: *stance,
            })
            .collect();
        config
    }

    #[test]
    fn the_template_layout_is_independent() {
        filled()
            .check_independence(&providers())
            .expect("independent");
        with_reviewers("anthropic", &["openai", "google", "openai"])
            .check_independence(&providers())
            .expect("a provider may review twice with different stances");
    }

    #[test]
    fn a_reviewer_on_the_coder_url_is_rejected() {
        let error = with_reviewers("anthropic", &["anthropic", "openai"])
            .check_independence(&providers())
            .expect_err("rejected");
        assert!(
            matches!(
                &error,
                ValidationError::ReviewerSharesCoderUrl { reviewer, coder, .. }
                    if reviewer == "anthropic" && coder == "anthropic"
            ),
            "{error}"
        );
    }

    #[test]
    fn two_reviewers_on_one_url_are_rejected() {
        let mut config = providers();
        let openai_url = config.providers["openai"].base_url.clone();
        config.providers.get_mut("google").expect("google").base_url = format!("{openai_url}/");
        let error = with_reviewers("anthropic", &["openai", "google"])
            .check_independence(&config)
            .expect_err("rejected");
        assert!(
            matches!(
                &error,
                ValidationError::ReviewersShareUrl { first, second, .. }
                    if first == "openai" && second == "google"
            ),
            "{error}"
        );
    }

    #[test]
    fn reviewers_must_span_two_urls() {
        let error = with_reviewers("anthropic", &["openai", "openai"])
            .check_independence(&providers())
            .expect_err("rejected");
        assert_eq!(error, ValidationError::TooFewReviewerUrls { found: 1 });
    }

    #[test]
    fn unknown_providers_are_named() {
        let error = with_reviewers("nope", &["openai", "google"])
            .check_independence(&providers())
            .expect_err("rejected");
        assert!(
            matches!(&error, ValidationError::UnknownProvider { role: "coder", provider } if provider == "nope"),
            "{error}"
        );
        let error = with_reviewers("anthropic", &["openai", "nope"])
            .check_independence(&providers())
            .expect_err("rejected");
        assert!(
            matches!(&error, ValidationError::UnknownProvider { role: "reviewer", provider } if provider == "nope"),
            "{error}"
        );
    }

    #[test]
    fn eval_section_defaults_and_rejects_zero() {
        let config = filled();
        assert_eq!(config.eval, EvalConfig::default());
        assert_eq!(config.eval.parallel, DEFAULT_EVAL_PARALLEL);
        let text = format!(
            "{}
[eval]
parallel = 4
",
            FactoryConfig::template("o/r")
        );
        let config: FactoryConfig = toml::from_str(&text).expect("parses");
        assert_eq!(config.eval.parallel, 4);
        let mut config = filled();
        config.eval.parallel = 0;
        assert_eq!(
            config.validate(),
            Err(ValidationError::ZeroField {
                field: "eval.parallel"
            })
        );
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let text = format!("{}\n[extra]\nx = 1\n", FactoryConfig::template("o/r"));
        assert!(toml::from_str::<FactoryConfig>(&text).is_err());
    }
}
