//! The rules store: `~/.anvil/rules.toml`.
//!
//! A rule is one sentence the coder must follow, tagged with the languages
//! it applies to and an optional check that enforces it on changed files.
//! The file is plain TOML with one `[[rules]]` table per rule, written in a
//! shape that is easy to edit by hand.

pub mod check;

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::MemoryError;

/// Longest allowed rule id, in characters.
pub const MAX_ID_LEN: usize = 64;

/// The prefix that turns a chat message into a rule.
pub const RULE_PREFIX: &str = "rule:";

/// The docstring check script, installed into `~/.anvil/checks/`.
pub const DOCSTRING_CHECK: &str = include_str!("../../checks/docstrings.py");

/// The file name the docstring check script is installed under.
pub const DOCSTRING_CHECK_FILE: &str = "docstrings.py";

/// The language tag that matches every file.
pub const ANY_LANGUAGE: &str = "*";

/// Words of a `rule:` message that make up its id.
const SLUG_WORDS: usize = 4;

/// The comment written at the top of a saved rules file.
const FILE_HEADER: &str = "\
# anvil standing rules. Edit by hand or with `anvil rules add|remove`.
# Each rule has an id (kebab-case), the languages it applies to (a list, or
# [\"*\"] for all), the rule itself (one sentence) and an optional check:
#   check = { regex = \"pattern that must not match any line of a changed file\" }
#   check = { command = \"program that gets the changed files as arguments\" }

";

/// How a rule is enforced on changed files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Check {
    /// A pattern that must not match any line of a changed file.
    Regex(String),
    /// A command that receives the changed files as arguments and must exit zero.
    Command(String),
}

/// One standing rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    /// Kebab-case identifier the reviewer cites.
    pub id: String,
    /// Languages the rule applies to, such as `python`, or `*` for all.
    pub languages: Vec<String>,
    /// The rule, one sentence.
    pub rule: String,
    /// How to enforce it, if it can be checked mechanically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<Check>,
}

impl Rule {
    /// A rule for every language with no check, from the text of a
    /// `rule:` message.
    ///
    /// # Arguments
    ///
    /// * `text` - The rule sentence, without the prefix.
    ///
    /// # Returns
    ///
    /// The rule, with an id made from its first words.
    pub fn from_message(text: &str) -> Self {
        Self {
            id: slug(text),
            languages: vec![ANY_LANGUAGE.to_owned()],
            rule: text.trim().to_owned(),
            check: None,
        }
    }

    /// Checks the rule's fields.
    ///
    /// # Returns
    ///
    /// `Ok(())` when the id is kebab-case, the text and language list are
    /// non-empty and any check is well formed.
    ///
    /// # Errors
    ///
    /// The first [`MemoryError`] found.
    pub fn validate(&self) -> Result<(), MemoryError> {
        if !is_kebab_case(&self.id) {
            return Err(MemoryError::InvalidRuleId {
                id: self.id.clone(),
                max: MAX_ID_LEN,
            });
        }
        if self.rule.trim().is_empty() {
            return Err(MemoryError::EmptyRule {
                id: self.id.clone(),
            });
        }
        if self
            .languages
            .iter()
            .all(|language| language.trim().is_empty())
        {
            return Err(MemoryError::NoLanguages {
                id: self.id.clone(),
            });
        }
        match &self.check {
            Some(Check::Regex(pattern)) => {
                regex::Regex::new(pattern).map_err(|error| MemoryError::BadRegex {
                    id: self.id.clone(),
                    detail: error.to_string(),
                })?;
            }
            Some(Check::Command(command)) if command.trim().is_empty() => {
                return Err(MemoryError::BadCommand {
                    id: self.id.clone(),
                });
            }
            Some(Check::Command(_)) | None => {}
        }
        Ok(())
    }

    /// The language list as shown in prompts: `python, rust` or `all languages`.
    fn languages_label(&self) -> String {
        if self
            .languages
            .iter()
            .any(|language| language == ANY_LANGUAGE)
        {
            return "all languages".to_owned();
        }
        self.languages.join(", ")
    }
}

/// Every standing rule, in file order.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleSet {
    /// The rules.
    #[serde(default)]
    pub rules: Vec<Rule>,
}

