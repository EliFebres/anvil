//! Tools the model can call, and the registry that offers, guards and runs them.
//!
//! [`ToolRegistry::execute`] is the single path from a model's tool call to a
//! result: it parses the arguments, applies the permission model, runs the
//! tool, truncates the output to the tool's byte cap and keeps the full
//! output on disk. Nothing here prints; prompting goes through
//! [`PermissionPrompt`], which the CLI implements.

/// The `edit_file` tool: exact-string replacement that refuses ambiguity.
pub mod edit_file;
/// The `glob` tool: finds files by pattern, honoring `.gitignore`.
pub mod glob;
/// The `grep` tool: regex search over files, honoring `.gitignore`.
pub mod grep;
/// The `list_dir` tool.
pub mod list_dir;
/// The `outline` tool: a file's or directory's structure without the bodies.
pub mod outline;
mod output;
/// The permission model: the user's rules, tightening rules, rule matching, and the
/// prompt hook the CLI implements.
pub mod permissions;
/// What a file-writing tool call will do, computed before it runs so the
/// change can be shown and judged.
pub mod preview;
/// The `read_file` tool.
pub mod read_file;
/// The `shell` tool and the shell program behind it.
pub mod shell;
/// The workspace jail: which paths the file tools may open.
pub mod workspace;
/// The `write_file` tool.
pub mod write_file;

#[cfg(test)]
mod allowlist_tests;
#[cfg(test)]
mod boundary_tests;

use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};

use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::Value;

use anvil_types::policy::TranscriptMode;

use crate::guard::{self, DataGuard};
use crate::provider::lock_or_recover;
use crate::provider::types::{StopReason, ToolCall, ToolResult, ToolSpec, Usage};

pub use output::{OutputStats, digest, sha256_hex};
pub use permissions::{
    Decision, DecisionScope, DecisionSource, PermissionDecision, PermissionError,
    PermissionObserver, PermissionPrompt, PermissionRequest, Permissions, PromptAnswer, RuleOrigin,
    ScriptedPrompt, SessionApprovals, StaticPrompt,
};
pub use shell::{Container, Runner, Shell};
pub use workspace::Workspace;

/// Byte cap applied to a tool's output unless the tool overrides it.
pub const DEFAULT_OUTPUT_CAP: usize = 30 * 1024;

/// The future a tool's `run` returns.
pub type ToolFuture<'a> = Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>>;

/// One model completion a tool made on its own, such as a delegated
/// subagent's turn, so the loop can count it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelTurn {
    /// Why the model stopped.
    pub stop_reason: StopReason,
    /// Tokens it used, when the provider reported them.
    pub usage: Option<Usage>,
    /// Wall-clock milliseconds the completion took.
    pub latency_ms: u64,
    /// Retries before the request got through.
    pub retries: u32,
}

/// What a tool produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    /// Text for the model.
    pub content: String,
    /// Whether the tool failed.
    pub is_error: bool,
    /// Completions the tool made itself; empty for every tool but `delegate`.
    pub model_turns: Vec<ModelTurn>,
    /// The exit code of the program the tool ran, for `shell`.
    pub exit_code: Option<i32>,
    /// Files the tool wrote, for `write_file` and `edit_file`.
    pub changes: Vec<FileChange>,
    /// Secrets the tool redacted itself, by kind, before it chose what to
    /// show; `read_file` and `grep` redact whole files this way.
    pub redactions: BTreeMap<String, usize>,
}

/// One file a tool wrote, by SHA-256 before and after. A command run by
/// `shell` can change files too; those changes are not observed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    /// The file, as the tool showed it to the model.
    pub path: String,
    /// SHA-256 (lowercase hex) of the file before the call, or `None` when
    /// it did not exist.
    pub before: Option<String>,
    /// SHA-256 (lowercase hex) of the file after the call.
    pub after: String,
}

impl ToolOutput {
    /// A successful output.
    ///
    /// # Arguments
    ///
    /// * `content` - Text for the model.
    ///
    /// # Returns
    ///
    /// The output.
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            model_turns: Vec::new(),
            exit_code: None,
            changes: Vec::new(),
            redactions: BTreeMap::new(),
        }
    }

    /// The output with the counts of what the tool redacted itself.
    ///
    /// # Arguments
    ///
    /// * `redactions` - Kinds and counts.
    ///
    /// # Returns
    ///
    /// The output.
    pub fn with_redactions(mut self, redactions: BTreeMap<String, usize>) -> Self {
        self.redactions = redactions;
        self
    }

    /// The output with one written file added.
    ///
    /// # Arguments
    ///
    /// * `change` - The file and its digests.
    ///
    /// # Returns
    ///
    /// The output.
    pub fn with_change(mut self, change: FileChange) -> Self {
        self.changes.push(change);
        self
    }

    /// A failed output.
    ///
    /// # Arguments
    ///
    /// * `message` - What went wrong, for the model.
    ///
    /// # Returns
    ///
    /// The output.
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            content: message.into(),
            is_error: true,
            model_turns: Vec::new(),
            exit_code: None,
            changes: Vec::new(),
            redactions: BTreeMap::new(),
        }
    }
}

/// Whether a tool only reads or may change the project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Reads files or metadata only.
    Read,
    /// Writes files or runs commands.
    Write,
}

impl Access {
    /// The permission decision used when no rule matches.
    ///
    /// # Returns
    ///
    /// `Allow` for reads, `Ask` for writes.
    pub fn default_decision(self) -> Decision {
        match self {
            Access::Read => Decision::Allow,
            Access::Write => Decision::Ask,
        }
    }
}

/// A tool the model can call.
pub trait Tool: Send + Sync {
    /// The name the model uses to call it.
    fn name(&self) -> &str;

    /// What it does, for the model.
    fn description(&self) -> &str;

    /// JSON Schema of its arguments object.
    fn schema(&self) -> Value;

    /// Whether it reads or writes.
    fn access(&self) -> Access;

    /// Byte cap for its output before truncation.
    fn output_cap(&self) -> usize {
        DEFAULT_OUTPUT_CAP
    }

    /// The path or command a permission rule's glob is matched against.
    ///
    /// The default reads `path`, then `command`, from the arguments.
    fn target(&self, args: &Value) -> Option<String> {
        args.get("path")
            .or_else(|| args.get("command"))
            .and_then(Value::as_str)
            .map(str::to_owned)
    }

    /// Every target a permission rule must allow before the call runs.
    ///
    /// The default is the single [`Tool::target`]; a tool that acts on
    /// several paths in one call lists each, so a rule cannot be sidestepped
    /// by batching.
    fn targets(&self, args: &Value) -> Vec<Option<String>> {
        vec![self.target(args)]
    }

    /// Whether [`Tool::target`] is a file path, so a session approval for
    /// this tool only covers targets that resolve inside the workspace root.
    ///
    /// # Returns
    ///
    /// `false` by default; `true` for tools that write files.
    fn target_is_path(&self) -> bool {
        false
    }

    /// Why this call must not run even if a prompt would allow it.
    ///
    /// Only a rule that names the blocked phrase can override a blocked
    /// call (see [`Permissions::allows_by_name`]); a prompt, `--yes` or a
    /// broad `allow` rule cannot.
    fn blocked_reason(&self, _args: &Value) -> Option<Blocked> {
        None
    }

    /// Whether this call may run at the same time as other calls that
    /// answer `true` in the same model turn.
    ///
    /// A call that may write must answer `false`: those run one after
    /// another, in the order the model gave them.
    ///
    /// # Arguments
    ///
    /// * `args` - The call's arguments.
    ///
    /// # Returns
    ///
    /// `false` by default.
    fn parallel_safe(&self, _args: &Value) -> bool {
        false
    }

    /// Whether this particular call may change anything.
    ///
    /// The loop refuses such calls while it is planning. The default follows
    /// [`Tool::access`]; a tool that only sometimes writes, like a
    /// delegation, answers per call.
    ///
    /// # Arguments
    ///
    /// * `args` - The call's arguments.
    ///
    /// # Returns
    ///
    /// `true` unless the tool only reads.
    fn may_write(&self, _args: &Value) -> bool {
        self.access() != Access::Read
    }

    /// Runs the tool.
    fn run<'a>(&'a self, args: Value, ctx: &'a ToolContext) -> ToolFuture<'a>;
}

/// A call a tool refuses to run unless a rule names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blocked {
    /// What matched, such as `git push` or `curl`. A rule allows the call
    /// only when its pattern contains this text.
    pub phrase: String,
    /// The explanation for the model.
    pub reason: String,
}

/// Where and how tools run.
#[derive(Debug, Clone)]
pub struct ToolContext {
    /// Directory relative paths resolve against.
    pub cwd: PathBuf,
    /// Session folder; full tool outputs are kept under it when set.
    pub session_dir: Option<PathBuf>,
    /// The shell behind the `shell` tool.
    pub shell: Shell,
    /// How commands are started: through the shell or directly, on the
    /// host or in a container.
    pub runner: Runner,
    /// Folders outside `cwd` the file tools may also open.
    pub workspace: Workspace,
    /// Redaction and the untrusted-data delimiters every tool result and
    /// hook output gets before it goes back to the model.
    pub guard: DataGuard,
    /// What the session folder keeps. Below `full`, no untruncated tool
    /// output and no compaction summary is written there.
    pub transcript: TranscriptMode,
}

