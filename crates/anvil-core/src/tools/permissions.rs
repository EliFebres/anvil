//!
//! A rule is `tool` or `tool:glob`. The glob matches the call's target: the
//! `path` argument for file tools, the `command` for `shell`. When several
//! rules match, `deny` wins, then `allow`, then `ask`, then the tool's own
//! default.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Component, Path, PathBuf};
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
    /// A tool name given to pre-approve or approve a tool is unusable.
    #[error("invalid tool name `{name}`: {reason}")]
    InvalidToolName {
        /// The name as given.
        name: String,
        /// Why it is invalid.
        reason: &'static str,
    },
    /// The workspace root of an approval cannot be resolved on disk.
    #[error("cannot resolve the workspace root {path}: {detail}")]
    UnresolvableRoot {
        /// The root as given.
        path: PathBuf,
        /// The I/O error text.
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
        self.rule_decision(tool, target).unwrap_or(default)
    }

    /// What the rules alone say about a call: the matching rules, then the
    /// rules' own `default`; `None` leaves it to the tool's default.
    ///
    /// # Arguments
    ///
    /// * `tool` - The tool name.
    /// * `target` - The path or command, if the tool has one.
    ///
    /// # Returns
    ///
    /// The rules' decision, if they make one.
    pub fn rule_decision(&self, tool: &str, target: Option<&str>) -> Option<Decision> {
        self.matched(tool, target).or(self.unmatched)
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

impl PermissionRequest {
    /// Whether the prompt should offer `always`, which saves a rule to disk.
    ///
    /// Only `shell` offers it: its saved rule is a command prefix, which is
    /// narrower than the session approval every prompt offers.
    ///
    /// # Returns
    ///
    /// `true` for `shell`.
    pub fn offers_always(&self) -> bool {
        self.tool == "shell"
    }
}

/// The user's answer at an `ask` prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptAnswer {
    /// Run this call.
    Yes,
    /// Refuse this call.
    No,
    /// Run it and stop asking about this tool for the rest of the session,
    /// within the workspace root for path tools.
    Session,
    /// Run it and record an `allow` rule.
    Always,
}

/// Tools the user approved for this session, held in memory only.
///
/// One instance is shared by every registry of a session, including the
/// ones sub-agents get, so an approval is never lost between them; a
/// registry holds its lock while prompting, so two registries never ask at
/// once and the second sees the first one's `session` answer. An
/// approval given at a prompt is bound to the workspace root it was given
/// in, so a registry working in another root still prompts.
#[derive(Debug, Clone, Default)]
pub struct SessionApprovals {
    /// Approved at a prompt, with the canonical workspace roots each tool
    /// was approved in.
    session: BTreeMap<String, BTreeSet<PathBuf>>,
    /// Approved by `--allow-tool` or `permissions.allow_tools`, any target.
    preapproved: BTreeSet<String>,
}

impl SessionApprovals {
    /// Approvals seeded with pre-approved tools.
    ///
    /// # Arguments
    ///
    /// * `tools` - Tool names from `--allow-tool` and `permissions.allow_tools`.
    ///
    /// # Returns
    ///
    /// The approvals.
    ///
    /// # Errors
    ///
    /// [`PermissionError::InvalidToolName`] for an empty name, or one with
    /// whitespace or a `:`.
    pub fn preapproved<I, S>(tools: I) -> Result<Self, PermissionError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut preapproved = BTreeSet::new();
        for tool in tools {
            let tool = tool.as_ref();
            validate_tool_name(tool)?;
            preapproved.insert(tool.to_owned());
        }
        Ok(Self {
            session: BTreeMap::new(),
            preapproved,
        })
    }

    /// Records a `session` answer for a tool in one workspace root.
    ///
    /// # Arguments
    ///
    /// * `tool` - The tool name.
    /// * `root` - The workspace root the call ran in.
    ///
    /// # Errors
    ///
    /// [`PermissionError::InvalidToolName`] for an invalid name, or
    /// [`PermissionError::UnresolvableRoot`] when the root cannot be
    /// resolved on disk. Nothing is recorded in either case.
    pub fn approve(&mut self, tool: &str, root: &Path) -> Result<(), PermissionError> {
        validate_tool_name(tool)?;
        let root =
            std::fs::canonicalize(root).map_err(|error| PermissionError::UnresolvableRoot {
                path: root.to_path_buf(),
                detail: error.to_string(),
            })?;
        self.session
            .entry(tool.to_owned())
            .or_default()
            .insert(root);
        Ok(())
    }

    /// Whether an existing approval covers the call, so no prompt is needed.
    ///
    /// A prompted approval covers only calls in the root it was given in;
    /// for path tools the target must also resolve, following symlinks,
    /// to a location inside that root.
    ///
    /// # Arguments
    ///
    /// * `tool` - The tool name.
    /// * `target` - The call's path or command, if any.
    /// * `root` - The workspace root of this call.
    /// * `target_is_path` - Whether `target` is a file path to keep inside `root`.
    ///
    /// # Returns
    ///
    /// `true` when the call may run without asking.
    pub fn covers(
        &self,
        tool: &str,
        target: Option<&str>,
        root: &Path,
        target_is_path: bool,
    ) -> bool {
        if self.preapproved.contains(tool) {
            return true;
        }
        let Some(roots) = self.session.get(tool) else {
            return false;
        };
        let Ok(root) = std::fs::canonicalize(root) else {
            return false;
        };
        if !roots.contains(&root) {
            return false;
        }
        if !target_is_path {
            return true;
        }
        target
            .and_then(|target| resolve_path(&root, target))
            .is_some_and(|resolved| resolved.starts_with(&root))
    }
}

