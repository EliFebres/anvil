//! The permission model: `.anvil/permissions.toml`, rule matching, and the
//! prompt hook the CLI implements.
//!
//! A rule is `tool` or `tool:glob`. The glob matches the call's target: the
//! `path` argument for file tools, the `command` for `shell`. When several
//! rules match, `deny` wins, then `allow`, then `ask`, then the tool's own
//! default.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anvil_types::{PermissionRules, Unmatched};
use globset::{Glob, GlobMatcher};
use thiserror::Error;

use crate::provider::lock_or_recover;

/// Where the project's rules live, relative to the project root.
pub const PERMISSIONS_FILE: &str = ".anvil/permissions.toml";

/// Why the permissions file could not be used.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PermissionError {
    /// The file exists but could not be read.
    #[error("cannot read {path}: {detail}")]
    Read {
        /// The file.
        path: PathBuf,
        /// The I/O error text.
        detail: String,
    },
    /// The file is not valid TOML for this schema.
    #[error("cannot parse {path}: {detail}")]
    Parse {
        /// The file.
        path: PathBuf,
        /// The parse error text.
        detail: String,
    },
    /// The file could not be written after an `always` answer.
    #[error("cannot write {path}: {detail}")]
    Write {
        /// The file.
        path: PathBuf,
        /// The I/O error text.
        detail: String,
    },
    /// A rule's glob does not compile.
    #[error("invalid permission rule `{rule}`: {detail}")]
    InvalidRule {
        /// The rule as written.
        rule: String,
        /// Why it is invalid.
        detail: String,
    },
}

/// The on-disk shape of `.anvil/permissions.toml`, shared with the
/// `[permissions]` table of a profile.
pub type PermissionsFile = PermissionRules;

/// What the rules say about one call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Run it.
    Allow,
    /// Prompt first.
    Ask,
    /// Refuse it.
    Deny,
}

impl From<Unmatched> for Decision {
    fn from(value: Unmatched) -> Self {
        match value {
            Unmatched::Allow => Decision::Allow,
            Unmatched::Ask => Decision::Ask,
            Unmatched::Deny => Decision::Deny,
        }
    }
}

/// One parsed rule.
#[derive(Debug, Clone)]
struct Rule {
    /// The tool name.
    tool: String,
    /// The target glob, if the rule has one.
    pattern: Option<GlobMatcher>,
}

impl Rule {
    /// Parses `tool` or `tool:glob`.
    fn parse(text: &str) -> Result<Self, PermissionError> {
        let (tool, glob) = match text.split_once(':') {
            Some((tool, glob)) => (tool.trim(), Some(glob.trim())),
            None => (text.trim(), None),
        };
        if tool.is_empty() {
            return Err(PermissionError::InvalidRule {
                rule: text.to_owned(),
                detail: "empty tool name".to_owned(),
            });
        }
        let pattern = match glob {
            Some(glob) => Some(
                Glob::new(glob)
                    .map_err(|error| PermissionError::InvalidRule {
                        rule: text.to_owned(),
                        detail: error.to_string(),
                    })?
                    .compile_matcher(),
            ),
            None => None,
        };
        Ok(Self {
            tool: tool.to_owned(),
            pattern,
        })
    }

    /// Whether this rule covers the call.
    fn matches(&self, tool: &str, target: Option<&str>) -> bool {
        if self.tool != tool {
            return false;
        }
        match (&self.pattern, target) {
            (None, _) => true,
            (Some(pattern), Some(target)) => pattern.is_match(target),
            (Some(_), None) => false,
        }
    }
}

/// The loaded rules.
#[derive(Debug)]
pub struct Permissions {
    /// Where to write `always` rules; `None` keeps them in memory.
    path: Option<PathBuf>,
    /// The rules as written.
    file: PermissionsFile,
    /// What an unmatched call gets instead of the tool's own default.
    unmatched: Option<Decision>,
    /// Parsed `allow` rules.
    allow: Vec<Rule>,
    /// Parsed `ask` rules.
    ask: Vec<Rule>,
    /// Parsed `deny` rules.
    deny: Vec<Rule>,
}

impl Permissions {
    /// No rules, nothing on disk.
    ///
    /// # Returns
    ///
    /// Empty permissions.
    pub fn empty() -> Self {
        Self {
            path: None,
            file: PermissionsFile::default(),
            unmatched: None,
            allow: Vec::new(),
            ask: Vec::new(),
            deny: Vec::new(),
        }
    }

