//! The `shell` tool and the shell program behind it.

use std::process::Stdio;
use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;

use super::{Access, Tool, ToolContext, ToolFuture, ToolOutput, parse_args, schema_for};

/// Timeout when the model does not set one.
pub const DEFAULT_TIMEOUT_S: u64 = 120;

/// Longest timeout the model may request.
pub const MAX_TIMEOUT_S: u64 = 600;

/// Bytes of output kept in memory; the registry caps what the model sees.
const MEMORY_CAP: usize = 1024 * 1024;

/// Substrings that make a command refuse to run without an `allow` rule.
pub const BLOCKED_COMMANDS: [&str; 4] = [
    "rm -rf",
    "Remove-Item -Recurse",
    "git push --force",
    "git reset --hard",
];

/// The program that runs commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shell {
    /// Executable name or path.
    program: String,
    /// Arguments placed before the command text.
    args: Vec<String>,
}

impl Shell {
    /// PowerShell on Windows, `sh` elsewhere.
    ///
    /// # Returns
    ///
    /// The shell.
    pub fn for_platform() -> Self {
        if cfg!(windows) {
            Self::named("powershell")
        } else {
            Self::named("sh")
        }
    }

    /// The shell named in config, or the platform default.
    ///
    /// # Arguments
    ///
    /// * `name` - `[tools].shell` from config.
    ///
    /// # Returns
    ///
    /// The shell.
    pub fn from_config(name: Option<&str>) -> Self {
        match name {
            Some(name) if !name.trim().is_empty() => Self::named(name.trim()),
            _ => Self::for_platform(),
        }
    }

    /// Builds the invocation for a known program name.
    ///
    /// `powershell` and `pwsh` take `-NoProfile -NonInteractive -Command`,
    /// `cmd` takes `/C`, and anything else is treated as a POSIX shell with `-c`.
    fn named(program: &str) -> Self {
        let base = std::path::Path::new(program)
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or(program)
            .to_ascii_lowercase();
        let args = match base.as_str() {
            "powershell" | "pwsh" => vec!["-NoProfile", "-NonInteractive", "-Command"],
            "cmd" => vec!["/C"],
            _ => vec!["-c"],
        };
        Self {
            program: program.to_owned(),
            args: args.into_iter().map(str::to_owned).collect(),
        }
    }

    /// The executable.
    ///
    /// # Returns
    ///
    /// Its name or path.
    pub fn program(&self) -> &str {
        &self.program
    }

    /// Prepares a command that runs `script`.
    fn command(&self, script: &str) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(&self.program);
        command.args(&self.args).arg(script);
        command
    }
}

/// Arguments of the `shell` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ShellArgs {
    /// The command line to run.
    pub command: String,
    /// Seconds before the command is killed. Default 120, maximum 600.
    #[serde(default)]
    pub timeout_s: Option<u64>,
}

/// Runs a command in the configured shell.
pub struct ShellTool;

impl Tool for ShellTool {
    fn name(&self) -> &'static str {
        "shell"
    }

    fn description(&self) -> &'static str {
        "Runs a command in the project shell (PowerShell on Windows, sh elsewhere) from the \
         project root and returns its combined stdout and stderr followed by the exit code. \
         Output is capped at 30 KB; the full output is kept in the session folder."
    }

    fn schema(&self) -> Value {
        schema_for::<ShellArgs>()
    }

    fn access(&self) -> Access {
        Access::Write
    }

    fn blocked_reason(&self, args: &Value) -> Option<String> {
        let command = args
            .get("command")
            .and_then(Value::as_str)?
            .to_ascii_lowercase();
        BLOCKED_COMMANDS
            .iter()
            .find(|blocked| command.contains(&blocked.to_ascii_lowercase()))
            .map(|blocked| format!("command contains `{blocked}`"))
    }

    fn run<'a>(&'a self, args: Value, ctx: &'a ToolContext) -> ToolFuture<'a> {
        Box::pin(async move {
            let args: ShellArgs = match parse_args(args) {
                Ok(args) => args,
                Err(output) => return output,
            };
            let timeout = Duration::from_secs(
                args.timeout_s
                    .unwrap_or(DEFAULT_TIMEOUT_S)
                    .clamp(1, MAX_TIMEOUT_S),
            );
            match run_command(ctx, &args.command, timeout).await {
                Ok(run) => run.into_output(),
                Err(error) => {
                    ToolOutput::error(format!("cannot start `{}`: {error}", ctx.shell.program()))
                }
            }
        })
    }
}