impl ToolContext {
    /// A context rooted at `cwd` with the platform shell and no session folder.
    ///
    /// # Arguments
    ///
    /// * `cwd` - Directory relative paths resolve against.
    ///
    /// # Returns
    ///
    /// The context.
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            session_dir: None,
            shell: Shell::for_platform(),
            runner: Runner::default(),
            workspace: Workspace::default(),
            guard: DataGuard::default(),
            transcript: TranscriptMode::Full,
        }
    }

    /// Resolves a path argument and checks that a file tool may open it.
    ///
    /// The path is resolved the way the file system will see it, symlinks
    /// and `..` included, so neither can lead out of the workspace. It must
    /// then lie inside `cwd`, the session folder or an extra folder, or,
    /// for a read, a read-only folder; it must not hold secrets (see
    /// [`workspace::holds_secrets`]); and a write must not land in a git
    /// directory (see [`workspace::inside_git_dir`]), in a file that grants
    /// power (see [`workspace::grants_power`]), in a protected folder or in
    /// a file a hook or MCP server runs. For reads and writes alike, on
    /// every system, its spelling must be one Windows does not fold (see
    /// [`workspace::win32_fold`]), so the path checked is the path opened.
    ///
    /// # Arguments
    ///
    /// * `path` - Relative or absolute path as the model wrote it.
    /// * `write` - Whether the tool will write to it.
    ///
    /// # Returns
    ///
    /// The path joined to `cwd`, for opening and display.
    ///
    /// # Errors
    ///
    /// A message for the model saying why the path is refused.
    pub fn checked_path(&self, path: &str, write: bool) -> Result<PathBuf, String> {
        // Windows strips trailing dots and spaces and reads `:` as a data
        // stream, so a name that folds differently could reach a file the
        // checks below never saw. Reads count too: `server.pem.` would
        // pass the `*.pem` check and open the key.
        if workspace::win32_fold(Path::new(path)).as_deref() != Some(Path::new(path)) {
            return Err(format!(
                "`{path}` has a name that ends in a dot or a space or names a data stream (`:`); Windows would open a different file, so anvil's file tools refuse it"
            ));
        }
        let root = workspace::canonical(&self.cwd);
        let resolved = permissions::resolve_path(&root, path).ok_or_else(|| {
            format!("`{path}` cannot be resolved safely (a dangling or looping link)")
        })?;
        if write && workspace::inside_git_dir(&resolved) {
            return Err(format!(
                "`{path}` is inside a git directory (hooks, config and the rest of .git); anvil's file tools never write there"
            ));
        }
        if workspace::holds_secrets(&resolved) {
            return Err(format!(
                "`{path}` holds secrets or credentials; anvil's file tools never open it"
            ));
        }
        if self.workspace.refuses(&resolved) {
            return Err(format!(
                "`{path}` matches the managed policy's [files] deny; anvil's file tools never open it"
            ));
        }
        if write && workspace::grants_power(&resolved) {
            return Err(format!(
                "`{path}` sets anvil's permissions, hooks or MCP servers; anvil's file tools never write it, so a model cannot raise its own permissions"
            ));
        }
        if write
            && self
                .workspace
                .protected_dirs
                .iter()
                .any(|dir| workspace::is_within(dir, &resolved))
        {
            return Err(format!(
                "`{path}` is in anvil's own settings folder; anvil's file tools never write there"
            ));
        }
        if write && self.workspace.protected_files.contains(&resolved) {
            return Err(format!(
                "`{path}` is a script or program a hook or MCP server runs; anvil's file tools never write it, so a model cannot plant code that runs without a prompt"
            ));
        }
        let mut allowed: Vec<&Path> = vec![&self.cwd];
        allowed.extend(self.session_dir.as_deref());
        allowed.extend(self.workspace.extra_dirs.iter().map(PathBuf::as_path));
        if !write {
            allowed.extend(self.workspace.read_only_dirs.iter().map(PathBuf::as_path));
        }
        if allowed
            .iter()
            .any(|dir| workspace::is_within(dir, &resolved))
        {
            return Ok(self.resolve(path));
        }
        Err(format!(
            "`{path}` is outside the workspace {}; anvil's file tools stay inside it unless `[tools] extra_dirs` in ~/.anvil/config.toml lists the folder",
            self.cwd.display()
        ))
    }

    /// Resolves a path argument against `cwd`.
    ///
    /// # Arguments
    ///
    /// * `path` - Relative or absolute path as the model wrote it.
    ///
    /// # Returns
    ///
    /// An absolute path when `cwd` is absolute.
    pub fn resolve(&self, path: &str) -> PathBuf {
        let path = Path::new(path);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.cwd.join(path)
        }
    }

    /// Renders a path for the model: relative to `cwd` with forward slashes
    /// when inside it, absolute otherwise.
    ///
    /// # Arguments
    ///
    /// * `path` - The path to render.
    ///
    /// # Returns
    ///
    /// The rendered path.
    pub fn display_path(&self, path: &Path) -> String {
        match path.strip_prefix(&self.cwd) {
            Ok(relative) if !relative.as_os_str().is_empty() => {
                relative.to_string_lossy().replace('\\', "/")
            }
            Ok(_) => ".".to_owned(),
            Err(_) => path.to_string_lossy().into_owned(),
        }
    }
}

/// Builds the argument schema for a tool from its arguments type.
///
/// The `$schema` and `title` keys are dropped: providers want a bare object
/// schema.
///
/// # Returns
///
/// The schema as JSON.
pub fn schema_for<T: JsonSchema>() -> Value {
    let mut schema = serde_json::to_value(schemars::schema_for!(T)).unwrap_or(Value::Null);
    if let Some(object) = schema.as_object_mut() {
        object.remove("$schema");
        object.remove("title");
    }
    schema
}

/// Parses tool arguments, turning a mismatch into an error output.
///
/// # Arguments
///
/// * `args` - The arguments object from the model.
///
/// # Returns
///
/// The typed arguments.
///
/// # Errors
///
/// A [`ToolOutput::error`] naming the problem, to be returned to the model.
pub fn parse_args<T: DeserializeOwned>(args: Value) -> Result<T, ToolOutput> {
    serde_json::from_value(args)
        .map_err(|error| ToolOutput::error(format!("invalid arguments: {error}")))
}

/// How a tool call ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolOutcome {
    /// The tool ran and succeeded.
    Completed,
    /// The tool ran and reported an error.
    Failed,
    /// A `deny` rule matched.
    DeniedByRule,
    /// The user answered `n` at the prompt.
    DeclinedByUser,
    /// The call is on the blocked list and no `allow` rule covers it.
    Blocked,
    /// No tool has that name.
    UnknownTool,
    /// The arguments were not valid JSON or did not match the schema.
    InvalidArguments,
}

impl ToolOutcome {
    /// Whether the call was stopped by the permission model.
    ///
    /// # Returns
    ///
    /// `true` for rule denials, user declines and blocked commands.
    pub fn is_denial(self) -> bool {
        matches!(
            self,
            ToolOutcome::DeniedByRule | ToolOutcome::DeclinedByUser | ToolOutcome::Blocked
        )
    }
}

/// The result of [`ToolRegistry::execute`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Execution {
    /// What goes back to the model.
    pub result: ToolResult,
    /// How the call ended.
    pub outcome: ToolOutcome,
    /// Where the untruncated output was written, if it was truncated.
    pub full_output: Option<PathBuf>,
    /// Size and digest of the full output, for the usage records.
    pub stats: OutputStats,
    /// Completions the tool made itself, to count against the session.
    pub model_turns: Vec<ModelTurn>,
    /// How many secrets of each kind were redacted from the output and
    /// hook text. Never holds a value.
    pub redactions: BTreeMap<String, usize>,
    /// Files the call wrote.
    pub changes: Vec<FileChange>,
}

/// A boxed future returned by a [`ToolHook`].
pub type HookFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What a [`ToolHook`] decided before a tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookDecision {
    /// Run the tool.
    Allow,
    /// Do not run the tool; the reason goes back to the model.
    Block {
        /// Why the call was refused.
        reason: String,
    },
}

/// Code that runs around every tool call, after the permission check.
///
/// The registry owns at most one hook. The CLI plugs in Claude Code-style
/// `PreToolUse` and `PostToolUse` hooks through it; anvil-core itself only
/// calls the two methods.
pub trait ToolHook: Send + Sync {
    /// Runs before a tool that the permission model allowed.
    ///
    /// # Arguments
    ///
    /// * `call` - The model's call.
    /// * `args` - Its parsed arguments.
    /// * `ctx` - Where the tool would run.
    ///
    /// # Returns
    ///
    /// Whether to run the tool.
    fn before<'a>(
        &'a self,
        call: &'a ToolCall,
        args: &'a Value,
        ctx: &'a ToolContext,
    ) -> HookFuture<'a, HookDecision>;

    /// Runs after a tool finished, with its result.
    ///
    /// # Arguments
    ///
    /// * `call` - The model's call.
    /// * `args` - Its parsed arguments.
    /// * `result` - What the tool returned, already truncated.
    /// * `ctx` - Where the tool ran.
    ///
    /// # Returns
    ///
    /// Text to append to the result for the model, if any.
    fn after<'a>(
        &'a self,
        call: &'a ToolCall,
        args: &'a Value,
        result: &'a ToolResult,
        ctx: &'a ToolContext,
    ) -> HookFuture<'a, Option<String>>;
}

/// The tools on offer, with the permission model that guards them.
pub struct ToolRegistry {
    /// Tools by name.
    tools: BTreeMap<String, Box<dyn Tool>>,
    /// Rules, mutated when the user answers `always`.
    permissions: Mutex<Permissions>,
    /// Who to ask when a rule says `ask`.
    prompt: Box<dyn PermissionPrompt>,
    /// Session-scoped approvals, shared with sub-agent registries; its lock
    /// is also held across a prompt, so sharing registries never prompt at once.
    approvals: Arc<Mutex<SessionApprovals>>,
    /// Hears every permission decision.
    observer: Option<Arc<dyn PermissionObserver>>,
    /// Runs around every allowed call.
    hook: Option<Arc<dyn ToolHook>>,
    /// Refuses every call once it says so.
    gate: Option<Arc<dyn CallGate>>,
    /// Whether an `always` answer may save a rule; a managed policy can
    /// turn it off, and the answer then counts for the session only.
    always_allowed: bool,
}

/// Refuses every tool call and command once it gives a reason, such as
/// when a session's required audit log can no longer be written.
pub trait CallGate: Send + Sync {
    /// Why nothing may run any more.
    ///
    /// # Returns
    ///
    /// The reason, or `None` while calls may run.
    fn refusal(&self) -> Option<String>;
}

impl ToolRegistry {
    /// An empty registry.
    ///
    /// # Arguments
    ///
    /// * `permissions` - The loaded rules.
    /// * `prompt` - Answers `ask` decisions.
    ///
    /// # Returns
    ///
    /// The registry.
    pub fn new(permissions: Permissions, prompt: Box<dyn PermissionPrompt>) -> Self {
        Self {
            tools: BTreeMap::new(),
            permissions: Mutex::new(permissions),
            prompt,
            approvals: Arc::new(Mutex::new(SessionApprovals::default())),
            always_allowed: true,
            observer: None,
            gate: None,
            hook: None,
        }
    }

    /// Replaces the session approvals with a shared set, so every registry
    /// of one session (the main one and each sub-agent's) remembers the
    /// same approvals. Its lock is held while a prompt is open, so registries
    /// sharing it ask one at a time and each rechecks after the previous answer.
    ///
    /// # Arguments
    ///
    /// * `approvals` - The session's approvals.
    ///
    /// # Returns
    ///
    /// The registry.
    pub fn with_session_approvals(mut self, approvals: Arc<Mutex<SessionApprovals>>) -> Self {
        self.approvals = approvals;
        self
    }

    /// Turns the `always` answer, which saves a rule to disk, on or off.
    /// When off, prompts do not offer it and an `always` answer counts as
    /// `session`.
    ///
    /// # Arguments
    ///
    /// * `allowed` - Whether `always` may save a rule.
    ///
    /// # Returns
    ///
    /// The registry.
    pub fn with_always(mut self, allowed: bool) -> Self {
        self.always_allowed = allowed;
        self
    }

    /// The session approvals this registry consults.
    ///
    /// # Returns
    ///
    /// The shared handle.
    pub fn session_approvals(&self) -> Arc<Mutex<SessionApprovals>> {
        Arc::clone(&self.approvals)
    }

    /// Installs the observer that hears every permission decision,
    /// replacing any previous one.
    ///
    /// # Arguments
    ///
    /// * `observer` - The observer; it receives only the tool name, the
    ///   scope and whether the call was decided without a prompt.
    pub fn set_observer(&mut self, observer: Arc<dyn PermissionObserver>) {
        self.observer = Some(observer);
    }

    /// Installs the hook that runs around every allowed call, replacing any
    /// previous one.
    ///
    /// # Arguments
    ///
    /// * `hook` - The hook.
    pub fn set_hook(&mut self, hook: Arc<dyn ToolHook>) {
        self.hook = Some(hook);
    }

    /// Installs a gate that refuses every call once it gives a reason.
    ///
    /// # Arguments
    ///
    /// * `gate` - The gate.
    pub fn set_gate(&mut self, gate: Arc<dyn CallGate>) {
        self.gate = Some(gate);
    }

    /// The gate's reason to refuse everything, if it has one; the loop
    /// asks before it runs a check command itself.
    ///
    /// # Returns
    ///
    /// The reason, or `None` while calls may run.
    pub fn gate_refusal(&self) -> Option<String> {
        self.gate.as_ref().and_then(|gate| gate.refusal())
    }

    /// A registry with every built-in tool.
    ///
    /// # Arguments
    ///
    /// * `permissions` - The loaded rules.
    /// * `prompt` - Answers `ask` decisions.
    ///
    /// # Returns
    ///
    /// The registry.
    pub fn with_builtins(permissions: Permissions, prompt: Box<dyn PermissionPrompt>) -> Self {
        let mut registry = Self::new(permissions, prompt);
        registry.register(Box::new(read_file::ReadFile));
        registry.register(Box::new(write_file::WriteFile));
        registry.register(Box::new(edit_file::EditFile));
        registry.register(Box::new(glob::Glob));
        registry.register(Box::new(grep::Grep));
        registry.register(Box::new(shell::ShellTool));
        registry.register(Box::new(list_dir::ListDir));
        registry.register(Box::new(outline::Outline));
        registry
    }

