//! The `[shell]` settings in force: the user's configuration, bounded by
//! the managed policy when one is installed.
//!
//! The policy decides the executor and the container; the user may only
//! make the mode stricter and add allowlist patterns, which narrow the
//! policy's. A user setting that would loosen the policy is refused before
//! this runs (`crate::policy::check_config`).

use anvil_core::agent::PromptSection;
use anvil_core::tools::{Container, Runner};
use anvil_types::policy::ShellPolicy;
use anvil_types::shell::{ContainerSettings, Executor, ShellConfig, ShellMode};

/// The heading of the prompt section that tells the model the allowlist.
const ALLOWLIST_HEADING: &str = "Shell allowlist";

/// How the `shell` tool may run commands in this session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellSettings {
    /// The strictest mode of the layers.
    pub mode: ShellMode,
    /// The allowlist layers, each with where it came from; a command must
    /// match a pattern of every layer.
    pub layers: Vec<(String, Vec<String>)>,
    /// Where commands run.
    pub executor: Executor,
    /// The container settings when `executor` is `container`.
    pub container: Option<ContainerSettings>,
}

impl ShellSettings {
    /// Combines the user's `[shell]` table with the policy's.
    ///
    /// # Arguments
    ///
    /// * `config` - The user's `[shell]` table, after any profile.
    /// * `policy` - The policy's `[shell]` table, when a policy is installed.
    ///
    /// # Returns
    ///
    /// Unmanaged: the user's settings, `ask` on the host by default.
    /// Managed: the stricter mode of the two, every allowlist of a layer in
    /// allowlist mode, and the policy's executor and container.
    pub fn resolve(config: &ShellConfig, policy: Option<&ShellPolicy>) -> Self {
        let user_mode = config.mode.unwrap_or(ShellMode::Ask);
        let user_layer = (user_mode == ShellMode::Allowlist)
            .then(|| ("your [shell] allow".to_owned(), config.allow.clone()));
        let Some(policy) = policy else {
            return Self {
                mode: user_mode,
                layers: user_layer.into_iter().collect(),
                executor: config.executor.unwrap_or_default(),
                container: config.container.clone(),
            };
        };
        let mode = match config.mode {
            Some(mode) if mode.strictness() > policy.mode.strictness() => mode,
            _ => policy.mode,
        };
        let mut layers = Vec::new();
        if policy.mode == ShellMode::Allowlist {
            layers.push((
                "the managed policy's [shell] allow".to_owned(),
                policy.allow.clone(),
            ));
        }
        layers.extend(user_layer);
        Self {
            mode,
            layers,
            executor: policy.executor,
            container: policy.container.clone(),
        }
    }

    /// How commands start.
    ///
    /// # Arguments
    ///
    /// * `direct` - Whether another layer (a `shell_programs` list) already
    ///   limits commands to an allowlist.
    ///
    /// # Returns
    ///
    /// Direct start, without a shell, in allowlist mode or when `direct`;
    /// the container executor when one is configured.
    pub fn runner(&self, direct: bool) -> Runner {
        Runner {
            direct: direct || self.mode == ShellMode::Allowlist,
            container: match (self.executor, &self.container) {
                (Executor::Container, Some(settings)) => Some(Container::new(settings.clone())),
                _ => None,
            },
        }
    }

    /// What the model is told about the shell in allowlist mode.
    ///
    /// # Returns
    ///
    /// A prompt section listing the allowed commands, or `None` in another
    /// mode.
    pub fn prompt_section(&self) -> Option<PromptSection> {
        if self.mode != ShellMode::Allowlist {
            return None;
        }
        let mut text = String::from(
            "The shell tool starts one program with plain arguments, without a shell: \
             no `;`, `&&`, `|`, redirection, substitution, braces or line breaks.",
        );
        for (source, patterns) in &self.layers {
            text.push_str(&format!(
                "\n- Allowed by {source}: {}",
                patterns
                    .iter()
                    .map(|pattern| format!("`{pattern}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        Some(PromptSection {
            heading: ALLOWLIST_HEADING.to_owned(),
            text,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(mode: ShellMode, allow: &[&str]) -> ShellPolicy {
        ShellPolicy {
            mode,
            allow: allow.iter().map(|p| (*p).to_owned()).collect(),
            ..ShellPolicy::default()
        }
    }

    #[test]
    fn unmanaged_takes_the_users_settings_with_ask_by_default() {
        let settings = ShellSettings::resolve(&ShellConfig::default(), None);
        assert_eq!(settings.mode, ShellMode::Ask);
        assert!(settings.layers.is_empty());
        assert!(!settings.runner(false).direct);
        assert!(settings.prompt_section().is_none());
        let allowlist = ShellConfig {
            mode: Some(ShellMode::Allowlist),
            allow: vec!["cargo test *".to_owned()],
            ..ShellConfig::default()
        };
        let settings = ShellSettings::resolve(&allowlist, None);
        assert!(settings.runner(false).direct);
        let section = settings.prompt_section().expect("section");
        assert!(section.text.contains("`cargo test *`"), "{}", section.text);
    }

    #[test]
    fn under_a_policy_the_stricter_mode_and_every_allowlist_apply() {
        let user = ShellConfig {
            mode: Some(ShellMode::Allowlist),
            allow: vec!["cargo test *".to_owned()],
            ..ShellConfig::default()
        };
        let both = ShellSettings::resolve(&user, Some(&policy(ShellMode::Allowlist, &["cargo *"])));
        assert_eq!(both.mode, ShellMode::Allowlist);
        assert_eq!(both.layers.len(), 2, "the user can only narrow");
        let stricter = ShellSettings::resolve(&user, Some(&policy(ShellMode::Ask, &[])));
        assert_eq!(stricter.mode, ShellMode::Allowlist);
        let off = ShellSettings::resolve(&user, Some(&ShellPolicy::default()));
        assert_eq!(off.mode, ShellMode::Off, "the policy's default is off");
        let unset =
            ShellSettings::resolve(&ShellConfig::default(), Some(&policy(ShellMode::Ask, &[])));
        assert_eq!(unset.mode, ShellMode::Ask);
    }
}