/// What a finished command produced.
struct CommandRun {
    /// Combined stdout and stderr, possibly cut at [`MEMORY_CAP`].
    output: Vec<u8>,
    /// Whether output beyond [`MEMORY_CAP`] was discarded.
    overflowed: bool,
    /// The exit code, or `None` when killed by the timeout or a signal.
    exit_code: Option<i32>,
    /// Whether the timeout fired.
    timed_out: bool,
    /// The timeout that applied.
    timeout: Duration,
}

impl CommandRun {
    /// Renders the run for the model.
    fn into_output(self) -> ToolOutput {
        let mut text = String::from_utf8_lossy(&self.output).into_owned();
        if text.is_empty() {
            text.push_str("(no output)");
        }
        if self.overflowed {
            text.push_str("\n[output beyond 1 MB was discarded]");
        }
        let failed = self.timed_out || self.exit_code != Some(0);
        if self.timed_out {
            text.push_str(&format!(
                "\n[timed out after {}s; process killed]",
                self.timeout.as_secs()
            ));
        } else {
            match self.exit_code {
                Some(code) => text.push_str(&format!("\n[exit code: {code}]")),
                None => text.push_str("\n[terminated by signal]"),
            }
        }
        ToolOutput {
            content: text,
            is_error: failed,
        }
    }
}

/// Spawns the command and streams its output until it exits or times out.
async fn run_command(
    ctx: &ToolContext,
    script: &str,
    timeout: Duration,
) -> std::io::Result<CommandRun> {
    let mut child = ctx
        .shell
        .command(script)
        .current_dir(&ctx.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(32);
    if let Some(stdout) = child.stdout.take() {
        tokio::spawn(pump(stdout, tx.clone()));
    }
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(pump(stderr, tx.clone()));
    }
    drop(tx);

    let deadline = tokio::time::Instant::now() + timeout;
    let mut run = CommandRun {
        output: Vec::new(),
        overflowed: false,
        exit_code: None,
        timed_out: false,
        timeout,
    };
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Some(chunk)) => {
                let room = MEMORY_CAP.saturating_sub(run.output.len());
                if chunk.len() > room {
                    run.overflowed = true;
                }
                run.output
                    .extend_from_slice(&chunk[..chunk.len().min(room)]);
            }
            Ok(None) => break,
            Err(_elapsed) => {
                run.timed_out = true;
                break;
            }
        }
    }
    if run.timed_out {
        let _ = child.kill().await;
        return Ok(run);
    }
    match tokio::time::timeout_at(deadline, child.wait()).await {
        Ok(Ok(status)) => run.exit_code = status.code(),
        Ok(Err(_)) => run.exit_code = None,
        Err(_elapsed) => {
            run.timed_out = true;
            let _ = child.kill().await;
        }
    }
    Ok(run)
}