    /// Builds in-memory permissions from rules that came from configuration,
    /// such as a profile's `[permissions]` table.
    ///
    /// Nothing is written back: an `always` answer lasts for the session.
    ///
    /// # Arguments
    ///
    /// * `rules` - The rules.
    ///
    /// # Returns
    ///
    /// The permissions.
    ///
    /// # Errors
    ///
    /// [`PermissionError::InvalidRule`] for a glob that does not compile.
    pub fn from_rules(rules: &PermissionRules) -> Result<Self, PermissionError> {
        let parse_all = |rules: &[String]| -> Result<Vec<Rule>, PermissionError> {
            rules.iter().map(|rule| Rule::parse(rule)).collect()
        };
        Ok(Self {
            path: None,
            allow: parse_all(&rules.allow)?,
            ask: parse_all(&rules.ask)?,
            deny: parse_all(&rules.deny)?,
            unmatched: rules.unmatched.map(Decision::from),
            file: rules.clone(),
        })
    }

    /// Parses rules from TOML text.
    ///
    /// # Arguments
    ///
    /// * `text` - The file contents.
    /// * `path` - Where `always` rules are written back, if anywhere.
    ///
    /// # Returns
    ///
    /// The permissions.
    ///
    /// # Errors
    ///
    /// [`PermissionError::Parse`] for bad TOML, [`PermissionError::InvalidRule`]
    /// for a glob that does not compile.
    pub fn from_toml(text: &str, path: Option<PathBuf>) -> Result<Self, PermissionError> {
        let file: PermissionsFile =
            toml::from_str(text).map_err(|error| PermissionError::Parse {
                path: path.clone().unwrap_or_default(),
                detail: error.to_string(),
            })?;
        let mut permissions = Self::from_rules(&file)?;
        permissions.path = path;
        Ok(permissions)
    }

    /// Loads `<project_root>/.anvil/permissions.toml`, or empty rules when
    /// the file does not exist.
    ///
    /// # Arguments
    ///
    /// * `project_root` - The project directory.
    ///
    /// # Returns
    ///
    /// The permissions, remembering the path for `always` rules.
    ///
    /// # Errors
    ///
    /// When the file exists but cannot be read or parsed.
    pub fn load(project_root: &Path) -> Result<Self, PermissionError> {
        let path = project_root.join(PERMISSIONS_FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::from_toml(&text, Some(path)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self {
                path: Some(path),
                ..Self::empty()
            }),
            Err(error) => Err(PermissionError::Read {
                path,
                detail: error.to_string(),
            }),
        }
    }

    /// Decides one call: `deny` beats `allow` beats `ask` beats the rules'
    /// own `default`, which beats the tool's `default`.
    ///
    /// # Arguments
    ///
    /// * `tool` - The tool name.
    /// * `target` - The path or command, if the tool has one.
    /// * `default` - Used when no rule matches and the rules set no default.
    ///
    /// # Returns
    ///
    /// The decision.
    pub fn decide(&self, tool: &str, target: Option<&str>, default: Decision) -> Decision {
        self.matched(tool, target)
            .or(self.unmatched)
            .unwrap_or(default)
    }

    /// The decision of the matching rules alone, if any rule matches.
    fn matched(&self, tool: &str, target: Option<&str>) -> Option<Decision> {
        let any = |rules: &[Rule]| rules.iter().any(|rule| rule.matches(tool, target));
        if any(&self.deny) {
            Some(Decision::Deny)
        } else if any(&self.allow) {
            Some(Decision::Allow)
        } else if any(&self.ask) {
            Some(Decision::Ask)
        } else {
            None
        }
    }

    /// Whether an `allow` rule covers the call and no `deny` rule does.
    ///
    /// A `default = "allow"` does not count: only a rule that names the
    /// call can override the blocked-command list.
    ///
    /// # Arguments
    ///
    /// * `tool` - The tool name.
    /// * `target` - The path or command, if the tool has one.
    ///
    /// # Returns
    ///
    /// `true` only for an explicit allow.
    pub fn explicitly_allowed(&self, tool: &str, target: Option<&str>) -> bool {
        self.matched(tool, target) == Some(Decision::Allow)
    }

