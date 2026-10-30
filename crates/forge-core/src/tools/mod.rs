//! Tools the model can call, and the registry that offers, guards and runs them.
//!
//! [`ToolRegistry::execute`] is the single path from a model's tool call to a
//! result: it parses the arguments, applies the permission model, runs the
//! tool, truncates the output to the tool's byte cap and keeps the full
//! output on disk. Nothing here prints; prompting goes through
//! [`PermissionPrompt`], which the CLI implements.

pub mod edit_file;
pub mod glob;
pub mod grep;
pub mod list_dir;
mod output;
pub mod permissions;
pub mod read_file;
pub mod shell;
pub mod write_file;

use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Mutex, MutexGuard};

use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::provider::lock_or_recover;
use crate::provider::types::{ToolCall, ToolResult, ToolSpec};

pub use permissions::{
    Decision, PermissionError, PermissionPrompt, PermissionRequest, Permissions, PromptAnswer,
    ScriptedPrompt, StaticPrompt,
};
pub use shell::Shell;

/// Byte cap applied to a tool's output unless the tool overrides it.
pub const DEFAULT_OUTPUT_CAP: usize = 30 * 1024;

/// The future a tool's `run` returns.
pub type ToolFuture<'a> = Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>>;

/// What a tool produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    /// Text for the model.
    pub content: String,
    /// Whether the tool failed.
    pub is_error: bool,
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
        }
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
    fn name(&self) -> &'static str;

    /// What it does, for the model.
    fn description(&self) -> &'static str;

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

    /// Why this call must not run even if a prompt would allow it.
    ///
    /// Only an explicit `allow` rule can override a blocked call.
    fn blocked_reason(&self, _args: &Value) -> Option<String> {
        None
    }

    /// Runs the tool.
    fn run<'a>(&'a self, args: Value, ctx: &'a ToolContext) -> ToolFuture<'a>;
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
        }
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
}

/// The tools on offer, with the permission model that guards them.
pub struct ToolRegistry {
    /// Tools by name.
    tools: BTreeMap<&'static str, Box<dyn Tool>>,
    /// Rules, mutated when the user answers `always`.
    permissions: Mutex<Permissions>,
    /// Who to ask when a rule says `ask`.
    prompt: Box<dyn PermissionPrompt>,
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
        }
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
        registry
    }

    /// Adds or replaces a tool.
    ///
    /// # Arguments
    ///
    /// * `tool` - The tool.
    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.insert(tool.name(), tool);
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
    pub fn names(&self) -> Vec<&'static str> {
        self.tools.keys().copied().collect()
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
        let target = tool.target(&args);
        if let Some(outcome) = self.check_permission(tool, &args, target.as_deref()) {
            return Execution::finished(call, outcome.0, outcome.1);
        }

        let output = tool.run(args, ctx).await;
        let outcome = if output.is_error {
            ToolOutcome::Failed
        } else {
            ToolOutcome::Completed
        };
        let Some(truncated) = output::truncate(&output.content, tool.output_cap()) else {
            return Execution::finished(call, output, outcome);
        };
        let full_output = ctx.session_dir.as_deref().and_then(|dir| {
            match output::write_full(dir, &call.id, &output.content) {
                Ok(path) => Some(path),
                Err(error) => {
                    tracing::warn!(%error, "could not keep full tool output");
                    None
                }
            }
        });
        let content = output::with_note(truncated, output.content.len(), full_output.as_deref());
        Execution {
            result: ToolResult {
                call_id: call.id.clone(),
                content,
                is_error: output.is_error,
            },
            outcome,
            full_output,
        }
    }

    /// Applies the blocked list and the rules; `None` means the call may run.
    fn check_permission(
        &self,
        tool: &dyn Tool,
        args: &Value,
        target: Option<&str>,
    ) -> Option<(ToolOutput, ToolOutcome)> {
        let name = tool.name();
        let mut permissions = lock_or_recover(&self.permissions);
        if let Some(reason) = tool.blocked_reason(args)
            && !permissions.explicitly_allowed(name, target)
        {
            tracing::info!(tool = name, ?target, reason, "blocked tool call");
            return Some((
                ToolOutput::error(format!(
                    "{reason}; blocked unless an `allow` rule in .forge/permissions.toml covers it"
                )),
                ToolOutcome::Blocked,
            ));
        }
        match permissions.decide(name, target, tool.access().default_decision()) {
            Decision::Allow => None,
            Decision::Deny => {
                tracing::info!(tool = name, ?target, "denied by permission rule");
                Some((
                    ToolOutput::error("denied by a permission rule".to_owned()),
                    ToolOutcome::DeniedByRule,
                ))
            }
            Decision::Ask => {
                let request = PermissionRequest {
                    tool: name.to_owned(),
                    target: target.map(str::to_owned),
                    arguments: args.clone(),
                };
                match self.prompt.ask(&request) {
                    PromptAnswer::Yes => None,
                    PromptAnswer::No => {
                        tracing::info!(tool = name, ?target, "declined by user");
                        Some((
                            ToolOutput::error("the user declined this call".to_owned()),
                            ToolOutcome::DeclinedByUser,
                        ))
                    }
                    PromptAnswer::Always => {
                        let rule = permissions::always_rule(name, target);
                        if let Err(error) = permissions.add_allow(&rule) {
                            tracing::warn!(%error, rule, "could not save the permission rule");
                        }
                        None
                    }
                }
            }
        }
    }
}

impl Execution {
    /// Wraps an untruncated output.
    fn finished(call: &ToolCall, output: ToolOutput, outcome: ToolOutcome) -> Self {
        Self {
            result: ToolResult {
                call_id: call.id.clone(),
                content: output.content,
                is_error: output.is_error,
            },
            outcome,
            full_output: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        fn name(&self) -> &'static str {
            "echo"
        }
        fn description(&self) -> &'static str {
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
        fn blocked_reason(&self, args: &Value) -> Option<String> {
            args.get("text")
                .and_then(Value::as_str)
                .filter(|text| text.contains("nuke"))
                .map(|_| "text contains `nuke`".to_owned())
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
        assert_eq!(execution.result.content, "hi");
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
    async fn deny_rule_wins_over_allow() {
        let registry = make_registry("allow = [\"echo\"]\ndeny = [\"echo:secret*\"]", vec![]);
        let execution = registry
            .execute(&call(r#"{"text": "secret stuff"}"#), &ToolContext::new("."))
            .await;
        assert_eq!(execution.outcome, ToolOutcome::DeniedByRule);
        assert_eq!(execution.result.content, "denied by a permission rule");
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
        assert!(
            execution
                .result
                .content
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
                "read_file",
                "shell",
                "write_file"
            ]
        );
        let specs = registry.specs();
        assert_eq!(specs.len(), 7);
        for spec in &specs {
            assert_eq!(spec.parameters["type"], "object", "{}", spec.name);
            assert!(spec.parameters.get("$schema").is_none());
            assert!(!spec.description.is_empty());
        }
        insta::assert_json_snapshot!(specs);
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
}
