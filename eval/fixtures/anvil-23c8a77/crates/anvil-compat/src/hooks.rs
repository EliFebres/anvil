//! Claude Code hooks: shell commands run around tool calls and sessions.
//!
//! The `hooks` section of a settings file maps an event to matcher groups,
//! each with a list of commands. A command gets the event as JSON on stdin.
//! Exit code 2 blocks the tool call (or objects to stopping) and its stderr
//! goes back to the model; exit code 0 lets it proceed and its stdout is
//! added as context. The JSON keys are the ones Claude Code sends, so
//! existing hook scripts run unchanged.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anvil_core::provider::{ToolCall, ToolResult};
use anvil_core::tools::{HookDecision, HookFuture, Shell, ToolContext, ToolHook};
use regex::Regex;
use serde_json::{Map, Value, json};
use tokio::io::AsyncWriteExt;

use crate::error::{CompatError, LoadError};
use crate::names::to_claude_name;

/// How long a hook may run when it sets no `timeout`.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// The exit code that blocks a call.
const BLOCK_EXIT_CODE: i32 = 2;

/// Settings files read for a `hooks` section, relative to the project root.
const PROJECT_SETTINGS: &[&str] = &[
    ".claude/settings.json",
    ".claude/settings.local.json",
    ".anvil/settings.json",
];

/// Settings files read for a `hooks` section, relative to the home directory.
const HOME_SETTINGS: &[&str] = &[".claude/settings.json", ".anvil/settings.json"];

/// The events anvil runs hooks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HookEvent {
    /// Before an allowed tool call.
    PreToolUse,
    /// After a tool call returned.
    PostToolUse,
    /// When the agent has finished responding.
    Stop,
    /// When a session begins.
    SessionStart,
}

impl HookEvent {
    /// The name Claude Code uses in settings and payloads.
    ///
    /// # Returns
    ///
    /// The event name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PreToolUse => "PreToolUse",
            Self::PostToolUse => "PostToolUse",
            Self::Stop => "Stop",
            Self::SessionStart => "SessionStart",
        }
    }

    /// Parses a settings key.
    ///
    /// # Arguments
    ///
    /// * `name` - The key under `hooks`.
    ///
    /// # Returns
    ///
    /// The event, or `None` for events anvil does not run.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "PreToolUse" => Some(Self::PreToolUse),
            "PostToolUse" => Some(Self::PostToolUse),
            "Stop" => Some(Self::Stop),
            "SessionStart" => Some(Self::SessionStart),
            _ => None,
        }
    }
}

impl fmt::Display for HookEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One shell command to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookCommand {
    /// The command line, run through the configured shell.
    pub command: String,
    /// When to kill it.
    pub timeout: Duration,
}

/// Commands that share a matcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookGroup {
    /// The tool-name pattern, or `None` for every tool.
    pub matcher: Option<String>,
    /// The commands, run in order.
    pub commands: Vec<HookCommand>,
    /// The settings file they came from.
    pub source: PathBuf,
}

impl HookGroup {
    /// Whether this group applies to a subject (a tool name, or the
    /// session source for `SessionStart`).
    ///
    /// The matcher is a regex anchored to the whole name, tried against the
    /// Claude Code spelling first (`Bash` for `shell`) and then the anvil
    /// name. An empty matcher or `*` matches everything; a matcher that is
    /// not a valid regex is compared literally.
    ///
    /// # Arguments
    ///
    /// * `subject` - The anvil tool name or event source.
    ///
    /// # Returns
    ///
    /// `true` when the group's commands should run.
    pub fn matches(&self, subject: &str) -> bool {
        let Some(matcher) = self.matcher.as_deref().map(str::trim) else {
            return true;
        };
        if matcher.is_empty() || matcher == "*" {
            return true;
        }
        let claude_name = to_claude_name(subject);
        match Regex::new(&format!("^(?:{matcher})$")) {
            Ok(regex) => regex.is_match(&claude_name) || regex.is_match(subject),
            Err(_) => matcher == claude_name || matcher == subject,
        }
    }
}