    /// Adds an `allow` rule and writes the file when one is configured.
    ///
    /// # Arguments
    ///
    /// * `rule` - `tool` or `tool:glob`.
    ///
    /// # Errors
    ///
    /// [`PermissionError::InvalidRule`] when the glob does not compile, or
    /// [`PermissionError::Write`] when the file cannot be written. The rule
    /// is kept in memory in the latter case.
    pub fn add_allow(&mut self, rule: &str) -> Result<(), PermissionError> {
        let parsed = Rule::parse(rule)?;
        if !self.file.allow.iter().any(|existing| existing == rule) {
            self.file.allow.push(rule.to_owned());
            self.allow.push(parsed);
        }
        let Some(path) = &self.path else {
            return Ok(());
        };
        let text = toml::to_string_pretty(&self.file).map_err(|error| PermissionError::Write {
            path: path.clone(),
            detail: error.to_string(),
        })?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|error| PermissionError::Write {
                path: path.clone(),
                detail: error.to_string(),
            })?;
        }
        std::fs::write(path, text).map_err(|error| PermissionError::Write {
            path: path.clone(),
            detail: error.to_string(),
        })
    }

    /// Adds an `allow` rule in memory only, never written to disk.
    ///
    /// The agent loop uses this for its own session folder.
    ///
    /// # Arguments
    ///
    /// * `rule` - `tool` or `tool:glob`.
    ///
    /// # Errors
    ///
    /// [`PermissionError::InvalidRule`] when the glob does not compile.
    pub fn allow_unsaved(&mut self, rule: &str) -> Result<(), PermissionError> {
        self.allow.push(Rule::parse(rule)?);
        Ok(())
    }

    /// The rules as written.
    ///
    /// # Returns
    ///
    /// The file contents.
    pub fn rules(&self) -> &PermissionsFile {
        &self.file
    }
}

/// The rule an `always` answer records.
///
/// For `shell` the first word of the command plus ` *`, so `cargo test`
/// becomes `shell:cargo *`. For every other tool, the bare tool name.
///
/// # Arguments
///
/// * `tool` - The tool name.
/// * `target` - The command or path.
///
/// # Returns
///
/// The rule text.
pub fn always_rule(tool: &str, target: Option<&str>) -> String {
    if tool == "shell"
        && let Some(first) = target.and_then(|command| command.split_whitespace().next())
    {
        return format!("shell:{first} *");
    }
    tool.to_owned()
}

/// What the user is asked about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionRequest {
    /// The tool name.
    pub tool: String,
    /// The path or command, if any.
    pub target: Option<String>,
    /// The full arguments, for display.
    pub arguments: serde_json::Value,
}

/// The user's answer at an `ask` prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptAnswer {
    /// Run this call.
    Yes,
    /// Refuse this call.
    No,
    /// Run it and record an `allow` rule.
    Always,
}

/// Answers `ask` decisions. The CLI implements it with the terminal.
pub trait PermissionPrompt: Send + Sync {
    /// Asks about one call.
    ///
    /// # Arguments
    ///
    /// * `request` - What is about to run.
    ///
    /// # Returns
    ///
    /// The answer.
    fn ask(&self, request: &PermissionRequest) -> PromptAnswer;
}

/// A prompt that always gives the same answer.
#[derive(Debug, Clone, Copy)]
pub struct StaticPrompt(pub PromptAnswer);

impl PermissionPrompt for StaticPrompt {
    fn ask(&self, _request: &PermissionRequest) -> PromptAnswer {
        self.0
    }
}

/// A prompt that replays scripted answers and records the requests.
///
/// Once the script is exhausted it answers `No`.
#[derive(Debug)]
pub struct ScriptedPrompt {
    /// Answers not yet given.
    answers: Mutex<VecDeque<PromptAnswer>>,
    /// Every request seen.
    requests: Mutex<Vec<PermissionRequest>>,
}

impl ScriptedPrompt {
    /// Creates the prompt.
    ///
    /// # Arguments
    ///
    /// * `answers` - Answers in the order they will be given.
    ///
    /// # Returns
    ///
    /// The prompt.
    pub fn new(answers: Vec<PromptAnswer>) -> Self {
        Self {
            answers: Mutex::new(answers.into()),
            requests: Mutex::new(Vec::new()),
        }
    }

    /// Every request seen so far.
    ///
    /// # Returns
    ///
    /// Clones of the requests, oldest first.
    pub fn requests(&self) -> Vec<PermissionRequest> {
        lock_or_recover(&self.requests).clone()
    }
}