/// Checks a tool name given to [`SessionApprovals`].
fn validate_tool_name(tool: &str) -> Result<(), PermissionError> {
    let reason = if tool.is_empty() {
        "it is empty"
    } else if tool.chars().any(char::is_whitespace) {
        "it contains whitespace"
    } else if tool.contains(':') {
        "it contains `:`, which separates a rule's tool from its glob"
    } else {
        return Ok(());
    };
    Err(PermissionError::InvalidToolName {
        name: tool.to_owned(),
        reason,
    })
}

/// Resolves `target` against `root` the way the file system will, so a
/// symlink cannot make an outside location look inside the root and a
/// different spelling of an existing directory (such as other letter case
/// on Windows) cannot make an inside location look outside.
///
/// Components are walked one at a time and every one is checked on disk:
/// an existing component is replaced by its canonical path (following
/// symlinks and taking the file system's own spelling) before the next
/// component (including `..`) is applied. A missing component is kept as
/// written, which keeps new files resolvable; a later `..` that walks back
/// out of it lands on a checked path again, so `new/../link` still follows
/// `link`.
///
/// # Arguments
///
/// * `root` - Directory relative targets resolve against; should be canonical.
/// * `target` - The path as the model wrote it.
///
/// # Returns
///
/// The resolved path, or `None` when it cannot be resolved safely (a
/// dangling or looping symlink, or an unreadable component).
pub fn resolve_path(root: &Path, target: &str) -> Option<PathBuf> {
    let mut resolved = PathBuf::new();
    for component in root.join(target).components() {
        match component {
            Component::Prefix(_) => resolved.push(component),
            Component::RootDir => {
                resolved.push(component);
                // On Windows this turns `C:\` into the `\\?\C:\` form that
                // a canonical root uses, so prefixes compare equal.
                resolved = std::fs::canonicalize(&resolved).unwrap_or(resolved);
            }
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            Component::Normal(name) => {
                resolved.push(name);
                match std::fs::symlink_metadata(&resolved) {
                    Ok(_) => {
                        resolved = std::fs::canonicalize(&resolved).ok()?;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(_) => return None,
                }
            }
        }
    }
    Some(resolved)
}

/// The scope a permission decision ended with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionScope {
    /// Allowed for this call only, at a prompt.
    Once,
    /// Allowed for the rest of the session: a `session` or `always` answer,
    /// a session approval, or a pre-approved tool.
    Session,
    /// Allowed by a permission rule (an `allow` rule or `default = "allow"`).
    Rule,
    /// Refused, by the user at a prompt or by a `deny` rule.
    Deny,
    /// Refused because the call is on the blocked list and no explicit
    /// `allow` rule overrides it.
    Blocked,
}

/// One permission decision, without paths, commands or arguments.
///
/// | how it was decided              | `scope`   | `covered` |
/// |---------------------------------|-----------|-----------|
/// | prompt: yes                     | `Once`    | `false`   |
/// | prompt: session or always       | `Session` | `false`   |
/// | prompt: no                      | `Deny`    | `false`   |
/// | session approval / pre-approved | `Session` | `true`    |
/// | `allow` rule or default         | `Rule`    | `true`    |
/// | `deny` rule or default          | `Deny`    | `false`   |
/// | blocked list                    | `Blocked` | `false`   |
///
/// `covered` means an existing approval (a session approval, a pre-approved
/// tool or an `allow` rule) let the call run without a prompt. A refusal is
/// never covered, so a prompted decision for a tool that already had an
/// approval stands out as a re-prompt. A tool's built-in `allow` default
/// (read-only tools) is not a permission decision and is not reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionDecision {
    /// The tool name.
    pub tool: String,
    /// The scope the decision ended with.
    pub scope: DecisionScope,
    /// Whether an existing approval covered the call.
    pub covered: bool,
}