/// Every hook found in the settings files.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HookConfig {
    /// Groups per supported event, in settings-file order.
    groups: BTreeMap<HookEvent, Vec<HookGroup>>,
    /// Event names anvil does not run, with the file that set them.
    unsupported: Vec<(String, PathBuf)>,
    /// Settings files that exist but did not parse.
    errors: Vec<LoadError>,
    /// Settings files that were read.
    sources: Vec<PathBuf>,
}

impl HookConfig {
    /// The settings files consulted, in order.
    ///
    /// # Arguments
    ///
    /// * `cwd` - The project root.
    /// * `home` - The home directory, if known.
    ///
    /// # Returns
    ///
    /// The paths, whether or not they exist.
    pub fn settings_files(cwd: &Path, home: Option<&Path>) -> Vec<PathBuf> {
        let mut files: Vec<PathBuf> = PROJECT_SETTINGS.iter().map(|name| cwd.join(name)).collect();
        if let Some(home) = home {
            files.extend(HOME_SETTINGS.iter().map(|name| home.join(name)));
        }
        files
    }

    /// Reads every settings file and merges their `hooks` sections.
    ///
    /// # Arguments
    ///
    /// * `cwd` - The project root.
    /// * `home` - The home directory, if known.
    ///
    /// # Returns
    ///
    /// The config. Files that do not exist are skipped; files that do not
    /// parse are listed in `errors`.
    pub fn load(cwd: &Path, home: Option<&Path>) -> Self {
        let mut config = Self::default();
        for path in Self::settings_files(cwd, home) {
            let text = match std::fs::read_to_string(&path) {
                Ok(text) => text,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    config.errors.push(LoadError {
                        error: CompatError::io(&path, &error),
                        path,
                    });
                    continue;
                }
            };
            let settings: Value = match serde_json::from_str(&text) {
                Ok(settings) => settings,
                Err(error) => {
                    config.errors.push(LoadError {
                        error: CompatError::Frontmatter {
                            path: path.clone(),
                            detail: format!("not valid JSON: {error}"),
                        },
                        path,
                    });
                    continue;
                }
            };
            config.sources.push(path.clone());
            if let Some(hooks) = settings.get("hooks") {
                config.add(hooks, &path);
            }
        }
        config
    }

    /// Adds the groups of one `hooks` object.
    ///
    /// # Arguments
    ///
    /// * `hooks` - The value under the `hooks` key.
    /// * `source` - The file it came from.
    pub fn add(&mut self, hooks: &Value, source: &Path) {
        let Some(events) = hooks.as_object() else {
            return;
        };
        for (name, groups) in events {
            let Some(event) = HookEvent::parse(name) else {
                self.unsupported.push((name.clone(), source.to_path_buf()));
                continue;
            };
            for group in groups.as_array().into_iter().flatten() {
                let commands: Vec<HookCommand> = group
                    .get("hooks")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|hook| parse_command(hook, name, source, &mut self.unsupported))
                    .collect();
                if commands.is_empty() {
                    continue;
                }
                self.groups.entry(event).or_default().push(HookGroup {
                    matcher: group
                        .get("matcher")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    commands,
                    source: source.to_path_buf(),
                });
            }
        }
    }

    /// The groups for one event.
    ///
    /// # Arguments
    ///
    /// * `event` - The event.
    ///
    /// # Returns
    ///
    /// The groups, possibly empty.
    pub fn groups(&self, event: HookEvent) -> &[HookGroup] {
        self.groups.get(&event).map_or(&[], Vec::as_slice)
    }

    /// The events that have at least one command.
    ///
    /// # Returns
    ///
    /// The events, in enum order.
    pub fn events(&self) -> Vec<HookEvent> {
        self.groups.keys().copied().collect()
    }

    /// How many commands are configured across all events.
    ///
    /// # Returns
    ///
    /// The count.
    pub fn command_count(&self) -> usize {
        self.groups
            .values()
            .flatten()
            .map(|group| group.commands.len())
            .sum()
    }

    /// Whether no supported hook is configured.
    ///
    /// # Returns
    ///
    /// `true` when there is nothing to run.
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    /// Hook entries anvil does not run: unknown events and non-command
    /// hook types.
    ///
    /// # Returns
    ///
    /// Pairs of the entry name and the file that set it.
    pub fn unsupported(&self) -> &[(String, PathBuf)] {
        &self.unsupported
    }

    /// Settings files that exist but did not parse.
    ///
    /// # Returns
    ///
    /// The errors.
    pub fn errors(&self) -> &[LoadError] {
        &self.errors
    }

    /// Settings files that were read.
    ///
    /// # Returns
    ///
    /// The paths.
    pub fn sources(&self) -> &[PathBuf] {
        &self.sources
    }
}