impl PermissionPrompt for ScriptedPrompt {
    fn ask(&self, request: &PermissionRequest) -> PromptAnswer {
        lock_or_recover(&self.requests).push(request.clone());
        lock_or_recover(&self.answers)
            .pop_front()
            .unwrap_or(PromptAnswer::No)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Permissions {
        Permissions::from_toml(text, None).expect("parses")
    }

    #[test]
    fn empty_rules_fall_back_to_the_default() {
        let perms = Permissions::empty();
        assert_eq!(
            perms.decide("read_file", Some("a"), Decision::Allow),
            Decision::Allow
        );
        assert_eq!(
            perms.decide("shell", Some("ls"), Decision::Ask),
            Decision::Ask
        );
        assert!(!perms.explicitly_allowed("shell", Some("ls")));
    }

    #[test]
    fn default_deny_refuses_unmatched_calls_without_counting_as_explicit() {
        let perms = parse("allow = [\"shell:cargo *\"]\ndefault = \"deny\"");
        assert_eq!(
            perms.decide("shell", Some("cargo test"), Decision::Ask),
            Decision::Allow
        );
        assert_eq!(
            perms.decide("shell", Some("rm -rf /"), Decision::Ask),
            Decision::Deny
        );
        assert_eq!(
            perms.decide("read_file", Some("a.rs"), Decision::Allow),
            Decision::Deny,
            "the rules' default beats the tool's default"
        );
        let open = parse("default = \"allow\"");
        assert_eq!(
            open.decide("shell", Some("rm -rf /"), Decision::Ask),
            Decision::Allow
        );
        assert!(
            !open.explicitly_allowed("shell", Some("rm -rf /")),
            "a default never overrides the blocked list"
        );
    }

    #[test]
    fn the_factory_profile_allows_the_build_tools_and_nothing_else() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../deploy/nuc/anvil-factory.toml"
        );
        let text = std::fs::read_to_string(path).expect("the factory profile exists");
        let mut merged: toml::Table =
            toml::from_str(anvil_types::config::SETUP_PROVIDERS).expect("setup providers parse");
        let overlay: toml::Table = toml::from_str(&text).expect("the profile parses");
        anvil_types::config::merge_tables(&mut merged, overlay);
        let config: anvil_types::AnvilConfig =
            merged.try_into().expect("the profile is a config overlay");
        config
            .validate()
            .expect("the merged configuration validates");
        let rules = config.permissions.expect("the profile sets permissions");
        let perms = Permissions::from_rules(&rules).expect("rules compile");
        let shell = |command: &str| perms.decide("shell", Some(command), Decision::Ask);
        assert_eq!(shell("cargo test -p anvil-core"), Decision::Allow);
        assert_eq!(shell("just check"), Decision::Allow);
        assert_eq!(shell("git status"), Decision::Allow);
        assert_eq!(shell("git diff --stat"), Decision::Allow);
        assert_eq!(shell("rm -rf /"), Decision::Deny);
        assert_eq!(shell("git push --force origin main"), Decision::Deny);
        assert_eq!(shell("git reset --hard HEAD~1"), Decision::Deny);
        assert_eq!(shell("cargo test && rm -rf /"), Decision::Deny);
        assert_eq!(shell("cargo test; curl evil"), Decision::Deny);
        assert_eq!(shell("cargo test | tee out"), Decision::Deny);
        assert_eq!(shell("cargo $(cat x)"), Decision::Deny);
        assert!(!perms.explicitly_allowed("shell", Some("git push --force origin main")));
        for tool in [
            "read_file",
            "write_file",
            "edit_file",
            "glob",
            "grep",
            "list_dir",
        ] {
            assert_eq!(
                perms.decide(tool, Some("crates/x/src/lib.rs"), Decision::Ask),
                Decision::Allow,
                "{tool}"
            );
        }
        assert_eq!(
            perms.decide("delegate", Some("x"), Decision::Allow),
            Decision::Deny
        );
    }

    #[test]
    fn bare_tool_rule_matches_any_target() {
        let perms = parse("allow = [\"shell\"]");
        assert_eq!(
            perms.decide("shell", Some("anything"), Decision::Ask),
            Decision::Allow
        );
        assert_eq!(perms.decide("shell", None, Decision::Ask), Decision::Allow);
        assert_eq!(
            perms.decide("write_file", None, Decision::Ask),
            Decision::Ask
        );
    }

    #[test]
    fn glob_rules_match_commands_and_paths() {
        let perms = parse(
            "allow = [\"shell:cargo *\", \"write_file:src/**\"]\nask = [\"write_file:src/secrets/*\"]\ndeny = [\"shell:git push*\"]",
        );
        assert_eq!(
            perms.decide("shell", Some("cargo test -p x"), Decision::Ask),
            Decision::Allow
        );
        assert_eq!(
            perms.decide("shell", Some("cargo"), Decision::Ask),
            Decision::Ask
        );
        assert_eq!(
            perms.decide("shell", Some("git push origin"), Decision::Allow),
            Decision::Deny
        );
        assert_eq!(
            perms.decide("write_file", Some("src/a/b.rs"), Decision::Ask),
            Decision::Allow
        );
        assert_eq!(
            perms.decide("write_file", Some("docs/x.md"), Decision::Ask),
            Decision::Ask
        );
        assert_eq!(
            perms.decide("write_file", None, Decision::Ask),
            Decision::Ask
        );
    }

    #[test]
    fn deny_beats_allow_beats_ask() {
        let perms = parse("allow = [\"shell\"]\nask = [\"shell\"]\ndeny = [\"shell:rm *\"]");
        assert_eq!(
            perms.decide("shell", Some("ls"), Decision::Ask),
            Decision::Allow
        );
        assert_eq!(
            perms.decide("shell", Some("rm -r x"), Decision::Ask),
            Decision::Deny
        );
        assert!(!perms.explicitly_allowed("shell", Some("rm -r x")));
        let perms = parse("ask = [\"read_file\"]");
        assert_eq!(
            perms.decide("read_file", Some("a"), Decision::Allow),
            Decision::Ask
        );
    }

    #[test]
    fn invalid_rules_are_rejected() {
        assert!(matches!(
            Permissions::from_toml("allow = [\"shell:[\"]", None),
            Err(PermissionError::InvalidRule { .. })
        ));
        assert!(matches!(
            Permissions::from_toml("allow = [\":x\"]", None),
            Err(PermissionError::InvalidRule { .. })
        ));
        assert!(matches!(
            Permissions::from_toml("allow = \"shell\"", None),
            Err(PermissionError::Parse { .. })
        ));
        assert!(matches!(
            Permissions::from_toml("block = []", None),
            Err(PermissionError::Parse { .. })
        ));
    }

    #[test]
    fn load_missing_file_is_empty_and_add_allow_creates_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut perms = Permissions::load(dir.path()).expect("loads");
        assert!(perms.rules().allow.is_empty());
        perms.add_allow("shell:cargo *").expect("writes");
        perms.add_allow("shell:cargo *").expect("idempotent");
        let path = dir.path().join(PERMISSIONS_FILE);
        let text = std::fs::read_to_string(&path).expect("file exists");
        assert_eq!(text.matches("cargo").count(), 1, "{text}");
        let reloaded = Permissions::load(dir.path()).expect("reloads");
        assert_eq!(
            reloaded.decide("shell", Some("cargo build"), Decision::Ask),
            Decision::Allow
        );
    }

    #[test]
    fn load_reports_parse_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(PERMISSIONS_FILE);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, "allow = [1]").expect("write");
        assert!(matches!(
            Permissions::load(dir.path()),
            Err(PermissionError::Parse { .. })
        ));
    }

    #[test]
    fn always_rule_shapes() {
        assert_eq!(
            always_rule("shell", Some("cargo test -p x")),
            "shell:cargo *"
        );
        assert_eq!(always_rule("shell", Some("  ")), "shell");
        assert_eq!(always_rule("shell", None), "shell");
        assert_eq!(always_rule("write_file", Some("src/a.rs")), "write_file");
    }

    #[test]
    fn scripted_prompt_replays_then_declines() {
        let prompt = ScriptedPrompt::new(vec![PromptAnswer::Always]);
        let request = PermissionRequest {
            tool: "shell".to_owned(),
            target: Some("ls".to_owned()),
            arguments: serde_json::json!({"command": "ls"}),
        };
        assert_eq!(prompt.ask(&request), PromptAnswer::Always);
        assert_eq!(prompt.ask(&request), PromptAnswer::No);
        assert_eq!(prompt.requests().len(), 2);
        assert_eq!(
            StaticPrompt(PromptAnswer::Yes).ask(&request),
            PromptAnswer::Yes
        );
    }
}