impl RuleSet {
    /// Reads and validates a rules file.
    ///
    /// # Arguments
    ///
    /// * `path` - The file.
    ///
    /// # Returns
    ///
    /// The rules.
    ///
    /// # Errors
    ///
    /// [`MemoryError::Io`] when the file cannot be read,
    /// [`MemoryError::Parse`] when it is not valid TOML, or a validation
    /// error naming the rule at fault.
    pub fn load(path: &Path) -> Result<Self, MemoryError> {
        let text = std::fs::read_to_string(path).map_err(|error| MemoryError::io(path, &error))?;
        let set: Self = toml::from_str(&text).map_err(|error| MemoryError::Parse {
            path: path.to_path_buf(),
            detail: error.to_string(),
        })?;
        set.validate()?;
        Ok(set)
    }

    /// Reads a rules file, writing the seed rules first when it is missing.
    ///
    /// # Arguments
    ///
    /// * `path` - The file.
    ///
    /// # Returns
    ///
    /// The rules and whether the file was just seeded.
    ///
    /// # Errors
    ///
    /// As [`RuleSet::load`], or [`MemoryError::Io`] when the seed cannot be written.
    pub fn open(path: &Path) -> Result<(Self, bool), MemoryError> {
        if path.is_file() {
            return Ok((Self::load(path)?, false));
        }
        let seed = Self::seed();
        seed.save(path)?;
        Ok((seed, true))
    }