/// Parses one hook object; non-command types are recorded as unsupported.
fn parse_command(
    hook: &Value,
    event: &str,
    source: &Path,
    unsupported: &mut Vec<(String, PathBuf)>,
) -> Option<HookCommand> {
    let kind = hook
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("command");
    if kind != "command" {
        unsupported.push((
            format!("{event} hook of type `{kind}`"),
            source.to_path_buf(),
        ));
        return None;
    }
    let command = hook.get("command").and_then(Value::as_str)?.trim();
    if command.is_empty() {
        return None;
    }
    let timeout = hook
        .get("timeout")
        .and_then(Value::as_f64)
        .filter(|secs| *secs > 0.0)
        .map_or(DEFAULT_TIMEOUT, Duration::from_secs_f64);
    Some(HookCommand {
        command: command.to_owned(),
        timeout,
    })
}

/// What the hooks of one event decided.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HookOutcome {
    /// The reason from the first hook that exited with code 2.
    pub blocked: Option<String>,
    /// Standard output of the hooks that exited with code 0, in order.
    pub context: Vec<String>,
}

/// Runs the configured hooks for one session.
pub struct HookRunner {
    /// What to run.
    config: Arc<HookConfig>,
    /// The project root; hooks run there.
    cwd: PathBuf,
    /// The shell that runs the commands.
    shell: Shell,
    /// The session id for payloads.
    session_id: String,
    /// The session folder, sent as `transcript_path`.
    transcript_path: Option<PathBuf>,
}

impl HookRunner {
    /// Builds a runner.
    ///
    /// # Arguments
    ///
    /// * `config` - The hooks.
    /// * `cwd` - The project root.
    /// * `shell` - The shell to run commands with.
    /// * `session_id` - The current session id.
    /// * `transcript_path` - The session folder, if any.
    ///
    /// # Returns
    ///
    /// The runner.
    pub fn new(
        config: Arc<HookConfig>,
        cwd: PathBuf,
        shell: Shell,
        session_id: String,
        transcript_path: Option<PathBuf>,
    ) -> Self {
        Self {
            config,
            cwd,
            shell,
            session_id,
            transcript_path,
        }
    }

    /// Whether any command is configured for an event.
    ///
    /// # Arguments
    ///
    /// * `event` - The event.
    ///
    /// # Returns
    ///
    /// `true` when running the event would do something.
    pub fn has(&self, event: HookEvent) -> bool {
        !self.config.groups(event).is_empty()
    }

    /// Runs every matching command for an event.
    ///
    /// # Arguments
    ///
    /// * `event` - The event.
    /// * `subject` - The anvil tool name (or session source) matchers are
    ///   tested against; `None` matches every group.
    /// * `fields` - Event-specific payload keys, merged over the common ones.
    ///
    /// # Returns
    ///
    /// The first block reason and the collected context. Commands after a
    /// blocking one do not run.
    pub async fn run(
        &self,
        event: HookEvent,
        subject: Option<&str>,
        fields: Map<String, Value>,
    ) -> HookOutcome {
        let mut outcome = HookOutcome::default();
        let mut payload = json!({
            "session_id": self.session_id,
            "transcript_path": self.transcript_path.as_ref().map(|path| path.to_string_lossy().into_owned()),
            "cwd": self.cwd.to_string_lossy(),
            "hook_event_name": event.as_str(),
        });
        if let Some(object) = payload.as_object_mut() {
            object.extend(fields);
        }
        let payload = payload.to_string();
        for group in self.config.groups(event) {
            if subject.is_some_and(|subject| !group.matches(subject)) {
                continue;
            }
            for command in &group.commands {
                match self.run_command(command, &payload).await {
                    Ok(run) if run.exit_code == Some(BLOCK_EXIT_CODE) => {
                        let reason = run.stderr.trim();
                        outcome.blocked = Some(if reason.is_empty() {
                            format!("hook `{}` exited with code 2", command.command)
                        } else {
                            reason.to_owned()
                        });
                        return outcome;
                    }
                    Ok(run) if run.exit_code == Some(0) => {
                        if !run.stdout.trim().is_empty() {
                            outcome.context.push(run.stdout.trim_end().to_owned());
                        }
                    }
                    Ok(run) => tracing::warn!(
                        %event,
                        command = command.command,
                        exit_code = ?run.exit_code,
                        stderr = run.stderr.trim(),
                        "hook failed; continuing"
                    ),
                    Err(error) => {
                        tracing::warn!(%event, command = command.command, error, "hook did not run");
                    }
                }
            }
        }
        outcome
    }