/// Hears every permission decision. The CLI records them as telemetry.
pub trait PermissionObserver: Send + Sync {
    /// Called once per decision.
    ///
    /// # Arguments
    ///
    /// * `decision` - What was decided.
    fn decided(&self, decision: &PermissionDecision);
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
        assert_eq!(shell("cargo test 2>&1"), Decision::Deny);
        assert_eq!(shell("cargo test > out.txt"), Decision::Deny);
        assert_eq!(shell("cargo run < input"), Decision::Deny);
        assert_eq!(shell("cargo $(cat x)"), Decision::Deny);
        assert!(!perms.explicitly_allowed("shell", Some("git push --force origin main")));
        for tool in [
            "read_file",
            "write_file",
            "edit_file",
            "glob",
            "grep",
            "list_dir",
            "outline",
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

    #[test]
    fn resolve_path_folds_spellings_of_one_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = std::fs::canonicalize(dir.path()).expect("canonical");
        std::fs::create_dir(root.join("src")).expect("mkdir");
        std::fs::write(root.join("src").join("a.txt"), "x").expect("write");
        let expected = root.join("src").join("a.txt");
        let absolute = expected.to_str().expect("utf-8").to_owned();
        for spelling in ["src/a.txt", "./src/./a.txt", "src/../src/a.txt", &absolute] {
            assert_eq!(
                resolve_path(&root, spelling),
                Some(expected.clone()),
                "{spelling}"
            );
        }
        assert_eq!(
            resolve_path(&root, "new/dir/b.txt"),
            Some(root.join("new").join("dir").join("b.txt")),
            "a write target that does not exist yet still resolves"
        );
        assert_eq!(
            resolve_path(&root, "src/new.txt"),
            Some(root.join("src").join("new.txt")),
            "a new file under an existing directory resolves under the canonical root"
        );
        let outside = resolve_path(&root, "../other.txt").expect("resolves");
        assert!(!outside.starts_with(&root), "{}", outside.display());
    }