    /// Writes the rules as TOML, creating the parent directory.
    ///
    /// # Arguments
    ///
    /// * `path` - The file.
    ///
    /// # Errors
    ///
    /// [`MemoryError::Io`] when the directory or file cannot be written.
    pub fn save(&self, path: &Path) -> Result<(), MemoryError> {
        debug_assert!(self.validate().is_ok(), "saving an invalid rule set");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| MemoryError::io(parent, &error))?;
        }
        std::fs::write(path, format!("{FILE_HEADER}{}", self.to_toml()))
            .map_err(|error| MemoryError::io(path, &error))
    }

    /// The rules as TOML, one `[[rules]]` table each with the check inline.
    ///
    /// Written by hand rather than with serde so the file keeps the shape
    /// the header documents; serde would move `check` into a sub-table.
    ///
    /// # Returns
    ///
    /// The TOML text.
    pub fn to_toml(&self) -> String {
        let quote = |text: &str| toml::Value::String(text.to_owned()).to_string();
        let mut out = String::new();
        for rule in &self.rules {
            out.push_str(
                "[[rules]]
",
            );
            out.push_str(&format!(
                "id = {}
",
                quote(&rule.id)
            ));
            let languages = rule
                .languages
                .iter()
                .map(|language| quote(language))
                .collect::<Vec<_>>()
                .join(", ");
            out.push_str(&format!(
                "languages = [{languages}]
"
            ));
            out.push_str(&format!(
                "rule = {}
",
                quote(&rule.rule)
            ));
            match &rule.check {
                Some(Check::Regex(pattern)) => {
                    out.push_str(&format!(
                        "check = {{ regex = {} }}
",
                        quote(pattern)
                    ));
                }
                Some(Check::Command(command)) => {
                    out.push_str(&format!(
                        "check = {{ command = {} }}
",
                        quote(command)
                    ));
                }
                None => {}
            }
            out.push('\n');
        }
        out
    }

    /// Checks every rule and that ids are unique.
    ///
    /// # Returns
    ///
    /// `Ok(())` when every rule validates and no id repeats.
    ///
    /// # Errors
    ///
    /// The first [`MemoryError`] found, in file order.
    pub fn validate(&self) -> Result<(), MemoryError> {
        let mut seen = std::collections::BTreeSet::new();
        for rule in &self.rules {
            rule.validate()?;
            if !seen.insert(rule.id.as_str()) {
                return Err(MemoryError::DuplicateRule {
                    id: rule.id.clone(),
                });
            }
        }
        Ok(())
    }

    /// Whether there are no rules.
    ///
    /// # Returns
    ///
    /// `true` when empty.
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// The rule with this id.
    ///
    /// # Arguments
    ///
    /// * `id` - The id.
    ///
    /// # Returns
    ///
    /// The rule, if any.
    pub fn get(&self, id: &str) -> Option<&Rule> {
        self.rules.iter().find(|rule| rule.id == id)
    }

    /// Appends a rule.
    ///
    /// # Arguments
    ///
    /// * `rule` - The rule.
    ///
    /// # Errors
    ///
    /// A validation error, or [`MemoryError::DuplicateRule`] when the id is taken.
    pub fn add(&mut self, rule: Rule) -> Result<(), MemoryError> {
        rule.validate()?;
        if self.get(&rule.id).is_some() {
            return Err(MemoryError::DuplicateRule { id: rule.id });
        }
        self.rules.push(rule);
        Ok(())
    }

    /// Appends a rule, suffixing its id with `-2`, `-3`, ... when it is taken.
    ///
    /// # Arguments
    ///
    /// * `rule` - The rule.
    ///
    /// # Returns
    ///
    /// The id it was stored under.
    ///
    /// # Errors
    ///
    /// A validation error.
    pub fn add_unique(&mut self, mut rule: Rule) -> Result<String, MemoryError> {
        let base = rule.id.clone();
        let mut suffix = 2;
        while self.get(&rule.id).is_some() {
            rule.id = format!("{base}-{suffix}");
            suffix += 1;
        }
        let id = rule.id.clone();
        self.add(rule)?;
        Ok(id)
    }

    /// Removes the rule with this id.
    ///
    /// # Arguments
    ///
    /// * `id` - The id.
    ///
    /// # Returns
    ///
    /// The removed rule.
    ///
    /// # Errors
    ///
    /// [`MemoryError::UnknownRule`] when no rule has the id.
    pub fn remove(&mut self, id: &str) -> Result<Rule, MemoryError> {
        let index = self
            .rules
            .iter()
            .position(|rule| rule.id == id)
            .ok_or_else(|| MemoryError::UnknownRule { id: id.to_owned() })?;
        Ok(self.rules.remove(index))
    }

    /// The numbered list for the coder's system prompt.
    ///
    /// # Returns
    ///
    /// The section body, or `None` when there are no rules.
    pub fn prompt_section(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        Some(format!(
            "Apply these to every file you touch, on top of the project's own conventions.\n\n{}",
            self.numbered_list()
        ))
    }

    /// The numbered list for the reviewer, with the citation requirement.
    ///
    /// # Returns
    ///
    /// The section body, or `None` when there are no rules.
    pub fn review_section(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        Some(format!(
            "The change must also follow these standing rules. When it violates one, request a \
             change whose item starts with the rule id in square brackets, for example \
             `[google-docstrings] src/api.py: add a docstring to parse_config`.\n\n{}",
            self.numbered_list()
        ))
    }

    /// `1. [id] (languages) rule` lines.
    fn numbered_list(&self) -> String {
        self.rules
            .iter()
            .enumerate()
            .map(|(index, rule)| {
                format!(
                    "{}. [{}] ({}) {}",
                    index + 1,
                    rule.id,
                    rule.languages_label(),
                    rule.rule.trim()
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The rules a fresh `rules.toml` starts with.
    ///
    /// # Returns
    ///
    /// Four rules; `google-docstrings` carries the docstring check.
    pub fn seed() -> Self {
        let any = || vec![ANY_LANGUAGE.to_owned()];
        Self {
            rules: vec![
                Rule {
                    id: "google-docstrings".to_owned(),
                    languages: vec!["python".to_owned(), "rust".to_owned()],
                    rule: "Every public function and class gets a Google-style docstring (Python) \
                           or a Google-shaped doc comment (Rust): a one-line summary, then Args, \
                           Returns and Raises or Errors where they apply."
                        .to_owned(),
                    check: Some(Check::Command(format!(
                        "python ~/{}/{}/{DOCSTRING_CHECK_FILE}",
                        crate::paths::ANVIL_DIR,
                        crate::paths::CHECKS_DIR
                    ))),
                },
                Rule {
                    id: "assert-boundaries".to_owned(),
                    languages: any(),
                    rule: "Validate inputs at class and function boundaries with assertions and \
                           descriptive errors."
                        .to_owned(),
                    check: None,
                },
                Rule {
                    id: "minimal-comments".to_owned(),
                    languages: any(),
                    rule: "Write no inline comment that restates the code; a comment explains \
                           why, never what."
                        .to_owned(),
                    check: None,
                },
                Rule {
                    id: "readable-first".to_owned(),
                    languages: any(),
                    rule: "An intermediate programmer must be able to follow any function \
                           without opening another file: YAGNI, DRY, orthogonality."
                        .to_owned(),
                    check: None,
                },
            ],
        }
    }
}

/// Writes the bundled check scripts into the checks directory when they
/// are missing, so the seeded `command` checks work in any repository.
///
/// # Arguments
///
/// * `checks_dir` - `~/.anvil/checks`.
///
/// # Returns
///
/// Whether anything was written.
///
/// # Errors
///
/// [`MemoryError::Io`] when the directory or a script cannot be written.
pub fn ensure_check_scripts(checks_dir: &Path) -> Result<bool, MemoryError> {
    let path = checks_dir.join(DOCSTRING_CHECK_FILE);
    if path.is_file() {
        return Ok(false);
    }
    std::fs::create_dir_all(checks_dir).map_err(|error| MemoryError::io(checks_dir, &error))?;
    std::fs::write(&path, DOCSTRING_CHECK).map_err(|error| MemoryError::io(&path, &error))?;
    Ok(true)
}

/// The rule text of a `rule:` message.
///
/// # Arguments
///
/// * `line` - The user's message.
///
/// # Returns
///
/// The text after the prefix, trimmed, or `None` when the message does
/// not start with `rule:` (any case) or nothing follows it.
pub fn strip_rule_prefix(line: &str) -> Option<&str> {
    let line = line.trim();
    let (head, rest) = line.split_at_checked(RULE_PREFIX.len())?;
    if !head.eq_ignore_ascii_case(RULE_PREFIX) {
        return None;
    }
    let rest = rest.trim();
    (!rest.is_empty()).then_some(rest)
}

/// A kebab-case id from the first words of a sentence.
///
/// # Arguments
///
/// * `text` - The sentence.
///
/// # Returns
///
/// Up to four lowercase words joined by hyphens, or `rule` when the text
/// has no letters or digits.
pub fn slug(text: &str) -> String {
    let words: Vec<String> = text
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .take(SLUG_WORDS)
        .map(str::to_ascii_lowercase)
        .collect();
    if words.is_empty() {
        return "rule".to_owned();
    }
    words
        .join("-")
        .chars()
        .take(MAX_ID_LEN)
        .collect::<String>()
        .trim_end_matches('-')
        .to_owned()
}

/// Whether an id is lowercase kebab-case within the length limit.
fn is_kebab_case(id: &str) -> bool {
    !id.is_empty()
        && id.chars().count() <= MAX_ID_LEN
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !id.starts_with('-')
        && !id.ends_with('-')
        && !id.contains("--")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_validates_and_round_trips_through_toml() {
        let seed = RuleSet::seed();
        seed.validate().expect("seed validates");
        assert_eq!(seed.rules.len(), 4);
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("deep").join("rules.toml");
        seed.save(&path).expect("saves");
        let text = std::fs::read_to_string(&path).expect("written");
        assert!(text.starts_with("# anvil standing rules"), "{text}");
        assert!(text.contains("[[rules]]"), "{text}");
        assert!(
            text.contains("check = { command = \"python ~/.anvil/checks/docstrings.py\" }"),
            "{text}"
        );
        assert_eq!(RuleSet::load(&path).expect("loads"), seed);
    }

    #[test]
    fn open_seeds_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("rules.toml");
        let (first, seeded) = RuleSet::open(&path).expect("opens");
        assert!(seeded);
        assert_eq!(first, RuleSet::seed());
        let (second, seeded) = RuleSet::open(&path).expect("opens again");
        assert!(!seeded);
        assert_eq!(second, first);
    }

    #[test]
    fn regex_checks_round_trip_and_hand_written_files_parse() {
        let text = "[[rules]]\nid = \"no-todo\"\nlanguages = [\"rust\"]\nrule = \"Leave no TODO.\"\ncheck = { regex = \"TODO\" }\n\n[[rules]]\nid = \"plain\"\nlanguages = [\"*\"]\nrule = \"Be plain.\"\n";
        let set: RuleSet = toml::from_str(text).expect("parses");
        assert_eq!(set.rules[0].check, Some(Check::Regex("TODO".to_owned())));
        assert_eq!(set.rules[1].check, None);
        let back = set.to_toml();
        assert!(back.contains("check = { regex = \"TODO\" }"), "{back}");
        assert!(back.contains("languages = [\"rust\"]"), "{back}");
        assert_eq!(toml::from_str::<RuleSet>(&back).expect("parses back"), set);
        assert!(toml::from_str::<RuleSet>("[[rules]]\nid = \"x\"\nbogus = 1\n").is_err());
    }

    #[test]
    fn validation_names_the_fault() {
        let mut set = RuleSet::seed();
        set.rules[0].id = "Bad Id".to_owned();
        assert!(matches!(
            set.validate(),
            Err(MemoryError::InvalidRuleId { .. })
        ));
        let mut set = RuleSet::seed();
        set.rules[1].rule = "  ".to_owned();
        assert_eq!(
            set.validate(),
            Err(MemoryError::EmptyRule {
                id: "assert-boundaries".to_owned()
            })
        );
        let mut set = RuleSet::seed();
        set.rules[1].languages.clear();
        assert!(matches!(
            set.validate(),
            Err(MemoryError::NoLanguages { .. })
        ));
        let mut set = RuleSet::seed();
        set.rules[1].check = Some(Check::Regex("(".to_owned()));
        assert!(matches!(set.validate(), Err(MemoryError::BadRegex { .. })));
        let mut set = RuleSet::seed();
        set.rules[1].check = Some(Check::Command(" ".to_owned()));
        assert!(matches!(
            set.validate(),
            Err(MemoryError::BadCommand { .. })
        ));
        let mut set = RuleSet::seed();
        set.rules[1].id = "readable-first".to_owned();
        assert_eq!(
            set.validate(),
            Err(MemoryError::DuplicateRule {
                id: "readable-first".to_owned()
            })
        );
    }

    #[test]
    fn add_remove_and_unique_ids() {
        let mut set = RuleSet::default();
        set.add(Rule::from_message("Never call unwrap outside tests."))
            .expect("adds");
        assert_eq!(set.rules[0].id, "never-call-unwrap-outside");
        assert_eq!(set.rules[0].languages, vec!["*"]);
        assert_eq!(
            set.add(Rule::from_message("never call unwrap outside")),
            Err(MemoryError::DuplicateRule {
                id: "never-call-unwrap-outside".to_owned()
            })
        );
        let id = set
            .add_unique(Rule::from_message("Never call unwrap outside of tests"))
            .expect("adds");
        assert_eq!(id, "never-call-unwrap-outside-2");
        let id = set
            .add_unique(Rule::from_message("Never call unwrap outside of tests"))
            .expect("adds");
        assert_eq!(id, "never-call-unwrap-outside-3");
        assert_eq!(set.remove(&id).expect("removes").id, id);
        assert_eq!(set.rules.len(), 2);
        assert!(matches!(
            set.remove("missing"),
            Err(MemoryError::UnknownRule { .. })
        ));
        assert!(set.get("never-call-unwrap-outside").is_some());
    }

    #[test]
    fn rule_prefix_and_slug() {
        assert_eq!(
            strip_rule_prefix("  RULE:   keep functions short  "),
            Some("keep functions short")
        );
        assert_eq!(strip_rule_prefix("rule:"), None);
        assert_eq!(strip_rule_prefix("rules are nice"), None);
        assert_eq!(strip_rule_prefix("ru"), None);
        assert_eq!(
            slug("Don't use `unwrap()` in library code, ever."),
            "don-t-use-unwrap"
        );
        assert_eq!(slug("!!!"), "rule");
    }

    #[test]
    fn prompt_sections_snapshot() {
        let seed = RuleSet::seed();
        insta::assert_snapshot!("prompt_section", seed.prompt_section().expect("section"));
        insta::assert_snapshot!("review_section", seed.review_section().expect("section"));
        assert_eq!(RuleSet::default().prompt_section(), None);
        assert_eq!(RuleSet::default().review_section(), None);
    }

    #[test]
    fn check_scripts_are_installed_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let checks = dir.path().join("checks");
        assert!(ensure_check_scripts(&checks).expect("installs"));
        assert!(!ensure_check_scripts(&checks).expect("already there"));
        let script = std::fs::read_to_string(checks.join(DOCSTRING_CHECK_FILE)).expect("script");
        assert_eq!(script, DOCSTRING_CHECK);
    }
}