    /// Adds or replaces a tool.
    ///
    /// # Arguments
    ///
    /// * `tool` - The tool.
    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.insert(tool.name().to_owned(), tool);
    }

    /// Removes a tool, so the model is not offered it and a call to it is
    /// an unknown tool. `[shell] mode = "off"` removes `shell` this way.
    ///
    /// # Arguments
    ///
    /// * `name` - The tool name.
    pub fn unregister(&mut self, name: &str) {
        self.tools.remove(name);
    }

    /// Looks a tool up by name.
    ///
    /// # Arguments
    ///
    /// * `name` - The tool name.
    ///
    /// # Returns
    ///
    /// The tool, if registered.
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.get(name).map(Box::as_ref)
    }

    /// Registered tool names, sorted.
    ///
    /// # Returns
    ///
    /// The names.
    pub fn names(&self) -> Vec<&str> {
        self.tools.keys().map(String::as_str).collect()
    }

    /// The tool list to send to a provider.
    ///
    /// # Returns
    ///
    /// One spec per tool, sorted by name.
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools
            .values()
            .map(|tool| ToolSpec {
                name: tool.name().to_owned(),
                description: tool.description().to_owned(),
                parameters: tool.schema(),
            })
            .collect()
    }

    /// The current rules.
    ///
    /// # Returns
    ///
    /// A guard over the permissions; drop it before calling `execute`.
    pub fn permissions(&self) -> MutexGuard<'_, Permissions> {
        lock_or_recover(&self.permissions)
    }

    /// Runs one tool call under the permission model.
    ///
    /// Never fails: every problem becomes an error result for the model and
    /// an [`Execution::outcome`] for the loop.
    ///
    /// # Arguments
    ///
    /// * `call` - The model's call.
    /// * `ctx` - Where to run it.
    ///
    /// # Returns
    ///
    /// The execution record.
    pub async fn execute(&self, call: &ToolCall, ctx: &ToolContext) -> Execution {
        if let Some(reason) = self.gate_refusal() {
            return Execution::finished(call, ToolOutput::error(reason), ToolOutcome::Blocked);
        }
        let Some(tool) = self.get(&call.name) else {
            tracing::warn!(tool = %call.name, "model called an unknown tool");
            return Execution::finished(
                call,
                ToolOutput::error(format!(
                    "unknown tool `{}`; available tools: {}",
                    call.name,
                    self.names().join(", ")
                )),
                ToolOutcome::UnknownTool,
            );
        };
        let args: Value = match serde_json::from_str(&call.arguments_json) {
            Ok(args) => args,
            Err(error) => {
                return Execution::finished(
                    call,
                    ToolOutput::error(format!("arguments are not valid JSON: {error}")),
                    ToolOutcome::InvalidArguments,
                );
            }
        };
        for target in tool.targets(&args) {
            if let Some(outcome) = self.check_permission(tool, &args, target.as_deref(), &ctx.cwd) {
                return Execution::finished(call, outcome.0, outcome.1);
            }
        }
        // Recording the decision may itself have closed the gate.
        if let Some(reason) = self.gate_refusal() {
            return Execution::finished(call, ToolOutput::error(reason), ToolOutcome::Blocked);
        }
        if let Some(hook) = &self.hook
            && let HookDecision::Block { reason } = hook.before(call, &args, ctx).await
        {
            tracing::info!(tool = %call.name, "blocked by a hook");
            let (reason, counts) = ctx.guard.guard("hook", &reason);
            let mut execution = Execution::finished(
                call,
                ToolOutput::error(format!("blocked by a hook: {reason}")),
                ToolOutcome::Blocked,
            );
            execution.redactions = counts;
            return execution;
        }

        // Under an allowlist a command runs without a shell interpreter,
        // whatever the context says, so what was matched is what runs.
        let direct;
        let ctx = if tool.name() == shell::SHELL_TOOL_NAME
            && !ctx.runner.direct
            && self.permissions().has_shell_allowlist()
        {
            direct = ToolContext {
                runner: Runner {
                    direct: true,
                    ..ctx.runner.clone()
                },
                ..ctx.clone()
            };
            &direct
        } else {
            ctx
        };
        if let Some(reason) = self.gate_refusal() {
            return Execution::finished(call, ToolOutput::error(reason), ToolOutcome::Blocked);
        }
        let mut output = tool.run(args.clone(), ctx).await;
        let redacted_by_tool = std::mem::take(&mut output.redactions);
        let redacted = ctx.guard.redact(&call.name, &output.content);
        output.content = redacted.text;
        let outcome = if output.is_error {
            ToolOutcome::Failed
        } else {
            ToolOutcome::Completed
        };
        let mut execution = match output::truncate(&output.content, tool.output_cap()) {
            None => Execution::finished(call, output, outcome),
            Some(truncated) => {
                let full_output = ctx
                    .session_dir
                    .as_deref()
                    .filter(|_| ctx.transcript == TranscriptMode::Full)
                    .and_then(
                        |dir| match output::write_full(dir, &call.id, &output.content) {
                            Ok(path) => Some(path),
                            Err(error) => {
                                tracing::warn!(%error, "could not keep full tool output");
                                None
                            }
                        },
                    );
                let content =
                    output::with_note(truncated, output.content.len(), full_output.as_deref());
                Execution {
                    result: ToolResult {
                        call_id: call.id.clone(),
                        content,
                        is_error: output.is_error,
                    },
                    outcome,
                    full_output,
                    stats: OutputStats::of(&output.content, true).exited(output.exit_code),
                    model_turns: output.model_turns,
                    redactions: BTreeMap::new(),
                    changes: output.changes,
                }
            }
        };
        execution.redactions = redacted.counts;
        guard::add_counts(&mut execution.redactions, redacted_by_tool);
        // A PostToolUse hook sees the redacted output as the tool produced
        // it, without the delimiters, so existing hook scripts keep working.
        let note = match &self.hook {
            Some(hook) if self.gate_refusal().is_none() => {
                hook.after(call, &args, &execution.result, ctx).await
            }
            _ => None,
        };
        execution.result.content = ctx.guard.wrap(&call.name, &execution.result.content);
        if let Some(note) = note {
            let (note, counts) = ctx.guard.guard("hook", note.trim_end());
            guard::add_counts(&mut execution.redactions, counts);
            execution.result.content.push_str("\n\n[hook] ");
            execution.result.content.push_str(&note);
        }
        execution
    }

    /// Whether a call may run at the same time as its neighbours.
    ///
    /// # Arguments
    ///
    /// * `call` - The model's call.
    ///
    /// # Returns
    ///
    /// `true` when the tool exists, the arguments parse and
    /// [`Tool::parallel_safe`] says so.
    pub fn runs_in_parallel(&self, call: &ToolCall) -> bool {
        let Some(tool) = self.get(&call.name) else {
            return false;
        };
        serde_json::from_str::<Value>(&call.arguments_json)
            .is_ok_and(|args| tool.parallel_safe(&args))
    }

    /// Whether a call may change anything.
    ///
    /// # Arguments
    ///
    /// * `call` - The model's call.
    ///
    /// # Returns
    ///
    /// [`Tool::may_write`] for a known tool with parsable arguments; `false`
    /// otherwise, since such a call fails without running anything.
    pub fn may_write(&self, call: &ToolCall) -> bool {
        let Some(tool) = self.get(&call.name) else {
            return false;
        };
        serde_json::from_str::<Value>(&call.arguments_json).is_ok_and(|args| tool.may_write(&args))
    }

    /// Decides whether a call may run, without running it.
    ///
    /// The loop uses it for a command it runs itself but did not choose,
    /// such as the check command a plan declares: the blocked list, the
    /// rules and the session approvals apply as they would to the model's
    /// own call, and an `ask` rule prompts.
    ///
    /// # Arguments
    ///
    /// * `name` - The tool the call would go to.
    /// * `args` - The call's arguments.
    /// * `root` - The project root.
    ///
    /// # Errors
    ///
    /// The reason, when the tool is unknown or the call may not run.
    pub fn permit(&self, name: &str, args: &Value, root: &Path) -> Result<(), String> {
        if let Some(reason) = self.gate_refusal() {
            return Err(reason);
        }
        let tool = self
            .get(name)
            .ok_or_else(|| format!("unknown tool `{name}`"))?;
        for target in tool.targets(args) {
            if let Some((output, _)) = self.check_permission(tool, args, target.as_deref(), root) {
                return Err(output.content);
            }
        }
        // Recording the decision may itself have closed the gate.
        match self.gate_refusal() {
            Some(reason) => Err(reason),
            None => Ok(()),
        }
    }

    /// Applies the blocked list, the rules and the session approvals; `None`
    /// means the call may run.
    fn check_permission(
        &self,
        tool: &dyn Tool,
        args: &Value,
        target: Option<&str>,
        root: &Path,
    ) -> Option<(ToolOutput, ToolOutcome)> {
        let name = tool.name();
        let mut permissions = lock_or_recover(&self.permissions);
        if name == shell::SHELL_TOOL_NAME
            && let Some((reason, origin)) =
                target.and_then(|command| permissions.shell_refusal_by(command))
        {
            tracing::info!(tool = name, ?target, reason, "refused by shell_programs");
            self.observe(
                name,
                DecisionScope::Blocked,
                false,
                DecisionSource::ShellAllowlist(origin),
            );
            return Some((ToolOutput::error(reason), ToolOutcome::Blocked));
        }
        if name == shell::SHELL_TOOL_NAME
            && permissions.has_shell_allowlist()
            && let Some(reason) = target
                .and_then(|command| anvil_types::shell::split_command(command).ok())
                .and_then(|argv| shell::argument_refusal(&workspace::canonical(root), &argv))
        {
            tracing::info!(
                tool = name,
                ?target,
                reason,
                "refused by the allowlist's argument checks"
            );
            self.observe(
                name,
                DecisionScope::Blocked,
                false,
                DecisionSource::BlockedList,
            );
            return Some((ToolOutput::error(reason), ToolOutcome::Blocked));
        }
        if let Some(blocked) = tool.blocked_reason(args)
            && !permissions.allows_by_name(name, target, &blocked.phrase)
        {
            tracing::info!(
                tool = name,
                ?target,
                reason = blocked.reason,
                "blocked tool call"
            );
            self.observe(
                name,
                DecisionScope::Blocked,
                false,
                DecisionSource::BlockedList,
            );
            return Some((
                ToolOutput::error(format!(
                    "{}; blocked unless an `allow` rule in your own permission rules names `{}`",
                    blocked.reason, blocked.phrase
                )),
                ToolOutcome::Blocked,
            ));
        }
        let by_rule = permissions.ruling(name, target);
        let rule_source = by_rule.map(|(_, origin)| DecisionSource::Rule(origin));
        match by_rule.map_or(tool.access().default_decision(), |(decision, _)| decision) {
            Decision::Allow => {
                if let Some(source) = rule_source {
                    self.observe(name, DecisionScope::Rule, true, source);
                }
                None
            }
            Decision::Deny => {
                tracing::info!(tool = name, ?target, "denied by permission rule");
                self.observe(
                    name,
                    DecisionScope::Deny,
                    false,
                    rule_source.unwrap_or(DecisionSource::Rule(RuleOrigin::Builtin)),
                );
                Some((
                    ToolOutput::error("denied by a permission rule".to_owned()),
                    ToolOutcome::DeniedByRule,
                ))
            }
            Decision::Ask => {
                // Held across the prompt so a registry sharing these approvals
                // waits for this answer and then rechecks, instead of prompting too.
                let mut approvals = lock_or_recover(&self.approvals);
                if approvals.covers(name, target, root, tool.target_is_path()) {
                    self.observe(
                        name,
                        DecisionScope::Session,
                        true,
                        DecisionSource::SessionApproval,
                    );
                    return None;
                }
                let answered_by = self.prompt.answer_source();
                let request = PermissionRequest {
                    tool: name.to_owned(),
                    target: target.map(str::to_owned),
                    arguments: args.clone(),
                    may_save: self.always_allowed,
                };
                match self.prompt.ask(&request) {
                    PromptAnswer::Yes => {
                        self.observe(name, DecisionScope::Once, false, answered_by);
                        None
                    }
                    PromptAnswer::No => {
                        self.observe(name, DecisionScope::Deny, false, answered_by);
                        tracing::info!(tool = name, ?target, "declined by user");
                        Some((
                            ToolOutput::error("the user declined this call".to_owned()),
                            ToolOutcome::DeclinedByUser,
                        ))
                    }
                    PromptAnswer::Session | PromptAnswer::Always if !self.always_allowed => {
                        if let Err(error) = approvals.approve(name, root) {
                            tracing::warn!(%error, tool = name, "could not keep the session approval");
                        }
                        self.observe(name, DecisionScope::Session, false, answered_by);
                        None
                    }
                    PromptAnswer::Session => {
                        if let Err(error) = approvals.approve(name, root) {
                            tracing::warn!(%error, tool = name, "could not keep the session approval");
                        }
                        self.observe(name, DecisionScope::Session, false, answered_by);
                        None
                    }
                    PromptAnswer::Always => {
                        let rule = permissions::always_rule(name, target);
                        let scope = match permissions.add_allow(&rule) {
                            Ok(()) => DecisionScope::Saved,
                            Err(error) => {
                                tracing::warn!(%error, rule, "could not save the permission rule");
                                DecisionScope::Session
                            }
                        };
                        self.observe(name, scope, false, answered_by);
                        None
                    }
                }
            }
        }
    }

    /// Tells the observer, if any, about one decision.
    fn observe(&self, tool: &str, scope: DecisionScope, covered: bool, source: DecisionSource) {
        if let Some(observer) = &self.observer {
            observer.decided(&PermissionDecision {
                tool: tool.to_owned(),
                scope,
                covered,
                source,
            });
        }
    }
}