/// Forwards chunks from a child pipe to the collector until EOF.
async fn pump(mut reader: impl AsyncReadExt + Unpin, tx: mpsc::Sender<Vec<u8>>) {
    let mut buffer = vec![0u8; 8192];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                if tx.send(buffer[..n].to_vec()).await.is_err() {
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn ctx() -> ToolContext {
        ToolContext::new(std::env::temp_dir())
    }

    /// A command that prints `lines` numbered lines on the platform shell.
    fn many_lines(lines: usize) -> String {
        if cfg!(windows) {
            format!("1..{lines} | ForEach-Object {{ \"line $_ padding padding padding\" }}")
        } else {
            format!(
                "i=1; while [ $i -le {lines} ]; do echo \"line $i padding padding padding\"; i=$((i+1)); done"
            )
        }
    }

    #[test]
    fn schema_snapshot() {
        insta::assert_json_snapshot!(ShellTool.schema());
    }

    #[test]
    fn shell_programs_get_the_right_flags() {
        assert_eq!(
            Shell::named("pwsh").args,
            vec!["-NoProfile", "-NonInteractive", "-Command"]
        );
        assert_eq!(Shell::named("C:/Windows/System32/cmd.exe").args, vec!["/C"]);
        assert_eq!(Shell::named("bash").args, vec!["-c"]);
        assert_eq!(Shell::from_config(Some(" zsh ")).program(), "zsh");
        assert_eq!(Shell::from_config(Some("")), Shell::for_platform());
        assert_eq!(Shell::from_config(None), Shell::for_platform());
    }

    #[test]
    fn blocked_commands_are_detected_case_insensitively() {
        let tool = ShellTool;
        assert!(
            tool.blocked_reason(&json!({"command": "rm -rf /"}))
                .is_some()
        );
        assert!(
            tool.blocked_reason(&json!({"command": "remove-item -recurse x"}))
                .is_some()
        );
        assert!(
            tool.blocked_reason(&json!({"command": "git push --force-with-lease"}))
                .is_some()
        );
        assert!(
            tool.blocked_reason(&json!({"command": "git reset --hard HEAD~1"}))
                .is_some()
        );
        assert_eq!(tool.blocked_reason(&json!({"command": "git status"})), None);
        assert_eq!(
            tool.target(&json!({"command": "ls"})).as_deref(),
            Some("ls")
        );
    }

    #[tokio::test]
    async fn runs_a_command_and_records_exit_code() {
        let output = ShellTool
            .run(json!({"command": "echo hello"}), &ctx())
            .await;
        assert!(!output.is_error, "{}", output.content);
        assert!(output.content.starts_with("hello"), "{}", output.content);
        assert!(output.content.ends_with("[exit code: 0]"));

        let output = ShellTool.run(json!({"command": "exit 3"}), &ctx()).await;
        assert!(output.is_error);
        assert!(
            output.content.ends_with("[exit code: 3]"),
            "{}",
            output.content
        );
    }

    #[tokio::test]
    async fn captures_stderr_and_no_output() {
        let command = if cfg!(windows) {
            "[Console]::Error.WriteLine('oops')"
        } else {
            "echo oops 1>&2"
        };
        let output = ShellTool.run(json!({"command": command}), &ctx()).await;
        assert!(output.content.contains("oops"), "{}", output.content);

        let output = ShellTool.run(json!({"command": "exit 0"}), &ctx()).await;
        assert!(
            output.content.starts_with("(no output)"),
            "{}",
            output.content
        );
    }

    #[tokio::test]
    async fn times_out_and_kills() {
        let command = if cfg!(windows) {
            "Start-Sleep -Seconds 30"
        } else {
            "sleep 30"
        };
        let started = std::time::Instant::now();
        let output = ShellTool
            .run(json!({"command": command, "timeout_s": 1}), &ctx())
            .await;
        assert!(output.is_error);
        assert!(
            output.content.contains("timed out after 1s"),
            "{}",
            output.content
        );
        assert!(started.elapsed() < Duration::from_secs(20));
    }

    #[tokio::test]
    async fn long_output_is_fully_collected_below_the_memory_cap() {
        let output = ShellTool
            .run(json!({"command": many_lines(3000)}), &ctx())
            .await;
        assert!(!output.is_error, "{}", output.content);
        assert!(output.content.len() > super::super::DEFAULT_OUTPUT_CAP);
        assert!(output.content.contains("line 3000 "));
        assert!(!output.content.contains("discarded"));
    }

    #[tokio::test]
    async fn registry_truncates_shell_output_at_30kb() {
        use crate::provider::types::ToolCall;
        use crate::tools::{Permissions, PromptAnswer, StaticPrompt, ToolOutcome, ToolRegistry};
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = ToolRegistry::with_builtins(
            Permissions::from_toml("allow = [\"shell\"]", None).expect("rules"),
            Box::new(StaticPrompt(PromptAnswer::No)),
        );
        let ctx = ToolContext {
            session_dir: Some(dir.path().to_path_buf()),
            ..ctx()
        };
        let call = ToolCall {
            id: "sh1".to_owned(),
            name: "shell".to_owned(),
            arguments_json: json!({"command": many_lines(3000)}).to_string(),
        };
        let execution = registry.execute(&call, &ctx).await;
        assert_eq!(execution.outcome, ToolOutcome::Completed);
        assert!(execution.result.content.len() < super::super::DEFAULT_OUTPUT_CAP + 200);
        assert!(
            execution
                .result
                .content
                .contains("[output truncated: 30720 of")
        );
        let full = execution.full_output.expect("full output kept");
        assert!(
            std::fs::read_to_string(full)
                .expect("file")
                .contains("line 3000 ")
        );
    }
}
