//! Tool permission rules.
//!
//! A rule is `tool` or `tool:glob`. The glob matches the call's target: the
//! `path` argument for file tools, the `command` for `shell`. When several
//! rules match, `deny` wins, then a tightening `ask` (from the repository
//! or a policy), then `allow`, then the user's own `ask`, then the tool's
//! own default.
//!
//! The user's rules come from their configuration or profile, or from
//! their own per-repository file under `~/.anvil/permissions/`. A
//! repository can only add rules through [`Permissions::tighten`].

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

use anvil_types::shell::{CommandPattern, split_command};
use anvil_types::{PermissionRules, Unmatched};
use globset::{Glob, GlobMatcher};
use thiserror::Error;

use crate::provider::lock_or_recover;

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

/// The on-disk shape of a permissions file, shared with the
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

/// Who wrote a permission rule or a `shell` allowlist layer, strictest
/// first, so the smallest origin among matching rules is the one named.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RuleOrigin {
    /// The managed policy.
    Policy,
    /// The repository's `.anvil/` files.
    Project,
    /// The user: their configuration, profile or per-repository file.
    User,
    /// anvil itself, such as the rule that lets the loop write its own
    /// session folder.
    Builtin,
}

/// Where a permission decision came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionSource {
    /// A permission rule, or the rules' own `default`.
    Rule(RuleOrigin),
    /// A `shell` allowlist layer (`shell_programs` or `[shell] allow`).
    ShellAllowlist(RuleOrigin),
    /// The built-in blocked-command list, or the argument checks an
    /// allowlist adds.
    BlockedList,
    /// An approval given earlier in the session, or `--allow-tool`.
    SessionApproval,
    /// A person answered a prompt.
    Prompt,
    /// `--yes` answered the prompt.
    YesFlag,
    /// Nobody could be asked, so the prompt answered `no`, as for a
    /// subagent without `permissions: inherit`.
    Unattended,
}

impl DecisionSource {
    /// The name audit records use.
    ///
    /// # Returns
    ///
    /// Such as `policy_rule`, `user_shell_allowlist`, `prompt` or `yes_flag`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rule(RuleOrigin::Policy) => "policy_rule",
            Self::Rule(RuleOrigin::Project) => "project_rule",
            Self::Rule(RuleOrigin::User) => "user_rule",
            Self::Rule(RuleOrigin::Builtin) => "builtin_rule",
            Self::ShellAllowlist(RuleOrigin::Policy) => "policy_shell_allowlist",
            Self::ShellAllowlist(RuleOrigin::Project) => "project_shell_allowlist",
            Self::ShellAllowlist(RuleOrigin::User) => "user_shell_allowlist",
            Self::ShellAllowlist(RuleOrigin::Builtin) => "builtin_shell_allowlist",
            Self::BlockedList => "blocked_list",
            Self::SessionApproval => "session_approval",
            Self::Prompt => "prompt",
            Self::YesFlag => "yes_flag",
            Self::Unattended => "unattended",
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
    /// Who wrote it.
    origin: RuleOrigin,
}