impl Execution {
    /// A call the loop did not run, answered with `message`. Its outcome is
    /// [`ToolOutcome::Failed`], which is not a denial: nothing was refused by
    /// the permission model.
    ///
    /// # Arguments
    ///
    /// * `call` - The call.
    /// * `message` - What the model is told.
    ///
    /// # Returns
    ///
    /// The execution.
    pub fn not_run(call: &ToolCall, message: impl Into<String>) -> Self {
        Self::finished(call, ToolOutput::error(message), ToolOutcome::Failed)
    }

    /// Wraps an untruncated output.
    fn finished(call: &ToolCall, output: ToolOutput, outcome: ToolOutcome) -> Self {
        Self {
            stats: OutputStats::of(&output.content, false).exited(output.exit_code),
            changes: output.changes,
            model_turns: output.model_turns,
            result: ToolResult {
                call_id: call.id.clone(),
                content: output.content,
                is_error: output.is_error,
            },
            outcome,
            full_output: None,
            redactions: BTreeMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_call_may_write_when_its_tool_does_by_default() {
        let registry = ToolRegistry::with_builtins(
            Permissions::empty(),
            Box::new(StaticPrompt(PromptAnswer::No)),
        );
        let call = |name: &str| ToolCall {
            id: "c".to_owned(),
            name: name.to_owned(),
            arguments_json: "{}".to_owned(),
        };
        assert!(registry.may_write(&call("write_file")));
        assert!(registry.may_write(&call("shell")));
        assert!(!registry.may_write(&call("read_file")));
        assert!(!registry.may_write(&call("no_such_tool")));
    }

    /// Returns its `text` argument repeated `times` times.
    struct Echo;

    #[derive(serde::Deserialize, JsonSchema)]
    struct EchoArgs {
        text: String,
        #[serde(default)]
        times: usize,
        #[serde(default)]
        fail: bool,
    }

    impl Tool for Echo {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "Echoes."
        }
        fn schema(&self) -> Value {
            schema_for::<EchoArgs>()
        }
        fn access(&self) -> Access {
            Access::Write
        }
        fn output_cap(&self) -> usize {
            100
        }
        fn target(&self, args: &Value) -> Option<String> {
            args.get("text").and_then(Value::as_str).map(str::to_owned)
        }
        fn blocked_reason(&self, args: &Value) -> Option<Blocked> {
            args.get("text")
                .and_then(Value::as_str)
                .filter(|text| text.contains("nuke"))
                .map(|_| Blocked {
                    phrase: "nuke".to_owned(),
                    reason: "text contains `nuke`".to_owned(),
                })
        }
        fn run<'a>(&'a self, args: Value, _ctx: &'a ToolContext) -> ToolFuture<'a> {
            Box::pin(async move {
                let args: EchoArgs = match parse_args(args) {
                    Ok(args) => args,
                    Err(output) => return output,
                };
                if args.fail {
                    return ToolOutput::error("asked to fail");
                }
                ToolOutput::ok(args.text.repeat(args.times.max(1)))
            })
        }
    }

    fn make_registry(rules: &str, answers: Vec<PromptAnswer>) -> ToolRegistry {
        let permissions = Permissions::from_toml(rules, None).expect("rules parse");
        let mut registry = ToolRegistry::new(permissions, Box::new(ScriptedPrompt::new(answers)));
        registry.register(Box::new(Echo));
        registry
    }

    fn call(args: &str) -> ToolCall {
        ToolCall {
            id: "c1".to_owned(),
            name: "echo".to_owned(),
            arguments_json: args.to_owned(),
        }
    }

    #[tokio::test]
    async fn allow_rule_runs_without_prompting() {
        let registry = make_registry("allow = [\"echo\"]", vec![]);
        let execution = registry
            .execute(&call(r#"{"text": "hi"}"#), &ToolContext::new("."))
            .await;
        assert_eq!(execution.outcome, ToolOutcome::Completed);
        assert_eq!(
            crate::guard::unwrap_untrusted(&execution.result.content),
            Some("hi")
        );
        assert!(!execution.result.is_error);
        assert_eq!(execution.result.call_id, "c1");
    }

    #[tokio::test]
    async fn default_for_write_access_is_ask_then_no_declines() {
        let registry = make_registry("", vec![PromptAnswer::No]);
        let execution = registry
            .execute(&call(r#"{"text": "hi"}"#), &ToolContext::new("."))
            .await;
        assert_eq!(execution.outcome, ToolOutcome::DeclinedByUser);
        assert!(execution.result.is_error);
        assert!(execution.outcome.is_denial());
    }

    #[tokio::test]
    async fn ask_then_always_records_rule_and_stops_prompting() {
        let registry = make_registry("", vec![PromptAnswer::Always]);
        let ctx = ToolContext::new(".");
        let first = registry.execute(&call(r#"{"text": "hi"}"#), &ctx).await;
        assert_eq!(first.outcome, ToolOutcome::Completed);
        assert_eq!(
            registry.permissions().rules().allow,
            vec!["echo".to_owned()]
        );
        // The scripted prompt has no answers left; a second prompt would say No.
        let second = registry.execute(&call(r#"{"text": "again"}"#), &ctx).await;
        assert_eq!(second.outcome, ToolOutcome::Completed);
    }

    #[tokio::test]
    async fn an_always_answer_is_reported_as_saved_only_when_its_rule_is_kept() {
        let scopes = |saving: bool| async move {
            let observer = Arc::new(RecordingObserver::default());
            let mut registry = make_registry("", vec![PromptAnswer::Always]).with_always(saving);
            registry.set_observer(Arc::clone(&observer) as Arc<dyn PermissionObserver>);
            registry
                .execute(&call(r#"{"text": "hi"}"#), &ToolContext::new("."))
                .await;
            lock_or_recover(&observer.0)
                .iter()
                .map(|decision| (decision.scope, decision.source))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            scopes(true).await,
            [(DecisionScope::Saved, DecisionSource::Prompt)]
        );
        assert_eq!(
            scopes(false).await,
            [(DecisionScope::Session, DecisionSource::Prompt)]
        );
    }

    #[tokio::test]
    async fn without_saving_always_counts_for_the_session_and_is_not_offered() {
        let registry = make_registry("", vec![PromptAnswer::Always]).with_always(false);
        let ctx = ToolContext::new(".");
        let first = registry.execute(&call(r#"{"text": "hi"}"#), &ctx).await;
        assert_eq!(first.outcome, ToolOutcome::Completed);
        assert!(
            registry.permissions().rules().allow.is_empty(),
            "no rule is recorded"
        );
        let second = registry.execute(&call(r#"{"text": "again"}"#), &ctx).await;
        assert_eq!(
            second.outcome,
            ToolOutcome::Completed,
            "the session approval holds"
        );
        let request = PermissionRequest {
            tool: "shell".to_owned(),
            target: None,
            arguments: Value::Null,
            may_save: false,
        };
        assert!(!request.offers_always());
    }

    #[tokio::test]
    async fn deny_rule_wins_over_allow() {
        let registry = make_registry("allow = [\"echo\"]\ndeny = [\"echo:secret*\"]", vec![]);
        let execution = registry
            .execute(&call(r#"{"text": "secret stuff"}"#), &ToolContext::new("."))
            .await;
        assert_eq!(execution.outcome, ToolOutcome::DeniedByRule);
        assert_eq!(execution.result.content, "denied by a permission rule");
    }

    #[tokio::test]
    async fn a_deny_rule_on_one_path_blocks_a_batched_read() {
        let mut registry = make_registry("deny = [\"read_file:.env\"]", vec![]);
        registry.register(Box::new(read_file::ReadFile));
        let read = ToolCall {
            id: "r1".to_owned(),
            name: "read_file".to_owned(),
            arguments_json: r#"{"files": [{"path": "Cargo.toml"}, {"path": ".env"}]}"#.to_owned(),
        };
        let execution = registry.execute(&read, &ToolContext::new(".")).await;
        assert_eq!(execution.outcome, ToolOutcome::DeniedByRule);
    }

    #[tokio::test]
    async fn blocked_call_needs_explicit_allow_rule() {
        let registry = make_registry("", vec![PromptAnswer::Yes]);
        let blocked = registry
            .execute(&call(r#"{"text": "nuke it"}"#), &ToolContext::new("."))
            .await;
        assert_eq!(blocked.outcome, ToolOutcome::Blocked);
        assert!(blocked.result.content.contains("blocked unless"));

        let allowed = make_registry("allow = [\"echo:nuke*\"]", vec![]);
        let execution = allowed
            .execute(&call(r#"{"text": "nuke it"}"#), &ToolContext::new("."))
            .await;
        assert_eq!(execution.outcome, ToolOutcome::Completed);
    }

    #[tokio::test]
    async fn output_is_truncated_and_kept_on_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = make_registry("allow = [\"echo\"]", vec![]);
        let ctx = ToolContext {
            session_dir: Some(dir.path().to_path_buf()),
            ..ToolContext::new(".")
        };
        let execution = registry
            .execute(&call(r#"{"text": "0123456789", "times": 50}"#), &ctx)
            .await;
        assert_eq!(execution.outcome, ToolOutcome::Completed);
        let path = execution.full_output.expect("full output kept");
        assert_eq!(path, dir.path().join("tool-output").join("c1.txt"));
        assert_eq!(std::fs::read_to_string(&path).expect("file").len(), 500);
        assert!(execution.stats.truncated);
        assert_eq!(execution.stats.bytes, 500);
        assert_eq!(
            execution.stats.digest,
            digest(std::fs::read_to_string(&path).expect("file").as_bytes())
        );
        assert!(
            crate::guard::unwrap_untrusted(&execution.result.content)
                .expect("wrapped")
                .starts_with(&"0123456789".repeat(10))
        );
        assert!(execution.result.content.contains("100 of 500 bytes shown"));
        assert!(execution.result.content.contains("c1.txt"));
    }

    #[tokio::test]
    async fn truncation_without_session_dir_still_notes() {
        let registry = make_registry("allow = [\"echo\"]", vec![]);
        let execution = registry
            .execute(
                &call(r#"{"text": "x", "times": 200}"#),
                &ToolContext::new("."),
            )
            .await;
        assert_eq!(execution.full_output, None);
        assert!(execution.result.content.contains("100 of 200 bytes shown"));
        assert!(!execution.result.content.contains("full output at"));
    }

    #[tokio::test]
    async fn failed_tool_is_reported_as_failed() {
        let registry = make_registry("allow = [\"echo\"]", vec![]);
        let execution = registry
            .execute(
                &call(r#"{"text": "x", "fail": true}"#),
                &ToolContext::new("."),
            )
            .await;
        assert_eq!(execution.outcome, ToolOutcome::Failed);
        assert!(execution.result.is_error);
    }

    /// Blocks calls whose arguments carry `"block": true` and annotates
    /// those carrying `"note": "<text>"`.
    struct ScriptedHook;

    impl ToolHook for ScriptedHook {
        fn before<'a>(
            &'a self,
            _call: &'a ToolCall,
            args: &'a Value,
            _ctx: &'a ToolContext,
        ) -> HookFuture<'a, HookDecision> {
            Box::pin(async move {
                if args.get("block").and_then(Value::as_bool) == Some(true) {
                    HookDecision::Block {
                        reason: "policy says no".to_owned(),
                    }
                } else {
                    HookDecision::Allow
                }
            })
        }

        fn after<'a>(
            &'a self,
            _call: &'a ToolCall,
            args: &'a Value,
            result: &'a ToolResult,
            _ctx: &'a ToolContext,
        ) -> HookFuture<'a, Option<String>> {
            Box::pin(async move {
                args.get("note")
                    .and_then(Value::as_str)
                    .map(|note| format!("{note} after {}", result.content.len()))
            })
        }
    }

    #[tokio::test]
    async fn a_hook_can_block_a_call_and_annotate_a_result() {
        let mut registry = make_registry("allow = [\"echo\"]", vec![]);
        registry.set_hook(Arc::new(ScriptedHook));
        let ctx = ToolContext::new(".");
        let blocked = registry
            .execute(&call(r#"{"text": "x", "block": true}"#), &ctx)
            .await;
        assert_eq!(blocked.outcome, ToolOutcome::Blocked);
        assert!(blocked.result.is_error);
        assert_eq!(
            blocked.result.content,
            format!(
                "blocked by a hook: {}",
                ctx.guard.wrap("hook", "policy says no")
            )
        );

        let noted = registry
            .execute(&call(r#"{"text": "hi", "note": "seen"}"#), &ctx)
            .await;
        assert_eq!(noted.outcome, ToolOutcome::Completed);
        assert_eq!(
            noted.result.content,
            format!(
                "{}\n\n[hook] {}",
                ctx.guard.wrap("echo", "hi"),
                ctx.guard.wrap("hook", "seen after 2")
            )
        );
    }

    #[tokio::test]
    async fn unknown_tool_and_bad_arguments() {
        let registry = make_registry("allow = [\"echo\"]", vec![]);
        let mut unknown = call("{}");
        unknown.name = "teleport".to_owned();
        let execution = registry.execute(&unknown, &ToolContext::new(".")).await;
        assert_eq!(execution.outcome, ToolOutcome::UnknownTool);
        assert!(execution.result.content.contains("available tools: echo"));

        let execution = registry
            .execute(&call("{not json"), &ToolContext::new("."))
            .await;
        assert_eq!(execution.outcome, ToolOutcome::InvalidArguments);

        let execution = registry
            .execute(&call(r#"{"times": 2}"#), &ToolContext::new("."))
            .await;
        assert_eq!(execution.outcome, ToolOutcome::Failed);
        assert!(execution.result.content.contains("invalid arguments"));
    }

    /// Lets a test keep a handle on the prompt the registry owns.
    struct SharedPrompt(Arc<ScriptedPrompt>);

    impl PermissionPrompt for SharedPrompt {
        fn ask(&self, request: &PermissionRequest) -> PromptAnswer {
            self.0.ask(request)
        }
    }

    /// Keeps every decision it hears.
    #[derive(Default)]
    struct RecordingObserver(Mutex<Vec<PermissionDecision>>);

    impl PermissionObserver for RecordingObserver {
        fn decided(&self, decision: &PermissionDecision) {
            lock_or_recover(&self.0).push(decision.clone());
        }
    }

    /// A workspace with five small files and a builtin registry that answers
    /// with `answers`.
    struct EditBench {
        dir: tempfile::TempDir,
        prompt: Arc<ScriptedPrompt>,
        observer: Arc<RecordingObserver>,
        registry: ToolRegistry,
    }

    impl EditBench {
        fn new(answers: Vec<PromptAnswer>, approvals: SessionApprovals) -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            std::fs::create_dir(dir.path().join("sub")).expect("mkdir");
            for i in 0..5 {
                std::fs::write(dir.path().join(format!("f{i}.txt")), "old\n").expect("write");
            }
            let prompt = Arc::new(ScriptedPrompt::new(answers));
            let observer = Arc::new(RecordingObserver::default());
            let mut registry = ToolRegistry::with_builtins(
                Permissions::empty(),
                Box::new(SharedPrompt(Arc::clone(&prompt))),
            )
            .with_session_approvals(Arc::new(Mutex::new(approvals)));
            registry.set_observer(Arc::clone(&observer) as Arc<dyn PermissionObserver>);
            Self {
                dir,
                prompt,
                observer,
                registry,
            }
        }

        async fn edit(&self, path: &str) -> ToolOutcome {
            let args = serde_json::json!({"path": path, "old": "old", "new": "new"});
            let call = ToolCall {
                id: "e1".to_owned(),
                name: "edit_file".to_owned(),
                arguments_json: args.to_string(),
            };
            self.registry
                .execute(&call, &ToolContext::new(self.dir.path()))
                .await
                .outcome
        }

        fn decisions(&self) -> Vec<PermissionDecision> {
            lock_or_recover(&self.observer.0).clone()
        }
    }

    fn decision(scope: DecisionScope, covered: bool) -> PermissionDecision {
        PermissionDecision {
            tool: "edit_file".to_owned(),
            scope,
            covered,
            source: if covered {
                DecisionSource::SessionApproval
            } else {
                DecisionSource::Prompt
            },
        }
    }

    #[tokio::test]
    async fn session_approval_stops_later_edit_prompts_inside_the_root() {
        let bench = EditBench::new(vec![PromptAnswer::Session], SessionApprovals::default());
        assert_eq!(bench.edit("f0.txt").await, ToolOutcome::Completed);
        let absolute = bench.dir.path().join("f2.txt");
        for path in [
            "./f1.txt",
            absolute.to_str().expect("utf-8"),
            "sub/../f3.txt",
            "f4.txt",
        ] {
            assert_eq!(bench.edit(path).await, ToolOutcome::Completed, "{path}");
        }
        assert_eq!(bench.prompt.requests().len(), 1);
        let mut expected = vec![decision(DecisionScope::Session, false)];
        expected.extend(std::iter::repeat_n(
            decision(DecisionScope::Session, true),
            4,
        ));
        assert_eq!(bench.decisions(), expected);
    }

    #[tokio::test]
    async fn edit_outside_the_root_still_prompts_after_a_session_approval() {
        let bench = EditBench::new(
            vec![PromptAnswer::Session, PromptAnswer::No],
            SessionApprovals::default(),
        );
        assert_eq!(bench.edit("f0.txt").await, ToolOutcome::Completed);
        assert_eq!(
            bench.edit("../outside.txt").await,
            ToolOutcome::DeclinedByUser
        );
        let requests = bench.prompt.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].target.as_deref(), Some("../outside.txt"));
        assert_eq!(
            bench.decisions(),
            vec![
                decision(DecisionScope::Session, false),
                decision(DecisionScope::Deny, false)
            ]
        );
    }

    #[tokio::test]
    async fn preapproved_tool_never_prompts() {
        let bench = EditBench::new(
            vec![],
            SessionApprovals::preapproved(["edit_file"]).expect("valid"),
        );
        for i in 0..5 {
            assert_eq!(
                bench.edit(&format!("f{i}.txt")).await,
                ToolOutcome::Completed
            );
        }
        assert!(bench.prompt.requests().is_empty());
        assert!(
            bench
                .decisions()
                .iter()
                .all(|d| *d == decision(DecisionScope::Session, true))
        );
    }

    #[tokio::test]
    async fn retried_call_after_a_session_approval_does_not_prompt() {
        let bench = EditBench::new(vec![PromptAnswer::Session], SessionApprovals::default());
        assert_eq!(bench.edit("f0.txt").await, ToolOutcome::Completed);
        // The same call again: `old` is gone now, so the tool fails, but
        // the permission check must pass without asking.
        assert_eq!(bench.edit("f0.txt").await, ToolOutcome::Failed);
        assert_eq!(bench.prompt.requests().len(), 1);
    }

    #[tokio::test]
    async fn shared_approvals_carry_over_to_a_second_registry() {
        let bench = EditBench::new(vec![PromptAnswer::Session], SessionApprovals::default());
        assert_eq!(bench.edit("f0.txt").await, ToolOutcome::Completed);
        let second = ToolRegistry::with_builtins(
            Permissions::empty(),
            Box::new(StaticPrompt(PromptAnswer::No)),
        )
        .with_session_approvals(bench.registry.session_approvals());
        let call = ToolCall {
            id: "e2".to_owned(),
            name: "edit_file".to_owned(),
            arguments_json: serde_json::json!({"path": "f1.txt", "old": "old", "new": "new"})
                .to_string(),
        };
        let execution = second
            .execute(&call, &ToolContext::new(bench.dir.path()))
            .await;
        assert_eq!(execution.outcome, ToolOutcome::Completed);
    }

    /// Waits before answering, so a concurrent call has time to reach its
    /// own permission check while this prompt is open.
    struct SlowPrompt(Arc<ScriptedPrompt>);

    impl PermissionPrompt for SlowPrompt {
        fn ask(&self, request: &PermissionRequest) -> PromptAnswer {
            std::thread::sleep(std::time::Duration::from_millis(200));
            self.0.ask(request)
        }
    }

    #[test]
    fn concurrent_registries_sharing_approvals_prompt_only_once() {
        let bench = EditBench::new(vec![], SessionApprovals::default());
        let first_prompt = Arc::new(ScriptedPrompt::new(vec![PromptAnswer::Session]));
        let second_prompt = Arc::new(ScriptedPrompt::new(vec![PromptAnswer::Session]));
        let approvals = bench.registry.session_approvals();
        let mut first = ToolRegistry::with_builtins(
            Permissions::empty(),
            Box::new(SlowPrompt(Arc::clone(&first_prompt))),
        )
        .with_session_approvals(Arc::clone(&approvals));
        let mut second = ToolRegistry::with_builtins(
            Permissions::empty(),
            Box::new(SlowPrompt(Arc::clone(&second_prompt))),
        )
        .with_session_approvals(approvals);
        for registry in [&mut first, &mut second] {
            registry.set_observer(Arc::clone(&bench.observer) as Arc<dyn PermissionObserver>);
        }
        let start = std::sync::Barrier::new(2);
        let run = |registry: &ToolRegistry, path: &str| {
            let args = serde_json::json!({"path": path, "old": "old", "new": "new"});
            let call = ToolCall {
                id: path.to_owned(),
                name: "edit_file".to_owned(),
                arguments_json: args.to_string(),
            };
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            start.wait();
            runtime
                .block_on(registry.execute(&call, &ToolContext::new(bench.dir.path())))
                .outcome
        };
        let (a, b) = std::thread::scope(|scope| {
            let a = scope.spawn(|| run(&first, "f0.txt"));
            let b = scope.spawn(|| run(&second, "f1.txt"));
            (a.join().expect("first"), b.join().expect("second"))
        });
        assert_eq!((a, b), (ToolOutcome::Completed, ToolOutcome::Completed));
        assert_eq!(
            first_prompt.requests().len() + second_prompt.requests().len(),
            1,
            "a session answer in one registry covers the queued call in the other"
        );
        let mut decisions = bench.decisions();
        decisions.sort_by_key(|decision| decision.covered);
        assert_eq!(
            decisions,
            vec![
                decision(DecisionScope::Session, false),
                decision(DecisionScope::Session, true)
            ]
        );
    }

    #[tokio::test]
    async fn once_answer_is_reported_and_prompts_again() {
        let bench = EditBench::new(
            vec![PromptAnswer::Yes, PromptAnswer::Yes],
            SessionApprovals::default(),
        );
        assert_eq!(bench.edit("f0.txt").await, ToolOutcome::Completed);
        assert_eq!(bench.edit("f1.txt").await, ToolOutcome::Completed);
        assert_eq!(bench.prompt.requests().len(), 2);
        assert_eq!(
            bench.decisions(),
            vec![
                decision(DecisionScope::Once, false),
                decision(DecisionScope::Once, false)
            ]
        );
    }

    #[tokio::test]
    async fn approvals_shared_with_a_registry_in_another_root_still_prompt() {
        let bench = EditBench::new(vec![PromptAnswer::Session], SessionApprovals::default());
        assert_eq!(bench.edit("f0.txt").await, ToolOutcome::Completed);
        let other = tempfile::tempdir().expect("other workspace");
        std::fs::write(other.path().join("f0.txt"), "old\n").expect("write");
        let other_prompt = Arc::new(ScriptedPrompt::new(vec![PromptAnswer::No]));
        let second = ToolRegistry::with_builtins(
            Permissions::empty(),
            Box::new(SharedPrompt(Arc::clone(&other_prompt))),
        )
        .with_session_approvals(bench.registry.session_approvals());
        let call = ToolCall {
            id: "e2".to_owned(),
            name: "edit_file".to_owned(),
            arguments_json: serde_json::json!({"path": "f0.txt", "old": "old", "new": "new"})
                .to_string(),
        };
        let execution = second.execute(&call, &ToolContext::new(other.path())).await;
        assert_eq!(execution.outcome, ToolOutcome::DeclinedByUser);
        assert_eq!(
            other_prompt.requests().len(),
            1,
            "the second workspace prompts"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_file_through_a_symlink_after_a_missing_dir_still_prompts() {
        let dir = tempfile::tempdir().expect("workspace");
        let outside = tempfile::tempdir().expect("outside");
        std::os::unix::fs::symlink(outside.path(), dir.path().join("escape")).expect("symlink");
        let prompt = Arc::new(ScriptedPrompt::new(vec![
            PromptAnswer::Session,
            PromptAnswer::No,
        ]));
        let registry = ToolRegistry::with_builtins(
            Permissions::empty(),
            Box::new(SharedPrompt(Arc::clone(&prompt))),
        );
        let ctx = ToolContext::new(dir.path());
        let write = |id: &str, path: &str| ToolCall {
            id: id.to_owned(),
            name: "write_file".to_owned(),
            arguments_json: serde_json::json!({"path": path, "content": "hi"}).to_string(),
        };
        let first = registry.execute(&write("w1", "a.txt"), &ctx).await;
        assert_eq!(first.outcome, ToolOutcome::Completed);
        let inside = registry.execute(&write("w2", "new/../b.txt"), &ctx).await;
        assert_eq!(
            inside.outcome,
            ToolOutcome::Completed,
            "in-root spelling stays covered"
        );
        let escaping = registry
            .execute(&write("w3", "new/../escape/file.txt"), &ctx)
            .await;
        assert_eq!(escaping.outcome, ToolOutcome::DeclinedByUser);
        assert!(!outside.path().join("file.txt").exists());
        let requests = prompt.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[1].target.as_deref(),
            Some("new/../escape/file.txt")
        );
    }

    /// The decisions an `echo` registry with `rules` reports for `text`.
    async fn echo_decisions(rules: &str, text: &str) -> (ToolOutcome, Vec<PermissionDecision>) {
        let observer = Arc::new(RecordingObserver::default());
        let mut registry = make_registry(rules, vec![]);
        registry.set_observer(Arc::clone(&observer) as Arc<dyn PermissionObserver>);
        let args = serde_json::json!({ "text": text }).to_string();
        let outcome = registry
            .execute(&call(&args), &ToolContext::new("."))
            .await
            .outcome;
        let decisions = lock_or_recover(&observer.0).clone();
        (outcome, decisions)
    }

    fn echo_decision(scope: DecisionScope, covered: bool) -> PermissionDecision {
        PermissionDecision {
            tool: "echo".to_owned(),
            scope,
            covered,
            source: if scope == DecisionScope::Blocked {
                DecisionSource::BlockedList
            } else {
                DecisionSource::Rule(RuleOrigin::User)
            },
        }
    }

    #[tokio::test]
    async fn a_closed_gate_refuses_every_call_and_command() {
        struct Closed;
        impl CallGate for Closed {
            fn refusal(&self) -> Option<String> {
                Some("the audit log cannot be written".to_owned())
            }
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let mut registry = ToolRegistry::with_builtins(
            Permissions::from_toml("allow = [\"write_file\", \"shell\"]", None).expect("rules"),
            Box::new(StaticPrompt(PromptAnswer::Yes)),
        );
        registry.set_gate(Arc::new(Closed));
        let write = ToolCall {
            id: "w1".to_owned(),
            name: "write_file".to_owned(),
            arguments_json: serde_json::json!({"path": "a.txt", "content": "x"}).to_string(),
        };
        let execution = registry
            .execute(&write, &ToolContext::new(dir.path()))
            .await;
        assert_eq!(execution.outcome, ToolOutcome::Blocked);
        assert!(execution.result.content.contains("audit log"));
        assert!(!dir.path().join("a.txt").exists());
        let args = serde_json::json!({"command": "echo hi"});
        assert!(registry.permit("shell", &args, dir.path()).is_err());
    }

    #[tokio::test]
    async fn a_gate_closed_by_the_decision_record_stops_the_hook_and_the_call() {
        /// Closes when its decision observer is told anything, as a
        /// required audit log does when writing the decision fails.
        #[derive(Default)]
        struct FailsOnRecord(std::sync::atomic::AtomicBool);
        impl CallGate for FailsOnRecord {
            fn refusal(&self) -> Option<String> {
                self.0
                    .load(std::sync::atomic::Ordering::SeqCst)
                    .then(|| "the audit log cannot be written".to_owned())
            }
        }
        impl PermissionObserver for FailsOnRecord {
            fn decided(&self, _decision: &PermissionDecision) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        #[derive(Default)]
        struct CountingHook(std::sync::atomic::AtomicUsize);
        impl ToolHook for CountingHook {
            fn before<'a>(
                &'a self,
                _call: &'a ToolCall,
                _args: &'a Value,
                _ctx: &'a ToolContext,
            ) -> HookFuture<'a, HookDecision> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Box::pin(async { HookDecision::Allow })
            }

            fn after<'a>(
                &'a self,
                _call: &'a ToolCall,
                _args: &'a Value,
                _result: &'a ToolResult,
                _ctx: &'a ToolContext,
            ) -> HookFuture<'a, Option<String>> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Box::pin(async { None })
            }
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let gate = Arc::new(FailsOnRecord::default());
        let hook = Arc::new(CountingHook::default());
        let mut registry = ToolRegistry::with_builtins(
            Permissions::from_toml("allow = [\"write_file\", \"shell\"]", None).expect("rules"),
            Box::new(StaticPrompt(PromptAnswer::Yes)),
        );
        registry.set_gate(Arc::clone(&gate) as Arc<dyn CallGate>);
        registry.set_observer(Arc::clone(&gate) as Arc<dyn PermissionObserver>);
        registry.set_hook(Arc::clone(&hook) as Arc<dyn ToolHook>);
        let write = ToolCall {
            id: "w1".to_owned(),
            name: "write_file".to_owned(),
            arguments_json: serde_json::json!({"path": "a.txt", "content": "x"}).to_string(),
        };
        let execution = registry
            .execute(&write, &ToolContext::new(dir.path()))
            .await;
        assert_eq!(execution.outcome, ToolOutcome::Blocked);
        assert!(!dir.path().join("a.txt").exists(), "the call did not run");
        assert_eq!(
            hook.0.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "nor the hook"
        );

        let fresh = Arc::new(FailsOnRecord::default());
        registry.set_gate(Arc::clone(&fresh) as Arc<dyn CallGate>);
        registry.set_observer(Arc::clone(&fresh) as Arc<dyn PermissionObserver>);
        let args = serde_json::json!({"command": "echo hi"});
        assert!(
            registry.permit("shell", &args, dir.path()).is_err(),
            "a command is not permitted once its decision could not be recorded"
        );
    }

    #[tokio::test]
    async fn rule_and_blocked_outcomes_are_reported_too() {
        assert_eq!(
            echo_decisions("allow = [\"echo\"]", "hi").await,
            (
                ToolOutcome::Completed,
                vec![echo_decision(DecisionScope::Rule, true)]
            )
        );
        assert_eq!(
            echo_decisions("default = \"allow\"", "hi").await,
            (
                ToolOutcome::Completed,
                vec![echo_decision(DecisionScope::Rule, true)]
            )
        );
        assert_eq!(
            echo_decisions("deny = [\"echo\"]", "hi").await,
            (
                ToolOutcome::DeniedByRule,
                vec![echo_decision(DecisionScope::Deny, false)]
            )
        );
        assert_eq!(
            echo_decisions("", "nuke it").await,
            (
                ToolOutcome::Blocked,
                vec![echo_decision(DecisionScope::Blocked, false)]
            )
        );
    }

    #[tokio::test]
    async fn a_read_tools_built_in_default_is_not_a_reported_decision() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.txt"), "x\n").expect("write");
        let observer = Arc::new(RecordingObserver::default());
        let mut registry = ToolRegistry::with_builtins(
            Permissions::empty(),
            Box::new(StaticPrompt(PromptAnswer::No)),
        );
        registry.set_observer(Arc::clone(&observer) as Arc<dyn PermissionObserver>);
        let read = ToolCall {
            id: "r1".to_owned(),
            name: "read_file".to_owned(),
            arguments_json: serde_json::json!({"path": "a.txt"}).to_string(),
        };
        let execution = registry.execute(&read, &ToolContext::new(dir.path())).await;
        assert_eq!(execution.outcome, ToolOutcome::Completed);
        assert!(lock_or_recover(&observer.0).is_empty());
    }

    #[test]
    fn permit_decides_like_a_call_without_running_anything() {
        let registry = |rules: &str, answer: PromptAnswer| {
            ToolRegistry::with_builtins(
                Permissions::from_toml(rules, None).expect("rules parse"),
                Box::new(StaticPrompt(answer)),
            )
        };
        let root = Path::new("/project");
        let args = serde_json::json!({"command": "bash -n tool.sh"});
        let allowed = registry("allow = [\"shell:bash *\"]", PromptAnswer::No);
        assert_eq!(allowed.permit("shell", &args, root), Ok(()));
        let denied = registry("deny = [\"shell\"]", PromptAnswer::Yes);
        assert_eq!(
            denied.permit("shell", &args, root),
            Err("denied by a permission rule".to_owned())
        );
        let asked = registry("", PromptAnswer::No);
        assert!(asked.permit("shell", &args, root).is_err());
        let answered_yes = registry("", PromptAnswer::Yes);
        assert_eq!(answered_yes.permit("shell", &args, root), Ok(()));
        assert_eq!(
            allowed.permit("no_such_tool", &args, root),
            Err("unknown tool `no_such_tool`".to_owned())
        );
    }

    #[test]
    fn specs_list_every_builtin_sorted() {
        let registry = ToolRegistry::with_builtins(
            Permissions::empty(),
            Box::new(StaticPrompt(PromptAnswer::No)),
        );
        assert_eq!(
            registry.names(),
            vec![
                "edit_file",
                "glob",
                "grep",
                "list_dir",
                "outline",
                "read_file",
                "shell",
                "write_file"
            ]
        );
        let specs = registry.specs();
        assert_eq!(specs.len(), 8);
        for spec in &specs {
            assert_eq!(spec.parameters["type"], "object", "{}", spec.name);
            assert!(spec.parameters.get("$schema").is_none());
            assert!(!spec.description.is_empty());
        }
        insta::assert_json_snapshot!(specs);
    }

    #[tokio::test]
    async fn the_built_in_writing_tools_and_the_shell_ask_when_no_rule_decides() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.txt"), "old").expect("write");
        let registry = ToolRegistry::with_builtins(
            Permissions::empty(),
            Box::new(StaticPrompt(PromptAnswer::No)),
        );
        let ctx = ToolContext::new(dir.path());
        for (name, args) in [
            ("write_file", r#"{"path": "b.txt", "content": "new"}"#),
            (
                "edit_file",
                r#"{"path": "a.txt", "old": "old", "new": "new"}"#,
            ),
            ("shell", r#"{"command": "echo hi"}"#),
        ] {
            let call = ToolCall {
                id: "c1".to_owned(),
                name: name.to_owned(),
                arguments_json: args.to_owned(),
            };
            let execution = registry.execute(&call, &ctx).await;
            assert_eq!(execution.outcome, ToolOutcome::DeclinedByUser, "{name}");
        }
        assert!(!dir.path().join("b.txt").exists(), "nothing was written");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).expect("read"),
            "old"
        );
        let read = ToolCall {
            id: "c2".to_owned(),
            name: "read_file".to_owned(),
            arguments_json: r#"{"path": "a.txt"}"#.to_owned(),
        };
        assert_eq!(
            registry.execute(&read, &ctx).await.outcome,
            ToolOutcome::Completed,
            "a read needs no answer"
        );
    }

    #[test]
    fn display_path_is_relative_with_forward_slashes() {
        let ctx = ToolContext::new(Path::new("/proj"));
        assert_eq!(ctx.display_path(Path::new("/proj/src/a.rs")), "src/a.rs");
        assert_eq!(ctx.display_path(Path::new("/proj")), ".");
        assert_eq!(ctx.display_path(Path::new("/elsewhere/x")), "/elsewhere/x");
        assert_eq!(ctx.resolve("/abs"), PathBuf::from("/abs"));
        assert_eq!(ctx.resolve("rel"), PathBuf::from("/proj").join("rel"));
    }

    /// Runs one built-in tool with every call allowed by the rules, so only
    /// the workspace jail can stop it.
    async fn jailed(ctx: &ToolContext, tool: &str, args: Value) -> Execution {
        let permissions = Permissions::from_toml("default = \"allow\"", None).expect("rules");
        let registry =
            ToolRegistry::with_builtins(permissions, Box::new(ScriptedPrompt::new(vec![])));
        let call = ToolCall {
            id: "j1".to_owned(),
            name: tool.to_owned(),
            arguments_json: args.to_string(),
        };
        registry.execute(&call, ctx).await
    }

    fn workspace_with_outside() -> (tempfile::TempDir, tempfile::TempDir, ToolContext) {
        let project = tempfile::tempdir().expect("project");
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(project.path().join("in.txt"), "inside\n").expect("write");
        std::fs::write(outside.path().join("out.txt"), "outside\n").expect("write");
        let ctx = ToolContext::new(project.path());
        (project, outside, ctx)
    }

    #[tokio::test]
    async fn file_tools_refuse_paths_outside_the_workspace_reads_included() {
        let (project, outside, ctx) = workspace_with_outside();
        let absolute = outside.path().join("out.txt").display().to_string();
        let dotted = format!(
            "../{}/out.txt",
            outside.path().file_name().expect("name").to_string_lossy()
        );
        for path in [absolute.as_str(), dotted.as_str()] {
            for (tool, args) in [
                ("read_file", serde_json::json!({"path": path})),
                (
                    "write_file",
                    serde_json::json!({"path": path, "content": "x"}),
                ),
                (
                    "edit_file",
                    serde_json::json!({"path": path, "old": "outside", "new": "x"}),
                ),
                (
                    "grep",
                    serde_json::json!({"pattern": "outside", "path": path}),
                ),
                ("outline", serde_json::json!({"path": path})),
            ] {
                let run = jailed(&ctx, tool, args).await;
                assert!(run.result.is_error, "{tool} {path}: {}", run.result.content);
                assert!(
                    run.result.content.contains("outside the workspace"),
                    "{tool} {path}: {}",
                    run.result.content
                );
            }
        }
        let parent = outside.path().display().to_string();
        for (tool, args) in [
            ("list_dir", serde_json::json!({"path": parent})),
            ("glob", serde_json::json!({"pattern": "*", "path": parent})),
        ] {
            let run = jailed(&ctx, tool, args).await;
            assert!(
                run.result.content.contains("outside the workspace"),
                "{tool}"
            );
        }
        assert_eq!(
            std::fs::read_to_string(outside.path().join("out.txt")).expect("read"),
            "outside\n",
            "nothing outside was changed"
        );
        let inside = jailed(&ctx, "read_file", serde_json::json!({"path": "in.txt"})).await;
        assert!(
            inside.result.content.contains("inside"),
            "{}",
            inside.result.content
        );
        drop(project);
    }

    #[tokio::test]
    async fn extra_dirs_open_outside_folders_and_read_only_dirs_refuse_writes() {
        let (_project, outside, mut ctx) = workspace_with_outside();
        let file = outside.path().join("out.txt").display().to_string();
        ctx.workspace.read_only_dirs = vec![outside.path().to_path_buf()];
        let read = jailed(&ctx, "read_file", serde_json::json!({"path": file})).await;
        assert!(
            read.result.content.contains("outside"),
            "{}",
            read.result.content
        );
        let write = jailed(
            &ctx,
            "write_file",
            serde_json::json!({"path": file, "content": "x"}),
        )
        .await;
        assert!(write.result.content.contains("outside the workspace"));

        ctx.workspace.read_only_dirs.clear();
        ctx.workspace.extra_dirs = vec![outside.path().to_path_buf()];
        let write = jailed(
            &ctx,
            "write_file",
            serde_json::json!({"path": file, "content": "x"}),
        )
        .await;
        assert!(!write.result.is_error, "{}", write.result.content);
    }

    #[tokio::test]
    async fn the_session_folder_is_writable_even_outside_the_project() {
        let (_project, outside, mut ctx) = workspace_with_outside();
        ctx.session_dir = Some(outside.path().to_path_buf());
        let plan = outside.path().join("plan.md").display().to_string();
        let run = jailed(
            &ctx,
            "write_file",
            serde_json::json!({"path": plan, "content": "x"}),
        )
        .await;
        assert!(!run.result.is_error, "{}", run.result.content);
    }

    #[tokio::test]
    async fn secrets_are_refused_inside_the_workspace_and_skipped_by_searches() {
        let project = tempfile::tempdir().expect("project");
        std::fs::write(project.path().join(".env"), "API_KEY=needle\n").expect("write");
        std::fs::create_dir_all(project.path().join(".ssh")).expect("mkdir");
        std::fs::write(project.path().join(".ssh").join("id_ed25519"), "needle").expect("write");
        std::fs::write(project.path().join("notes.txt"), "needle here\n").expect("write");
        let ctx = ToolContext::new(project.path());
        for path in [".env", "./.env", ".ssh/id_ed25519", "sub/../.env"] {
            let run = jailed(&ctx, "read_file", serde_json::json!({"path": path})).await;
            assert!(
                run.result.content.contains("holds secrets"),
                "{path}: {}",
                run.result.content
            );
        }
        let grep = jailed(&ctx, "grep", serde_json::json!({"pattern": "needle"})).await;
        assert!(
            grep.result.content.contains("notes.txt"),
            "{}",
            grep.result.content
        );
        assert!(
            !grep.result.content.contains(".env"),
            "{}",
            grep.result.content
        );
        let grep_hidden = jailed(
            &ctx,
            "grep",
            serde_json::json!({"pattern": "needle", "path": ".env"}),
        )
        .await;
        assert!(grep_hidden.result.content.contains("holds secrets"));
    }

    #[tokio::test]
    async fn shell_programs_beat_an_allow_rule_and_a_yes_to_everything() {
        let rules = "allow = [\"shell\"]\nshell_programs = [\"git\"]\n";
        let permissions = Permissions::from_toml(rules, None).expect("rules");
        let registry = ToolRegistry::with_builtins(
            permissions,
            Box::new(crate::tools::StaticPrompt(PromptAnswer::Yes)),
        );
        let dir = tempfile::tempdir().expect("dir");
        let ctx = ToolContext::new(dir.path());
        for command in [
            "git --version && echo pwned",
            "echo pwned",
            "git log; echo pwned",
        ] {
            let call = ToolCall {
                id: "s1".to_owned(),
                name: "shell".to_owned(),
                arguments_json: serde_json::json!({ "command": command }).to_string(),
            };
            let run = registry.execute(&call, &ctx).await;
            assert_eq!(run.outcome, ToolOutcome::Blocked, "{command}");
            assert!(!run.result.content.contains("pwned\n"), "{command} ran");
        }
    }

    /// Files that grant power, as a model might name them.
    const POWER_FILES: [&str; 10] = [
        ".anvil/permissions.toml",
        ".anvil/config.toml",
        ".claude/settings.json",
        ".claude/settings.local.json",
        ".anvil/settings.json",
        ".anvil/settings.local.json",
        ".mcp.json",
        "src/../.anvil/permissions.toml",
        "./.claude/../.claude/settings.json",
        "sub/.mcp.json",
    ];

    /// Writes `path` through `write_file` and then `edit_file`, returning
    /// both runs.
    async fn write_and_edit(
        registry: &ToolRegistry,
        ctx: &ToolContext,
        path: &str,
    ) -> Vec<Execution> {
        let mut runs = Vec::new();
        for (tool, args) in [
            (
                "write_file",
                serde_json::json!({"path": path, "content": "allow = [\"shell\"]"}),
            ),
            (
                "edit_file",
                serde_json::json!({"path": path, "old": "deny", "new": "allow"}),
            ),
        ] {
            let call = ToolCall {
                id: "h1".to_owned(),
                name: tool.to_owned(),
                arguments_json: args.to_string(),
            };
            runs.push(registry.execute(&call, ctx).await);
        }
        runs
    }

    /// A registry that allows every write, pre-approves the write tools and
    /// answers every prompt with `always`: everything a rule, profile,
    /// `--allow-tool`, `--yes` or a person could grant.
    fn everything_allowed() -> ToolRegistry {
        let rules = "default = \"allow\"\nallow = [\"write_file\", \"edit_file\"]\n";
        ToolRegistry::with_builtins(
            Permissions::from_toml(rules, None).expect("rules"),
            Box::new(StaticPrompt(PromptAnswer::Always)),
        )
        .with_session_approvals(Arc::new(Mutex::new(
            SessionApprovals::preapproved(["write_file", "edit_file"]).expect("valid"),
        )))
    }

    #[tokio::test]
    async fn tool_writes_to_files_that_grant_power_are_refused() {
        let project = tempfile::tempdir().expect("project");
        std::fs::create_dir_all(project.path().join(".anvil")).expect("mkdir");
        std::fs::create_dir_all(project.path().join("src")).expect("mkdir");
        let original = "deny = [\"shell\"]\n";
        std::fs::write(project.path().join(".anvil/permissions.toml"), original).expect("write");
        let ctx = ToolContext::new(project.path());
        let registry = everything_allowed();
        let absolute = project
            .path()
            .join(".anvil")
            .join("permissions.toml")
            .display()
            .to_string();
        let mut paths: Vec<&str> = POWER_FILES.to_vec();
        paths.push(&absolute);
        if cfg!(windows) {
            paths.extend([
                ".ANVIL/Permissions.toml",
                ".Claude/SETTINGS.json",
                ".MCP.json",
            ]);
        }
        for path in paths {
            for run in write_and_edit(&registry, &ctx, path).await {
                assert!(run.result.is_error, "{path}: {}", run.result.content);
                assert!(
                    run.result.content.contains("never write it"),
                    "{path}: {}",
                    run.result.content
                );
            }
        }
        assert_eq!(
            std::fs::read_to_string(project.path().join(".anvil/permissions.toml"))
                .expect("still there"),
            original
        );
        assert!(!project.path().join(".claude").exists());
        assert!(!project.path().join(".mcp.json").exists());

        // Other files in the same folders are not affected.
        for path in [".claude/agents/a.md", ".anvil/notes.md", "settings.json"] {
            let runs = write_and_edit(&registry, &ctx, path).await;
            assert_eq!(runs[0].outcome, ToolOutcome::Completed, "{path}");
        }
    }

    #[tokio::test]
    async fn names_windows_folds_onto_a_file_that_grants_power_are_refused() {
        let project = tempfile::tempdir().expect("project");
        std::fs::create_dir_all(project.path().join(".anvil")).expect("mkdir");
        std::fs::create_dir_all(project.path().join(".claude")).expect("mkdir");
        let ctx = ToolContext::new(project.path());
        let registry = everything_allowed();
        for path in [
            ".anvil/config.toml.",
            ".anvil/permissions.toml ",
            ".claude/settings.json::$DATA",
            ".mcp.json.",
            ".anvil./config.toml",
            "notes.md.",
        ] {
            for run in write_and_edit(&registry, &ctx, path).await {
                assert!(run.result.is_error, "{path}: {}", run.result.content);
                assert!(
                    run.result
                        .content
                        .contains("Windows would open a different file"),
                    "{path}: {}",
                    run.result.content
                );
            }
        }
        for written in [
            ".anvil/config.toml",
            ".anvil/permissions.toml",
            ".claude/settings.json",
            ".mcp.json",
            "notes.md",
        ] {
            assert!(
                !project.path().join(written).exists(),
                "{written} was written"
            );
        }
        let names: Vec<String> = std::fs::read_dir(project.path().join(".anvil"))
            .expect("list")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert!(names.is_empty(), "{names:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlink_to_a_file_that_grants_power_is_refused() {
        let project = tempfile::tempdir().expect("project");
        std::fs::create_dir_all(project.path().join(".anvil")).expect("mkdir");
        std::fs::write(project.path().join(".anvil/permissions.toml"), "").expect("write");
        std::os::unix::fs::symlink(
            project.path().join(".anvil/permissions.toml"),
            project.path().join("notes.toml"),
        )
        .expect("symlink");
        let ctx = ToolContext::new(project.path());
        for run in write_and_edit(&everything_allowed(), &ctx, "notes.toml").await {
            assert!(
                run.result.content.contains("never write it"),
                "{}",
                run.result.content
            );
        }
    }

    #[tokio::test]
    async fn tool_writes_into_the_users_anvil_folder_are_refused() {
        let home = tempfile::tempdir().expect("home");
        let anvil_home = home.path().join(".anvil");
        std::fs::create_dir_all(anvil_home.join("permissions")).expect("mkdir");
        let ctx = ToolContext {
            workspace: Workspace {
                extra_dirs: vec![home.path().to_path_buf()],
                protected_dirs: vec![anvil_home.clone()],
                ..Workspace::default()
            },
            ..ToolContext::new(home.path())
        };
        let registry = everything_allowed();
        for path in [
            ".anvil/trust.toml",
            ".anvil/permissions/repo.toml",
            ".anvil/rules.toml",
            ".anvil/profiles/strict.toml",
        ] {
            for run in write_and_edit(&registry, &ctx, path).await {
                assert!(run.result.is_error, "{path}: {}", run.result.content);
            }
        }
        assert!(!anvil_home.join("trust.toml").exists());
        let runs = write_and_edit(&registry, &ctx, "notes.md").await;
        assert_eq!(runs[0].outcome, ToolOutcome::Completed);
    }

    /// Runs plain git for test setup; `false` when git is unavailable.
    fn git_setup(dir: &Path, args: &[&str]) -> bool {
        std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(["-c", "protocol.file.allow=always"])
            .args(args)
            .current_dir(dir)
            .output()
            .is_ok_and(|output| output.status.success())
    }

    /// A git repository with one commit, or `None` without git.
    fn committed_repository(dir: &Path) -> Option<()> {
        if !git_setup(dir, &["init", "-q"]) {
            eprintln!("git not available; skipping");
            return None;
        }
        std::fs::write(dir.join("a.txt"), "one\n").expect("write");
        assert!(git_setup(dir, &["add", "a.txt"]));
        assert!(git_setup(dir, &["commit", "-q", "-m", "init"]));
        Some(())
    }

    /// Asserts that `write_file` and `edit_file` both refuse `path` as a
    /// git directory.
    async fn assert_git_write_refused(ctx: &ToolContext, path: &str) {
        for (tool, args) in [
            (
                "write_file",
                serde_json::json!({"path": path, "content": "x"}),
            ),
            (
                "edit_file",
                serde_json::json!({"path": path, "old": "[core]", "new": "x"}),
            ),
        ] {
            let run = jailed(ctx, tool, args).await;
            assert!(run.result.is_error, "{tool} {path}: {}", run.result.content);
            assert!(
                run.result.content.contains("inside a git directory"),
                "{tool} {path}: {}",
                run.result.content
            );
        }
    }

    #[tokio::test]
    async fn writes_to_git_config_and_hooks_are_refused() {
        let project = tempfile::tempdir().expect("project");
        let root = project.path();
        let Some(()) = committed_repository(root) else {
            return;
        };
        let config = root.join(".git").join("config");
        let before = std::fs::read_to_string(&config).expect("config");
        let ctx = ToolContext::new(root);
        let absolute = config.display().to_string();
        for path in [
            ".git/config",
            ".git/hooks/pre-commit",
            ".git/hooks/post-checkout",
            ".git/info/exclude",
            ".git/info/attributes",
            ".git/HEAD",
            "./.git/config",
            "src/../.git/config",
            ".GIT/config",
            absolute.as_str(),
        ] {
            assert_git_write_refused(&ctx, path).await;
        }
        assert_eq!(
            std::fs::read_to_string(&config).expect("config"),
            before,
            "the config is unchanged"
        );
        assert!(!root.join(".git").join("hooks").join("pre-commit").exists());

        let read = jailed(&ctx, "read_file", serde_json::json!({"path": ".git/HEAD"})).await;
        assert!(
            read.result.content.contains("ref:"),
            "reads of .git stay allowed"
        );
        let read = jailed(
            &ctx,
            "read_file",
            serde_json::json!({"path": ".git/config"}),
        )
        .await;
        assert!(
            read.result.content.contains("holds secrets"),
            "except .git/config, which can hold a remote URL with a token: {}",
            read.result.content
        );
        for path in [".gitignore", ".github/workflows/ci.yml", "git/notes.md"] {
            let run = jailed(
                &ctx,
                "write_file",
                serde_json::json!({"path": path, "content": "x"}),
            )
            .await;
            assert!(!run.result.is_error, "{path}: {}", run.result.content);
        }
    }

    #[tokio::test]
    async fn writes_to_worktree_and_submodule_git_directories_are_refused() {
        let project = tempfile::tempdir().expect("project");
        let root = project.path();
        let Some(()) = committed_repository(root) else {
            return;
        };
        let library = tempfile::tempdir().expect("library");
        assert!(committed_repository(library.path()).is_some());
        let library_path = library.path().to_string_lossy().replace('\\', "/");
        assert!(git_setup(root, &["worktree", "add", "-q", "wt"]));
        assert!(git_setup(
            root,
            &["submodule", "add", "-q", &library_path, "vendor/lib"]
        ));
        assert!(root.join("wt").join(".git").is_file(), "a worktree link");
        assert!(root.join("vendor/lib/.git").is_file(), "a submodule link");

        let ctx = ToolContext::new(root);
        for path in [
            "wt/.git",
            ".git/worktrees/wt/HEAD",
            ".git/worktrees/wt/commondir",
            "vendor/lib/.git",
            ".git/modules/vendor/lib/config",
            ".git/modules/vendor/lib/hooks/pre-commit",
        ] {
            assert_git_write_refused(&ctx, path).await;
        }
        let in_worktree = ToolContext::new(root.join("wt"));
        assert_git_write_refused(&in_worktree, ".git").await;
        let ok = jailed(
            &in_worktree,
            "write_file",
            serde_json::json!({"path": "a.txt", "content": "two\n"}),
        )
        .await;
        assert!(!ok.result.is_error, "{}", ok.result.content);
    }

    #[tokio::test]
    async fn writes_to_bare_and_separate_git_directories_are_refused() {
        let project = tempfile::tempdir().expect("project");
        let root = project.path();
        if !git_setup(root, &["init", "-q", "--bare", "store.git"]) {
            eprintln!("git not available; skipping");
            return;
        }
        assert!(git_setup(
            root,
            &["init", "-q", "--separate-git-dir", "meta", "checkout"]
        ));
        let ctx = ToolContext::new(root);
        for path in [
            "store.git/config",
            "store.git/hooks/post-receive",
            "meta/config",
            "meta/hooks/pre-commit",
            "checkout/.git",
        ] {
            assert_git_write_refused(&ctx, path).await;
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn a_junction_that_points_outside_is_refused() {
        let (project, outside, ctx) = workspace_with_outside();
        let link = project.path().join("link");
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(outside.path())
            .output()
            .expect("cmd runs");
        assert!(
            made.status.success(),
            "{}",
            String::from_utf8_lossy(&made.stderr)
        );
        assert!(link.join("out.txt").is_file(), "the junction works");
        for (tool, args) in [
            ("read_file", serde_json::json!({"path": "link/out.txt"})),
            ("outline", serde_json::json!({"path": "link/out.txt"})),
            ("list_dir", serde_json::json!({"path": "link"})),
            ("glob", serde_json::json!({"pattern": "*", "path": "link"})),
            (
                "grep",
                serde_json::json!({"pattern": "outside", "path": "link"}),
            ),
            (
                "write_file",
                serde_json::json!({"path": "link/out.txt", "content": "x"}),
            ),
            (
                "write_file",
                serde_json::json!({"path": "link/new.txt", "content": "x"}),
            ),
            (
                "edit_file",
                serde_json::json!({"path": "link/out.txt", "old": "outside", "new": "x"}),
            ),
        ] {
            let run = jailed(&ctx, tool, args).await;
            assert!(
                run.result.content.contains("outside the workspace"),
                "{tool}: {}",
                run.result.content
            );
        }
        let search = jailed(&ctx, "grep", serde_json::json!({"pattern": "outside"})).await;
        assert!(
            !search.result.content.contains("out.txt"),
            "a search from the root does not follow the junction: {}",
            search.result.content
        );
        assert_eq!(
            std::fs::read_to_string(outside.path().join("out.txt")).expect("read"),
            "outside\n"
        );
        assert!(!outside.path().join("new.txt").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlink_that_points_outside_is_refused() {
        let (project, outside, ctx) = workspace_with_outside();
        std::os::unix::fs::symlink(outside.path(), project.path().join("link")).expect("symlink");
        let run = jailed(
            &ctx,
            "read_file",
            serde_json::json!({"path": "link/out.txt"}),
        )
        .await;
        assert!(
            run.result.content.contains("outside the workspace"),
            "{}",
            run.result.content
        );
    }
}