    /// Runs the `SessionStart` hooks.
    ///
    /// # Returns
    ///
    /// Context lines to add to the system prompt.
    pub async fn session_start(&self) -> Vec<String> {
        let fields = json_fields([("source", json!("startup"))]);
        self.run(HookEvent::SessionStart, Some("startup"), fields)
            .await
            .context
    }

    /// Runs the `Stop` hooks.
    ///
    /// # Arguments
    ///
    /// * `last_message` - The agent's final text.
    /// * `stop_hook_active` - `true` when this stop is already the result
    ///   of a Stop hook asking for more work, so hooks can avoid loops.
    ///
    /// # Returns
    ///
    /// The reason a hook objected to stopping, if one did.
    pub async fn stop(&self, last_message: &str, stop_hook_active: bool) -> Option<String> {
        let fields = json_fields([
            ("stop_hook_active", json!(stop_hook_active)),
            ("last_message", json!(last_message)),
        ]);
        self.run(HookEvent::Stop, None, fields).await.blocked
    }

    /// Spawns one command with the payload on stdin.
    async fn run_command(
        &self,
        command: &HookCommand,
        payload: &str,
    ) -> Result<CommandRun, String> {
        let mut child = self
            .shell
            .command(&command.command)
            .current_dir(&self.cwd)
            .env("CLAUDE_PROJECT_DIR", &self.cwd)
            .env("ANVIL_PROJECT_DIR", &self.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| format!("cannot start `{}`: {error}", self.shell.program()))?;
        if let Some(mut stdin) = child.stdin.take() {
            // A hook that never reads stdin closes it early; that is not an error.
            let _ = stdin.write_all(payload.as_bytes()).await;
            let _ = stdin.write_all(b"\n").await;
            let _ = stdin.shutdown().await;
        }
        let output = tokio::time::timeout(command.timeout, child.wait_with_output())
            .await
            .map_err(|_| format!("timed out after {}s", command.timeout.as_secs()))?
            .map_err(|error| format!("cannot wait for the hook: {error}"))?;
        Ok(CommandRun {
            exit_code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// What a finished hook command produced.
struct CommandRun {
    /// The exit code, or `None` when killed by a signal.
    exit_code: Option<i32>,
    /// Captured standard output.
    stdout: String,
    /// Captured standard error.
    stderr: String,
}

/// Builds a payload object from key-value pairs.
fn json_fields<const N: usize>(pairs: [(&str, Value); N]) -> Map<String, Value> {
    pairs
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect()
}

impl ToolHook for HookRunner {
    fn before<'a>(
        &'a self,
        call: &'a ToolCall,
        args: &'a Value,
        _ctx: &'a ToolContext,
    ) -> HookFuture<'a, HookDecision> {
        Box::pin(async move {
            if !self.has(HookEvent::PreToolUse) {
                return HookDecision::Allow;
            }
            let fields = json_fields([
                ("tool_name", json!(to_claude_name(&call.name))),
                ("anvil_tool_name", json!(call.name)),
                ("tool_input", args.clone()),
            ]);
            match self
                .run(HookEvent::PreToolUse, Some(&call.name), fields)
                .await
                .blocked
            {
                Some(reason) => HookDecision::Block { reason },
                None => HookDecision::Allow,
            }
        })
    }

    fn after<'a>(
        &'a self,
        call: &'a ToolCall,
        args: &'a Value,
        result: &'a ToolResult,
        _ctx: &'a ToolContext,
    ) -> HookFuture<'a, Option<String>> {
        Box::pin(async move {
            if !self.has(HookEvent::PostToolUse) {
                return None;
            }
            let fields = json_fields([
                ("tool_name", json!(to_claude_name(&call.name))),
                ("anvil_tool_name", json!(call.name)),
                ("tool_input", args.clone()),
                (
                    "tool_response",
                    json!({"content": result.content, "is_error": result.is_error}),
                ),
            ]);
            let outcome = self
                .run(HookEvent::PostToolUse, Some(&call.name), fields)
                .await;
            if let Some(reason) = outcome.blocked {
                return Some(reason);
            }
            (!outcome.context.is_empty()).then(|| outcome.context.join("\n"))
        })
    }
}

