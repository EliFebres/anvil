//! The skill manifest: the frontmatter of a Claude Code-format `SKILL.md`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::ValidationError;

/// Maximum length of a skill name, in characters.
pub const MAX_SKILL_NAME_CHARS: usize = 64;

/// Maximum length of a skill description, in characters.
pub const MAX_SKILL_DESCRIPTION_CHARS: usize = 1024;

/// Metadata for one skill, as found in the frontmatter of its `SKILL.md`.
///
/// Mirrors Claude Code's skill format so skills in `skills/` load unchanged
/// there. Parsing the Markdown file is `forge-compat`'s job; this is the
/// typed result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SkillManifest {
    /// Kebab-case name, also the skill's directory name.
    pub name: String,
    /// What the skill does and when to use it. This is the trigger text.
    pub description: String,
    /// Tools the skill may use without asking. Empty means no restriction.
    #[serde(
        rename = "allowed-tools",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub allowed_tools: Vec<String>,
}

impl SkillManifest {
    /// Checks the name and description limits.
    ///
    /// # Returns
    ///
    /// `Ok(())` when the name is kebab-case (`a-z`, `0-9`, single hyphens,
    /// no leading or trailing hyphen) of 1 to [`MAX_SKILL_NAME_CHARS`]
    /// characters and the description is 1 to
    /// [`MAX_SKILL_DESCRIPTION_CHARS`] characters.
    ///
    /// # Errors
    ///
    /// [`ValidationError::InvalidSkillName`], [`ValidationError::EmptyField`]
    /// or [`ValidationError::DescriptionTooLong`].
    pub fn validate(&self) -> Result<(), ValidationError> {
        if !is_kebab_case(&self.name) || self.name.chars().count() > MAX_SKILL_NAME_CHARS {
            return Err(ValidationError::InvalidSkillName {
                name: self.name.clone(),
                max: MAX_SKILL_NAME_CHARS,
            });
        }
        let description_len = self.description.chars().count();
        if self.description.trim().is_empty() {
            return Err(ValidationError::EmptyField {
                field: "description",
            });
        }
        if description_len > MAX_SKILL_DESCRIPTION_CHARS {
            return Err(ValidationError::DescriptionTooLong {
                len: description_len,
                max: MAX_SKILL_DESCRIPTION_CHARS,
            });
        }
        Ok(())
    }
}

/// True for non-empty `a-z0-9` words joined by single hyphens.
fn is_kebab_case(name: &str) -> bool {
    !name.is_empty()
        && name.split('-').all(|word| {
            !word.is_empty()
                && word
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> SkillManifest {
        SkillManifest {
            name: "rust-review".to_owned(),
            description:
                "Review Rust changes for clippy-clean, idiomatic code. Use before committing."
                    .to_owned(),
            allowed_tools: vec!["Read".to_owned(), "Grep".to_owned()],
        }
    }

    #[test]
    fn sample_is_valid() {
        sample().validate().expect("sample validates");
    }

    #[test]
    fn json_round_trip_uses_claude_code_key_names() {
        let text = serde_json::to_string(&sample()).expect("serializes");
        assert!(text.contains("\"allowed-tools\""), "{text}");
        let back: SkillManifest = serde_json::from_str(&text).expect("parses back");
        assert_eq!(back, sample());
    }

    #[test]
    fn toml_round_trip_and_missing_tools_default_to_empty() {
        let text = toml::to_string(&sample()).expect("serializes");
        let back: SkillManifest = toml::from_str(&text).expect("parses back");
        assert_eq!(back, sample());

        let minimal: SkillManifest =
            toml::from_str("name = \"x\"\ndescription = \"y\"\n").expect("parses");
        assert!(minimal.allowed_tools.is_empty());
    }

    #[test]
    fn bad_names_are_rejected() {
        for bad in [
            "",
            "Rust-Review",
            "rust_review",
            "-rust",
            "rust-",
            "rust--review",
            "a b",
        ] {
            let mut skill = sample();
            skill.name = bad.to_owned();
            assert!(
                matches!(
                    skill.validate(),
                    Err(ValidationError::InvalidSkillName { .. })
                ),
                "{bad:?} should be rejected"
            );
        }
        let mut skill = sample();
        skill.name = "a".repeat(MAX_SKILL_NAME_CHARS + 1);
        assert!(skill.validate().is_err());
    }

    #[test]
    fn description_limits() {
        let mut skill = sample();
        skill.description = " ".to_owned();
        assert_eq!(
            skill.validate(),
            Err(ValidationError::EmptyField {
                field: "description"
            })
        );
        skill.description = "d".repeat(MAX_SKILL_DESCRIPTION_CHARS + 1);
        assert_eq!(
            skill.validate(),
            Err(ValidationError::DescriptionTooLong {
                len: 1025,
                max: 1024
            })
        );
    }
}