    #[cfg(windows)]
    #[test]
    fn differently_capitalized_workspace_path_is_inside_the_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = std::fs::canonicalize(dir.path()).expect("canonical");
        let root = base.join("Repo");
        std::fs::create_dir(&root).expect("mkdir");
        let root = std::fs::canonicalize(&root).expect("canonical root");
        std::fs::write(root.join("file.txt"), "x").expect("write");
        let mut approvals = SessionApprovals::default();
        approvals.approve("edit_file", &root).expect("approve");
        let lowered = base.join("repo");
        let existing = lowered.join("file.txt");
        let existing = existing.to_str().expect("utf-8");
        let missing = lowered.join("new.txt");
        let missing = missing.to_str().expect("utf-8");
        assert_eq!(
            resolve_path(&root, existing),
            Some(root.join("file.txt")),
            "takes the file system's spelling"
        );
        assert!(approvals.covers("edit_file", Some(existing), &root, true));
        assert!(approvals.covers("edit_file", Some(missing), &root, true));
    }

    /// A workspace with `a.txt`, plus a directory outside it.
    fn workspace() -> (tempfile::TempDir, PathBuf, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = std::fs::canonicalize(dir.path()).expect("canonical");
        std::fs::write(root.join("a.txt"), "x").expect("write");
        let outside = tempfile::tempdir().expect("outside");
        (dir, root, outside)
    }

    #[test]
    fn session_approval_covers_only_targets_inside_the_root() {
        let (_dir, root, _outside) = workspace();
        let mut approvals = SessionApprovals::default();
        assert!(!approvals.covers("edit_file", Some("a.txt"), &root, true));
        approvals.approve("edit_file", &root).expect("approve");
        let absolute = root.join("c.txt");
        assert!(approvals.covers("edit_file", Some("a.txt"), &root, true));
        assert!(approvals.covers("edit_file", Some("./src/b.txt"), &root, true));
        assert!(approvals.covers("edit_file", absolute.to_str(), &root, true));
        assert!(!approvals.covers("edit_file", Some("../x.txt"), &root, true));
        assert!(!approvals.covers("edit_file", Some("/etc/hosts"), &root, true));
        let sibling = format!("{}-sibling/a", root.display());
        assert!(!approvals.covers("edit_file", Some(&sibling), &root, true));
        assert!(!approvals.covers("edit_file", None, &root, true));
        assert!(!approvals.covers("write_file", Some("a.txt"), &root, true));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_that_escape_the_root_are_not_covered() {
        let (_dir, root, outside) = workspace();
        let outside_dir = std::fs::canonicalize(outside.path()).expect("canonical");
        std::fs::create_dir(outside_dir.join("deep")).expect("mkdir");
        std::os::unix::fs::symlink(&outside_dir, root.join("escape")).expect("symlink");
        std::os::unix::fs::symlink(outside_dir.join("deep"), root.join("deep_link"))
            .expect("symlink");
        std::os::unix::fs::symlink(outside_dir.join("gone.txt"), root.join("dangling"))
            .expect("symlink");
        std::os::unix::fs::symlink(root.join("a.txt"), root.join("inner")).expect("symlink");
        let mut approvals = SessionApprovals::default();
        approvals.approve("edit_file", &root).expect("approve");
        for escaping in [
            "escape/x.txt",
            "escape/new/dir/x.txt",
            "./escape/../escape/x.txt",
            "deep_link/../x.txt",
            "new/../escape/file.txt",
            "new/deeper/../../escape/file.txt",
            "dangling",
        ] {
            assert!(
                !approvals.covers("edit_file", Some(escaping), &root, true),
                "{escaping}"
            );
        }
        assert!(approvals.covers("edit_file", Some("inner"), &root, true));
        assert!(approvals.covers("edit_file", Some("./inner"), &root, true));
    }

    #[test]
    fn session_approval_is_bound_to_the_root_it_was_given_in() {
        let (_dir, root, other) = workspace();
        let mut approvals = SessionApprovals::default();
        approvals.approve("shell", &root).expect("approve");
        assert!(approvals.covers("shell", Some("ls"), &root, false));
        assert!(!approvals.covers("shell", Some("ls"), other.path(), false));
        assert!(!approvals.covers("shell", Some("ls"), Path::new("/no/such/root"), false));
    }

    #[test]
    fn preapproved_tools_cover_every_target() {
        let (_dir, root, _outside) = workspace();
        let approvals = SessionApprovals::preapproved(["edit_file"]).expect("valid");
        assert!(approvals.covers("edit_file", Some("/etc/hosts"), &root, true));
        assert!(approvals.covers("edit_file", None, Path::new("/elsewhere"), true));
        assert!(!approvals.covers("shell", Some("ls"), &root, false));
    }

    #[test]
    fn invalid_tool_names_are_rejected_at_both_boundaries() {
        let (_dir, root, _outside) = workspace();
        for bad in ["", " ", "edit file", " edit_file", "shell:cargo *"] {
            assert!(
                matches!(
                    SessionApprovals::preapproved([bad]),
                    Err(PermissionError::InvalidToolName { .. })
                ),
                "{bad:?}"
            );
            let mut approvals = SessionApprovals::default();
            assert!(
                matches!(
                    approvals.approve(bad, &root),
                    Err(PermissionError::InvalidToolName { .. })
                ),
                "{bad:?}"
            );
            assert!(!approvals.covers(bad, None, &root, false));
        }
        let error = SessionApprovals::preapproved(["edit_file", ""]).expect_err("empty");
        assert_eq!(error.to_string(), "invalid tool name ``: it is empty");
        let mut approvals = SessionApprovals::default();
        assert!(matches!(
            approvals.approve("edit_file", Path::new("/no/such/root")),
            Err(PermissionError::UnresolvableRoot { .. })
        ));
    }

    #[test]
    fn rule_decision_ignores_the_tool_default() {
        let perms = parse("allow = [\"edit_file\"]\ndeny = [\"shell:rm *\"]");
        assert_eq!(
            perms.rule_decision("edit_file", None),
            Some(Decision::Allow)
        );
        assert_eq!(
            perms.rule_decision("shell", Some("rm -rf x")),
            Some(Decision::Deny)
        );
        assert_eq!(perms.rule_decision("read_file", Some("a")), None);
        let defaulted = parse("default = \"ask\"");
        assert_eq!(
            defaulted.rule_decision("read_file", Some("a")),
            Some(Decision::Ask)
        );
    }

    #[test]
    fn only_shell_offers_always() {
        let request = |tool: &str| PermissionRequest {
            tool: tool.to_owned(),
            target: None,
            arguments: serde_json::json!({}),
        };
        assert!(request("shell").offers_always());
        assert!(!request("edit_file").offers_always());
    }
}