impl Rule {
    /// Parses `tool` or `tool:glob`.
    fn parse(text: &str, origin: RuleOrigin) -> Result<Self, PermissionError> {
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
            origin,
        })
    }

    /// Whether this rule has a pattern whose text contains `phrase`, so it
    /// names a blocked command rather than covering it by accident.
    fn names(&self, phrase: &str) -> bool {
        self.pattern.as_ref().is_some_and(|pattern| {
            pattern
                .glob()
                .glob()
                .to_ascii_lowercase()
                .contains(&phrase.to_ascii_lowercase())
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
    /// Parsed `deny` rules, the user's and any added by [`Permissions::tighten`].
    deny: Vec<Rule>,
    /// `ask` rules added by [`Permissions::tighten`]; they beat `allow`.
    forced_ask: Vec<Rule>,
    /// `allow` rules of a trusted repository, added by
    /// [`Permissions::allow_from_project`]; they only decide a call no
    /// other rule decides.
    project_allow: Vec<Rule>,
    /// The allowlists `shell` is limited to. A command must match a
    /// pattern of every layer; no layer means no limit.
    shell_allow: Vec<AllowLayer>,
}

/// One layer's `shell` allowlist: a `shell_programs` list or a `[shell]
/// allow` list, from the user, a repository or a policy.
#[derive(Debug, Clone)]
struct AllowLayer {
    /// Where the layer came from, for messages.
    source: String,
    /// Who wrote it.
    origin: RuleOrigin,
    /// The patterns; a command must match one of them.
    patterns: Vec<CommandPattern>,
}

impl AllowLayer {
    /// A layer from `shell_programs`: each program with any arguments.
    fn from_programs(
        programs: &[String],
        source: &str,
        origin: RuleOrigin,
    ) -> Result<Self, PermissionError> {
        let patterns: Vec<String> = programs
            .iter()
            .map(|program| format!("{} *", program.trim()))
            .collect();
        Self::from_patterns(&patterns, source, origin)
    }

    /// A layer from argument-prefix patterns such as `cargo test *`.
    fn from_patterns(
        patterns: &[String],
        source: &str,
        origin: RuleOrigin,
    ) -> Result<Self, PermissionError> {
        let patterns = patterns
            .iter()
            .map(|text| {
                CommandPattern::parse(text).map_err(|detail| PermissionError::InvalidRule {
                    rule: text.clone(),
                    detail,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            source: source.to_owned(),
            origin,
            patterns,
        })
    }
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
            forced_ask: Vec::new(),
            project_allow: Vec::new(),
            shell_allow: Vec::new(),
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
    /// [`PermissionError::InvalidRule`] for a glob that does not compile or
    /// a `shell_programs` entry that is not a program name.
    pub fn from_rules(rules: &PermissionRules) -> Result<Self, PermissionError> {
        let parse_all = |rules: &[String]| -> Result<Vec<Rule>, PermissionError> {
            rules
                .iter()
                .map(|rule| Rule::parse(rule, RuleOrigin::User))
                .collect()
        };
        let shell_allow = if rules.shell_programs.is_empty() {
            Vec::new()
        } else {
            vec![AllowLayer::from_programs(
                &rules.shell_programs,
                "your shell_programs",
                RuleOrigin::User,
            )?]
        };
        Ok(Self {
            path: None,
            allow: parse_all(&rules.allow)?,
            ask: parse_all(&rules.ask)?,
            deny: parse_all(&rules.deny)?,
            forced_ask: Vec::new(),
            project_allow: Vec::new(),
            shell_allow,
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

    /// Loads the user's own rules file, or empty rules when it does not exist.
    ///
    /// # Arguments
    ///
    /// * `path` - The file, normally `~/.anvil/permissions/<repository>.toml`.
    ///
    /// # Returns
    ///
    /// The permissions, remembering the path for `always` rules.
    ///
    /// # Errors
    ///
    /// When the file exists but cannot be read or parsed.
    pub fn load_file(path: PathBuf) -> Result<Self, PermissionError> {
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

    /// Adds rules that can only make a call stricter. Each `deny` refuses a
    /// call even when an `allow` covers it; each `ask` makes a call prompt
    /// even when an `allow` covers it. They are never written to disk.
    ///
    /// The repository's own rules and a policy's rules come in this way, so
    /// neither can loosen what the user decided.
    ///
    /// # Arguments
    ///
    /// * `deny` - Rules for calls to refuse.
    /// * `ask` - Rules for calls to ask about.
    /// * `shell_programs` - Programs `shell` is limited to; empty adds no
    ///   limit. It is one more allowlist layer, so with a limit already in
    ///   place only commands both allow stay allowed.
    ///
    /// # Errors
    ///
    /// [`PermissionError::InvalidRule`] for a glob that does not compile or
    /// a program that is not a plain name.
    pub fn tighten(
        &mut self,
        deny: &[String],
        ask: &[String],
        shell_programs: &[String],
    ) -> Result<(), PermissionError> {
        self.tighten_as(RuleOrigin::Project, deny, ask, shell_programs)
    }

    /// [`Permissions::tighten`] with rules whose author is `origin`, such
    /// as the managed policy, so decisions name where they came from.
    ///
    /// # Arguments
    ///
    /// * `origin` - Who wrote the rules.
    /// * `deny` - Rules for calls to refuse.
    /// * `ask` - Rules for calls to ask about.
    /// * `shell_programs` - Programs `shell` is limited to; empty adds no
    ///   limit.
    ///
    /// # Errors
    ///
    /// [`PermissionError::InvalidRule`] for a glob that does not compile or
    /// a program that is not a plain name.
    pub fn tighten_as(
        &mut self,
        origin: RuleOrigin,
        deny: &[String],
        ask: &[String],
        shell_programs: &[String],
    ) -> Result<(), PermissionError> {
        for rule in deny {
            self.deny.push(Rule::parse(rule, origin)?);
        }
        for rule in ask {
            self.forced_ask.push(Rule::parse(rule, origin)?);
        }
        if !shell_programs.is_empty() {
            self.shell_allow.push(AllowLayer::from_programs(
                shell_programs,
                "the repository's shell_programs",
                origin,
            )?);
        }
        Ok(())
    }

    /// Adds a `[shell] allow` list: a command must also match one of these
    /// argument-prefix patterns. Each call adds a layer, so a later list can
    /// only narrow what earlier ones allow.
    ///
    /// # Arguments
    ///
    /// * `patterns` - Patterns such as `cargo test *`.
    /// * `source` - Where they are set, for messages.
    ///
    /// # Errors
    ///
    /// [`PermissionError::InvalidRule`] for a pattern that does not parse.
    pub fn limit_shell(
        &mut self,
        patterns: &[String],
        source: &str,
    ) -> Result<(), PermissionError> {
        self.limit_shell_as(patterns, source, RuleOrigin::User)
    }

    /// [`Permissions::limit_shell`] with a layer whose author is `origin`.
    ///
    /// # Arguments
    ///
    /// * `patterns` - Patterns such as `cargo test *`.
    /// * `source` - Where they are set, for messages.
    /// * `origin` - Who wrote them.
    ///
    /// # Errors
    ///
    /// [`PermissionError::InvalidRule`] for a pattern that does not parse.
    pub fn limit_shell_as(
        &mut self,
        patterns: &[String],
        source: &str,
        origin: RuleOrigin,
    ) -> Result<(), PermissionError> {
        self.shell_allow
            .push(AllowLayer::from_patterns(patterns, source, origin)?);
        Ok(())
    }

    /// Whether any layer limits `shell` to an allowlist, so commands must be
    /// started as an argument vector without a shell interpreter.
    ///
    /// # Returns
    ///
    /// `true` under `shell_programs` or `[shell] mode = "allowlist"`.
    pub fn has_shell_allowlist(&self) -> bool {
        !self.shell_allow.is_empty()
    }

    /// Adds the `allow` rules of a repository the user trusts. They are
    /// weaker than every other rule: a call any `deny`, `ask` or `allow`
    /// rule of the user's or the repository's matches is decided by that
    /// rule, and so is a call the user's own `default` covers; a
    /// repository's allow only replaces the tool's own default. They never lift the blocked-command list or a
    /// `shell_programs` limit, and they are never written to disk.
    ///
    /// # Arguments
    ///
    /// * `allow` - The repository's `allow` rules.
    ///
    /// # Errors
    ///
    /// [`PermissionError::InvalidRule`] for a glob that does not compile.
    pub fn allow_from_project(&mut self, allow: &[String]) -> Result<(), PermissionError> {
        for rule in allow {
            self.project_allow
                .push(Rule::parse(rule, RuleOrigin::Project)?);
        }
        Ok(())
    }

    /// Why a `shell` command is refused under an allowlist (`shell_programs`
    /// or `[shell] mode = "allowlist"`).
    ///
    /// The command is split by the conservative parser
    /// ([`anvil_types::shell::split_command`]): anything that chains,
    /// redirects, substitutes or groups commands in sh, PowerShell or cmd is
    /// refused, because it would run a second program. The program must be
    /// named as found on `PATH`, not by a path, and the words must match a
    /// pattern of every layer.
    ///
    /// # Arguments
    ///
    /// * `command` - The command line.
    ///
    /// # Returns
    ///
    /// The reason, or `None` when there is no allowlist or the command fits.
    pub fn shell_refusal(&self, command: &str) -> Option<String> {
        self.shell_refusal_by(command).map(|(reason, _)| reason)
    }

    /// [`Permissions::shell_refusal`] with who wrote the layer that
    /// refused. A command the allowlist mode refuses as a whole (an
    /// operator, a path, a line that does not split) is attributed to the
    /// strictest author among the layers.
    ///
    /// # Arguments
    ///
    /// * `command` - The command line.
    ///
    /// # Returns
    ///
    /// The reason and the origin, or `None` when there is no allowlist or
    /// the command fits.
    pub fn shell_refusal_by(&self, command: &str) -> Option<(String, RuleOrigin)> {
        let strictest = self.shell_allow.iter().map(|layer| layer.origin).min()?;
        if let Some(operator) = SHELL_OPERATORS.iter().find(|op| command.contains(**op)) {
            return Some((
                format!(
                    "`{}` is not allowed: under a shell allowlist a command is one program with plain arguments",
                    operator.escape_debug()
                ),
                strictest,
            ));
        }
        let argv = match split_command(command) {
            Ok(argv) => argv,
            Err(reason) => return Some((reason, strictest)),
        };
        let program = &argv[0];
        if program.contains(['/', '\\']) {
            return Some((
                format!(
                    "`{program}` is a path; under a shell allowlist name a program found on PATH"
                ),
                strictest,
            ));
        }
        let refused = self
            .shell_allow
            .iter()
            .find(|layer| !layer.patterns.iter().any(|pattern| pattern.matches(&argv)))?;
        let patterns: Vec<String> = refused.patterns.iter().map(CommandPattern::text).collect();
        Some((
            format!(
                "`{command}` is not one of the allowed commands of {} ({})",
                refused.source,
                patterns.join(", ")
            ),
            refused.origin,
        ))
    }

    /// Whether a blocked command is allowed by name: a user `allow` rule
    /// with a pattern that matches the command and whose text names the
    /// blocked phrase (`shell:curl *` names `curl`; a bare `shell` or
    /// `shell:*` does not), or, under an allowlist, a matching pattern in
    /// every layer that names it (`git push *` names `git push`).
    ///
    /// # Arguments
    ///
    /// * `tool` - The tool name.
    /// * `target` - The command.
    /// * `phrase` - What the blocked list matched, such as `git push`.
    ///
    /// # Returns
    ///
    /// `true` when the block is lifted.
    pub fn allows_by_name(&self, tool: &str, target: Option<&str>, phrase: &str) -> bool {
        let by_rule = self.explicitly_allowed(tool, target)
            && self
                .allow
                .iter()
                .any(|rule| rule.matches(tool, target) && rule.names(phrase));
        if by_rule {
            return true;
        }
        if self.shell_allow.is_empty()
            || self.matched(tool, target).map(|(decision, _)| decision) == Some(Decision::Deny)
        {
            return false;
        }
        let Some(argv) = target.and_then(|command| split_command(command).ok()) else {
            return false;
        };
        let phrase = phrase.to_ascii_lowercase();
        self.shell_allow.iter().all(|layer| {
            layer.patterns.iter().any(|pattern| {
                pattern.matches(&argv) && pattern.text().to_ascii_lowercase().contains(&phrase)
            })
        })
    }

    /// Decides one call: `deny` beats a tightening `ask`, which beats
    /// `allow`, which beats `ask`, then the rules' own `default`, then the
    /// tool's `default`.
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
        self.ruling(tool, target).map(|(decision, _)| decision)
    }

    /// [`Permissions::rule_decision`] with who wrote the rule that decided.
    /// The rules' own `default` is the user's.
    ///
    /// # Arguments
    ///
    /// * `tool` - The tool name.
    /// * `target` - The path or command, if the tool has one.
    ///
    /// # Returns
    ///
    /// The decision and its origin, if the rules make one.
    pub fn ruling(&self, tool: &str, target: Option<&str>) -> Option<(Decision, RuleOrigin)> {
        self.matched(tool, target)
            .or(self.unmatched.map(|decision| (decision, RuleOrigin::User)))
            .or_else(|| {
                self.project_allow
                    .iter()
                    .find(|rule| rule.matches(tool, target))
                    .map(|rule| (Decision::Allow, rule.origin))
            })
    }

    /// The decision of the matching rules alone, if any rule matches, with
    /// the strictest author among the rules that decided.
    fn matched(&self, tool: &str, target: Option<&str>) -> Option<(Decision, RuleOrigin)> {
        let first = |rules: &[Rule]| {
            rules
                .iter()
                .filter(|rule| rule.matches(tool, target))
                .map(|rule| rule.origin)
                .min()
        };
        if let Some(origin) = first(&self.deny) {
            Some((Decision::Deny, origin))
        } else if let Some(origin) = first(&self.forced_ask) {
            Some((Decision::Ask, origin))
        } else if let Some(origin) = first(&self.allow) {
            Some((Decision::Allow, origin))
        } else {
            first(&self.ask).map(|origin| (Decision::Ask, origin))
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
        self.matched(tool, target).map(|(decision, _)| decision) == Some(Decision::Allow)
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
        let parsed = Rule::parse(rule, RuleOrigin::User)?;
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
            anvil_types::private_fs::create_dir_all(dir).map_err(|error| {
                PermissionError::Write {
                    path: path.clone(),
                    detail: error.to_string(),
                }
            })?;
        }
        anvil_types::private_fs::write(path, text).map_err(|error| PermissionError::Write {
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
        self.allow.push(Rule::parse(rule, RuleOrigin::Builtin)?);
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

/// Text that chains, redirects or substitutes commands in sh, PowerShell or
/// cmd: `&&`, `||`, `;`, `|`, `&` (also PowerShell's call operator), `>`,
/// `<`, the backtick (sh substitution, PowerShell escape), `$(`, `${`,
/// PowerShell's `@(` and `(`, which runs a command inside an argument, and
/// line breaks. Checked first so a refusal names the operator; the parser
/// then refuses every other metacharacter
/// ([`anvil_types::shell::METACHARACTERS`]).
pub const SHELL_OPERATORS: [&str; 14] = [
    "&&", "||", ";", "|", "&", ">", "<", "`", "$(", "${", "@(", "(", "\n", "\r",
];

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
    /// Whether an `always` answer may save a rule; `false` when a managed
    /// policy sets `[permissions] allow_always = false`.
    pub may_save: bool,
}

impl PermissionRequest {
    /// Whether the prompt should offer `always`, which saves a rule to disk.
    ///
    /// Only `shell` offers it: its saved rule is a command prefix, which is
    /// narrower than the session approval every prompt offers. A managed
    /// policy can turn it off.
    ///
    /// # Returns
    ///
    /// `true` for `shell` when saving is allowed.
    pub fn offers_always(&self) -> bool {
        self.tool == "shell" && self.may_save
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
    /// Where the decision came from.
    pub source: DecisionSource,
}

/// Hears every permission decision. The CLI records them in the usage records.
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

    /// Who answers, for the audit record of each answer. A wrapper
    /// forwards the inner prompt's source.
    ///
    /// # Returns
    ///
    /// [`DecisionSource::Prompt`] unless a prompt answers by itself.
    fn answer_source(&self) -> DecisionSource {
        DecisionSource::Prompt
    }
}

/// A prompt that always gives the same answer, as `--yes` does.
#[derive(Debug, Clone, Copy)]
pub struct StaticPrompt(pub PromptAnswer);

impl PermissionPrompt for StaticPrompt {
    fn ask(&self, _request: &PermissionRequest) -> PromptAnswer {
        self.0
    }

    fn answer_source(&self) -> DecisionSource {
        match self.0 {
            PromptAnswer::No => DecisionSource::Unattended,
            PromptAnswer::Yes | PromptAnswer::Session | PromptAnswer::Always => {
                DecisionSource::YesFlag
            }
        }
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
        let path = dir.path().join("permissions").join("repo.toml");
        let mut perms = Permissions::load_file(path.clone()).expect("loads");
        assert!(perms.rules().allow.is_empty());
        perms.add_allow("shell:cargo *").expect("writes");
        perms.add_allow("shell:cargo *").expect("idempotent");
        let text = std::fs::read_to_string(&path).expect("file exists");
        assert_eq!(text.matches("cargo").count(), 1, "{text}");
        let reloaded = Permissions::load_file(path).expect("reloads");
        assert_eq!(
            reloaded.decide("shell", Some("cargo build"), Decision::Ask),
            Decision::Allow
        );
    }

    #[test]
    fn load_reports_parse_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("repo.toml");
        std::fs::write(&path, "allow = [1]").expect("write");
        assert!(matches!(
            Permissions::load_file(path),
            Err(PermissionError::Parse { .. })
        ));
    }

    #[test]
    fn tightening_rules_beat_the_users_allow_and_never_loosen() {
        let mut perms = parse("allow = [\"shell\", \"write_file\"]\ndefault = \"allow\"\n");
        perms
            .tighten(
                &["shell:curl*".to_owned()],
                &["write_file:src/**".to_owned()],
                &[],
            )
            .expect("valid rules");
        assert_eq!(
            perms.decide("shell", Some("curl evil.example"), Decision::Ask),
            Decision::Deny
        );
        assert!(!perms.explicitly_allowed("shell", Some("curl evil.example")));
        assert_eq!(
            perms.decide("write_file", Some("src/main.rs"), Decision::Ask),
            Decision::Ask
        );
        assert_eq!(
            perms.decide("write_file", Some("README.md"), Decision::Ask),
            Decision::Allow
        );
        assert_eq!(
            perms.decide("shell", Some("cargo test"), Decision::Ask),
            Decision::Allow
        );
        assert!(
            perms.rules().deny.is_empty(),
            "tightening rules are not part of the user's file"
        );
        assert!(perms.tighten(&["shell:[".to_owned()], &[], &[]).is_err());
    }

    #[test]
    fn a_projects_allow_only_decides_what_no_other_rule_decides() {
        let mut perms = parse("ask = [\"edit_file:src/**\"]\ndeny = [\"shell:curl*\"]\n");
        perms
            .allow_from_project(&["edit_file".to_owned(), "shell".to_owned()])
            .expect("valid rules");
        assert_eq!(
            perms.decide("edit_file", Some("README.md"), Decision::Ask),
            Decision::Allow
        );
        assert_eq!(
            perms.decide("edit_file", Some("src/main.rs"), Decision::Ask),
            Decision::Ask,
            "the user's ask wins"
        );
        assert_eq!(
            perms.decide("shell", Some("curl x"), Decision::Ask),
            Decision::Deny
        );
        assert!(
            !perms.explicitly_allowed("shell", Some("rm -rf x")),
            "a repository's allow never lifts the blocked-command list"
        );
        assert!(perms.rules().allow.is_empty(), "never written to disk");

        let mut strict = parse("default = \"ask\"\n");
        strict
            .allow_from_project(&["edit_file".to_owned()])
            .expect("valid rules");
        assert_eq!(
            strict.decide("edit_file", Some("README.md"), Decision::Allow),
            Decision::Ask,
            "the user's own default wins"
        );
    }

    #[test]
    fn shell_programs_allow_one_listed_program_with_plain_arguments() {
        let perms = parse("shell_programs = [\"cargo\", \"git\"]\n");
        for ok in [
            "cargo test -p anvil-core",
            "git status",
            "CARGO.EXE build",
            "\"git\" log -1",
        ] {
            assert_eq!(perms.shell_refusal(ok), None, "{ok}");
        }
        for (bad, why) in [
            ("cargo test && curl x", "&&"),
            ("cargo test; rm -rf /", ";"),
            ("git log | sh", "|"),
            ("git log > out.txt", ">"),
            ("cargo run < input", "<"),
            ("cargo $(curl x)", "$("),
            ("cargo `curl x`", "`"),
            ("cargo (Remove-Item x)", "("),
            ("git log & calc", "&"),
            ("git log\ncurl x", "\\n"),
            ("./cargo build", "path"),
            ("C:\\tools\\git.exe status", "path"),
            ("npm test", "not one of"),
            ("powershell -c x", "not one of"),
        ] {
            let reason = perms
                .shell_refusal(bad)
                .unwrap_or_else(|| panic!("{bad} allowed"));
            assert!(reason.contains(why), "{bad}: {reason}");
        }
        assert_eq!(parse("").shell_refusal("a && b"), None, "no list, no limit");
    }

    #[test]
    fn tightening_shell_programs_only_narrows() {
        let mut perms = parse("shell_programs = [\"cargo\", \"git\"]\n");
        perms
            .tighten(&[], &[], &["git".to_owned(), "npm".to_owned()])
            .expect("tightens");
        assert_eq!(perms.shell_refusal("git status"), None);
        assert!(perms.shell_refusal("cargo test").is_some());
        assert!(
            perms.shell_refusal("npm test").is_some(),
            "npm was not the user's"
        );
        let mut open = parse("");
        open.tighten(&[], &[], &["cargo".to_owned()])
            .expect("tightens");
        assert!(open.shell_refusal("git status").is_some());
    }

    #[test]
    fn allowlist_layers_must_all_match_and_only_a_naming_pattern_lifts_a_block() {
        let mut perms = parse("shell_programs = [\"git\", \"cargo\"]\n");
        assert!(perms.has_shell_allowlist());
        perms
            .limit_shell(
                &["git status".to_owned(), "git push origin *".to_owned()],
                "the managed policy's [shell] allow",
            )
            .expect("patterns");
        assert_eq!(perms.shell_refusal("git status"), None);
        assert_eq!(perms.shell_refusal("git push origin main"), None);
        let reason = perms
            .shell_refusal("cargo test")
            .expect("the policy layer refuses it");
        assert!(reason.contains("[shell] allow"), "{reason}");
        assert!(
            perms.shell_refusal("git status -s").is_some(),
            "exact pattern"
        );
        assert!(
            !perms.allows_by_name("shell", Some("git push origin main"), "git push"),
            "every layer must name it, and `shell_programs = [\"git\"]` does not"
        );
        let mut named = parse("");
        named
            .limit_shell(&["git push origin *".to_owned()], "x")
            .expect("patterns");
        assert!(named.allows_by_name("shell", Some("git push origin main"), "git push"));
        assert!(!named.allows_by_name("shell", Some("git status"), "curl"));
        assert!(
            perms.limit_shell(&["git *;".to_owned()], "x").is_err(),
            "a pattern with a metacharacter is invalid"
        );
        let mut broad = parse("");
        broad
            .limit_shell(&["git *".to_owned()], "x")
            .expect("patterns");
        assert!(
            !broad.allows_by_name("shell", Some("git push origin main"), "git push"),
            "`git *` does not name `git push`"
        );
        let mut denied = parse("deny = [\"shell:git push*\"]\n");
        denied
            .limit_shell(&["git push *".to_owned()], "x")
            .expect("patterns");
        assert!(
            !denied.allows_by_name("shell", Some("git push x"), "git push"),
            "a deny rule still wins"
        );
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
            may_save: true,
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
            may_save: true,
        };
        assert!(request("shell").offers_always());
        assert!(!request("edit_file").offers_always());
    }
}
