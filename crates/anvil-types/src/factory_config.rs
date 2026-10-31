//! `factory.toml`: which models the factory uses, where its pull requests go
//! and where it sends notifications.
//!
//! The file lives at the repository root and is committed. Provider names
//! refer to entries in the anvil configuration (`~/.anvil/config.toml`).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::dotenv::is_placeholder;
use crate::error::ValidationError;

/// File name of the factory configuration at the repository root.
pub const FACTORY_FILE: &str = "factory.toml";

/// The placeholder `anvil setup` writes when no GitHub remote is found.
pub const REPO_PLACEHOLDER: &str = "<owner>/<repo>";

/// The template written by `anvil setup`; `{repo}` is replaced with the
/// `owner/repo` detected from the git remote.
pub const FACTORY_TEMPLATE: &str = r#"# factory configuration, written by `anvil setup` and committed.
# Provider names refer to [providers.<name>] entries in ~/.anvil/config.toml.

[roles]
coder = "anthropic"

# Each reviewer reads the pull request with one focus. Reviewers must sit on
# different base URLs from the coder, so no model reviews its own work.
[[reviewers]]
provider = "openai"
focus = "correctness"

[[reviewers]]
provider = "google"
focus = "safety"

[[reviewers]]
provider = "openai"
focus = "style"

[[reviewers]]
provider = "google"
focus = "simplicity"

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

/// One reviewer of a factory pull request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Reviewer {
    /// Provider that reviews.
    pub provider: String,
    /// What the review concentrates on: `correctness`, `safety`, `style`, ...
    pub focus: String,
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
    /// a placeholder, and `github.repo` is `owner/repo`.
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
            check_filled("reviewers.focus", &reviewer.focus)?;
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
        check_filled("notify.webhook_env", &self.notify.webhook_env)
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
        assert_eq!(config.reviewers[1].focus, "safety");
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
        config.reviewers[0].focus = " ".to_owned();
        assert_eq!(
            config.validate(),
            Err(ValidationError::EmptyField {
                field: "reviewers.focus"
            })
        );
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let text = format!("{}\n[extra]\nx = 1\n", FactoryConfig::template("o/r"));
        assert!(toml::from_str::<FactoryConfig>(&text).is_err());
    }
}