#[cfg(test)]
mod tests {
    use anvil_core::tools::{Permissions, PromptAnswer, StaticPrompt, ToolOutcome, ToolRegistry};

    use super::*;

    /// A command that writes `nope` to stderr and exits 2, in the platform shell.
    fn block_command() -> &'static str {
        if cfg!(windows) {
            "[Console]::Error.WriteLine('nope'); exit 2"
        } else {
            "echo nope >&2; exit 2"
        }
    }

    /// A command that echoes its stdin.
    fn echo_stdin_command() -> &'static str {
        if cfg!(windows) {
            "[Console]::In.ReadToEnd()"
        } else {
            "cat"
        }
    }

    fn settings(hooks: Value) -> String {
        json!({ "hooks": hooks }).to_string()
    }

    fn runner(config: HookConfig, cwd: &Path) -> Arc<HookRunner> {
        Arc::new(HookRunner::new(
            Arc::new(config),
            cwd.to_path_buf(),
            Shell::for_platform(),
            "sess-1".to_owned(),
            Some(cwd.join("session")),
        ))
    }

    fn call(name: &str, args: Value) -> ToolCall {
        ToolCall {
            id: "c1".to_owned(),
            name: name.to_owned(),
            arguments_json: args.to_string(),
        }
    }

    #[test]
    fn loads_and_merges_settings_files_keeping_unsupported_entries() {
        let project = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(project.path().join(".claude")).expect("mkdir");
        std::fs::create_dir_all(home.path().join(".claude")).expect("mkdir");
        std::fs::write(
            project.path().join(".claude/settings.json"),
            settings(json!({
                "PreToolUse": [
                    {"matcher": "Bash|Edit", "hooks": [{"type": "command", "command": "echo pre", "timeout": 5}]},
                    {"hooks": [{"type": "prompt", "prompt": "check"}]}
                ],
                "UserPromptSubmit": [{"hooks": [{"type": "command", "command": "echo x"}]}],
                "Stop": [{"hooks": [{"type": "command", "command": "echo stop"}]}]
            })),
        )
        .expect("write");
        std::fs::write(
            home.path().join(".claude/settings.json"),
            settings(json!({
                "PreToolUse": [{"matcher": "*", "hooks": [{"command": "echo home"}]}]
            })),
        )
        .expect("write");
        std::fs::create_dir_all(project.path().join(".anvil")).expect("mkdir");
        std::fs::write(project.path().join(".anvil/settings.json"), "{not json").expect("write");

        let config = HookConfig::load(project.path(), Some(home.path()));
        assert_eq!(config.events(), [HookEvent::PreToolUse, HookEvent::Stop]);
        assert_eq!(config.command_count(), 3);
        let pre = config.groups(HookEvent::PreToolUse);
        assert_eq!(pre.len(), 2);
        assert_eq!(pre[0].matcher.as_deref(), Some("Bash|Edit"));
        assert_eq!(pre[0].commands[0].timeout, Duration::from_secs(5));
        assert_eq!(pre[1].commands[0].timeout, DEFAULT_TIMEOUT);
        assert_eq!(pre[1].commands[0].command, "echo home");
        let names: Vec<&str> = config
            .unsupported()
            .iter()
            .map(|(n, _)| n.as_str())
            .collect();
        assert_eq!(
            names,
            ["PreToolUse hook of type `prompt`", "UserPromptSubmit"]
        );
        assert_eq!(config.errors().len(), 1);
        assert_eq!(config.sources().len(), 2);
    }

    #[test]
    fn matchers_see_claude_code_names() {
        let group = |matcher: Option<&str>| HookGroup {
            matcher: matcher.map(str::to_owned),
            commands: Vec::new(),
            source: PathBuf::new(),
        };
        assert!(group(Some("Bash|Edit")).matches("shell"));
        assert!(group(Some("Bash|Edit")).matches("edit_file"));
        assert!(!group(Some("Bash|Edit")).matches("glob"));
        assert!(group(Some("mcp__.*")).matches("mcp__fs__ls"));
        assert!(group(Some("read_file")).matches("read_file"));
        assert!(group(Some("*")).matches("anything"));
        assert!(group(Some("")).matches("anything"));
        assert!(group(None).matches("anything"));
        assert!(group(Some("Bash(")).matches("Bash("));
        assert!(!group(Some("Bash(")).matches("shell"));
    }

    #[tokio::test]
    async fn a_hook_that_exits_2_blocks_the_tool_and_the_model_sees_the_reason() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = HookConfig::default();
        config.add(
            &json!({"PreToolUse": [{"matcher": "LS", "hooks": [{"command": block_command()}]}]}),
            Path::new("settings.json"),
        );
        let mut registry = ToolRegistry::with_builtins(
            Permissions::empty(),
            Box::new(StaticPrompt(PromptAnswer::Yes)),
        );
        registry.set_hook(runner(config, dir.path()));
        let ctx = ToolContext::new(dir.path());

        let blocked = registry.execute(&call("list_dir", json!({})), &ctx).await;
        assert_eq!(blocked.outcome, ToolOutcome::Blocked);
        assert!(blocked.result.is_error);
        assert!(
            blocked.result.content.contains("nope"),
            "{}",
            blocked.result.content
        );

        let unmatched = registry
            .execute(&call("glob", json!({"pattern": "*.nothing"})), &ctx)
            .await;
        assert_ne!(unmatched.outcome, ToolOutcome::Blocked);
    }

    #[tokio::test]
    async fn post_tool_use_receives_the_claude_code_payload_and_adds_context() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = HookConfig::default();
        config.add(
            &json!({"PostToolUse": [{"hooks": [{"command": echo_stdin_command()}]}]}),
            Path::new("settings.json"),
        );
        let mut registry = ToolRegistry::with_builtins(
            Permissions::empty(),
            Box::new(StaticPrompt(PromptAnswer::Yes)),
        );
        registry.set_hook(runner(config, dir.path()));
        let ctx = ToolContext::new(dir.path());

        let execution = registry.execute(&call("list_dir", json!({})), &ctx).await;
        assert_eq!(execution.outcome, ToolOutcome::Completed);
        let (_, note) = execution
            .result
            .content
            .split_once("\n\n[hook] ")
            .expect("hook note appended");
        let payload: Value = serde_json::from_str(note.trim()).expect("payload is JSON");
        assert_eq!(payload["hook_event_name"], "PostToolUse");
        assert_eq!(payload["tool_name"], "LS");
        assert_eq!(payload["anvil_tool_name"], "list_dir");
        assert_eq!(payload["session_id"], "sess-1");
        assert_eq!(payload["tool_response"]["is_error"], false);
        assert!(
            payload["transcript_path"]
                .as_str()
                .is_some_and(|p| p.ends_with("session"))
        );
    }

    #[tokio::test]
    async fn session_start_collects_stdout_and_stop_can_object() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = HookConfig::default();
        config.add(
            &json!({
                "SessionStart": [
                    {"matcher": "startup|resume", "hooks": [{"command": "echo remember-the-milk"}]},
                    {"matcher": "compact", "hooks": [{"command": "echo not-this"}]}
                ],
                "Stop": [{"hooks": [{"command": block_command()}]}]
            }),
            Path::new("settings.json"),
        );
        let runner = runner(config, dir.path());
        assert_eq!(runner.session_start().await, ["remember-the-milk"]);
        assert_eq!(runner.stop("done", false).await.as_deref(), Some("nope"));
        assert!(!runner.has(HookEvent::PreToolUse));
    }
}
