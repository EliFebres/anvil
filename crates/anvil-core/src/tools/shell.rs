//! The `shell` tool, the built-in deny categories, and the one function
//! that starts every command anvil runs for the model or a check:
//! [`run_command`], which clears the environment and starts the command
//! through the shell, directly as an argument vector (allowlist mode), or
//! in a container.
//!
//! Matching command text, as the deny categories do, is a guard rail and
//! not a security boundary: a shell can hide a command from any text
//! match. The boundaries are allowlist mode, where no shell interprets the
//! command, and the container executor.

use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anvil_types::child_env;
use anvil_types::shell::{ContainerSettings, program_name, split_command};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;

use super::{Access, Blocked, Tool, ToolContext, ToolFuture, ToolOutput, parse_args, schema_for};

/// Timeout when the model does not set one.
pub const DEFAULT_TIMEOUT_S: u64 = 120;

/// The name the `shell` tool is registered and matched under.
pub const SHELL_TOOL_NAME: &str = "shell";

/// Longest timeout the model may request.
pub const MAX_TIMEOUT_S: u64 = 600;

/// Bytes of output kept in memory; the registry caps what the model sees.
const MEMORY_CAP: usize = 1024 * 1024;

/// Destructive substrings that make a command refuse to run unless a rule
/// names them, matched without regard to case.
pub const BLOCKED_COMMANDS: [&str; 4] = [
    "rm -rf",
    "Remove-Item -Recurse",
    "git push --force",
    "git reset --hard",
];

/// Programs that reach the network: the network clients category.
pub const NETWORK_CLIENTS: [&str; 15] = [
    "curl",
    "wget",
    "invoke-webrequest",
    "iwr",
    "invoke-restmethod",
    "irm",
    "nc",
    "ncat",
    "netcat",
    "ssh",
    "scp",
    "sftp",
    "ftp",
    "start-bitstransfer",
    "bitsadmin",
];

/// Programs that raise privileges: the privilege tools category.
pub const PRIVILEGE_TOOLS: [&str; 3] = ["sudo", "runas", "doas"];

/// Package managers and the subcommands that install packages, which runs
/// code from the network: the package installs category.
const PACKAGE_INSTALLS: [(&str, &[&str]); 5] = [
    ("pip", &["install"]),
    ("pip3", &["install"]),
    ("npm", &["install", "i", "add", "ci"]),
    ("cargo", &["install"]),
    ("gem", &["install"]),
];

/// git options before the subcommand that take the next word as their
/// value, besides `-c` and `--config-env`, which are read for aliases.
const GIT_OPTIONS_WITH_VALUE: [&str; 5] = [
    "--git-dir",
    "--work-tree",
    "--namespace",
    "--super-prefix",
    "--attr-source",
];

/// git options an allowlisted git may take before its subcommand; every
/// other one (`-c`, `-C`, `--git-dir`, `--exec-path` and the rest) can
/// change git's config, repository or programs.
const GIT_SAFE_GLOBALS: [&str; 7] = [
    "--no-pager",
    "-P",
    "--no-optional-locks",
    "--literal-pathspecs",
    "--no-replace-objects",
    "--version",
    "--help",
];

/// Long git options an allowlisted git refuses after its subcommand: they
/// write a file (`--output`), read outside the repository (`--no-index`)
/// or run another program. An abbreviation of three letters or more is
/// refused too, because git accepts unambiguous abbreviations.
const GIT_REFUSED_OPTIONS: [&str; 5] = [
    "output",
    "no-index",
    "ext-diff",
    "textconv",
    "open-files-in-pager",
];

/// Real git options that are also abbreviations of a refused one, so the
/// exact word means the real option.
const GIT_OPTIONS_NOT_ABBREVIATIONS: [&str; 1] = ["text"];

/// Characters that end a word when looking for denied programs: whitespace
/// and every shell metacharacter, so `x;curl y` and `$(curl y)` are found.
const WORD_BREAKS: &str = ";&|<>()`{}\"'$";

/// Which built-in deny category a command falls in, if any.
///
/// The categories apply in every shell mode: network clients, `git push`,
/// `git remote add` and `set-url`, `git clone`, a git alias defined on the
/// command line (`git -c alias.p=push p`), package installs (`pip
/// install`, `npm install`, `cargo install`, `gem install`), privilege
/// tools (`sudo`, `runas`, `doas`) and the destructive substrings of
/// [`BLOCKED_COMMANDS`]. The command is split into words at whitespace and
/// at every shell metacharacter; each word is compared by its program
/// name, without folder or extension. This is a text match: a guard rail
/// that a shell can get around, not a boundary.
///
/// # Arguments
///
/// * `command` - The command line.
///
/// # Returns
///
/// The matched phrase (such as `git push` or `curl`) and the category's
/// name, or `None`.
pub fn deny_category(command: &str) -> Option<(String, &'static str)> {
    let lower = command.to_ascii_lowercase();
    if let Some(blocked) = BLOCKED_COMMANDS
        .iter()
        .find(|blocked| lower.contains(&blocked.to_ascii_lowercase()))
    {
        return Some(((*blocked).to_owned(), "destructive commands"));
    }
    let words: Vec<String> = lower
        .split(|c: char| c.is_whitespace() || WORD_BREAKS.contains(c))
        .filter(|word| !word.is_empty())
        .map(program_name)
        .collect();
    for (index, word) in words.iter().enumerate() {
        if NETWORK_CLIENTS.contains(&word.as_str()) {
            return Some((word.clone(), "network clients"));
        }
        if PRIVILEGE_TOOLS.contains(&word.as_str()) {
            return Some((word.clone(), "privilege tools"));
        }
        let rest = &words[index + 1..];
        if word == "git"
            && let Some(found) = git_denied(rest)
        {
            return Some(found);
        }
        if let Some((manager, verbs)) = PACKAGE_INSTALLS.iter().find(|(name, _)| name == word)
            && rest
                .iter()
                .find(|arg| !arg.starts_with('-'))
                .is_some_and(|verb| verbs.contains(&verb.as_str()))
        {
            return Some((format!("{manager} install"), "package installs"));
        }
    }
    None
}

/// The denied phrase and category among the lowercased words after
/// `git`, if any: an alias defined with `-c` or `--config-env` (it can
/// name any subcommand or program), or a subcommand that reaches or
/// changes remotes.
fn git_denied(rest: &[String]) -> Option<(String, &'static str)> {
    const REMOTES: &str = "git commands that reach or change remotes";
    let mut words = rest.iter();
    let subcommand = loop {
        let word = words.next()?;
        // `-C` arrives lowercased as `-c`; reading its folder as a config
        // value only errs towards refusing.
        let config = match word.as_str() {
            "-c" | "--config-env" => Some(words.next()?.as_str()),
            other => other
                .strip_prefix("--config-env=")
                .or_else(|| other.strip_prefix("-c").filter(|value| !value.is_empty())),
        };
        if let Some(value) = config {
            if value.starts_with("alias.") {
                return Some(("git -c alias".to_owned(), "git alias definition"));
            }
        } else if GIT_OPTIONS_WITH_VALUE.contains(&word.as_str()) {
            words.next();
        } else if !word.starts_with('-') {
            break word;
        }
    };
    match subcommand.as_str() {
        "push" | "clone" => Some((format!("git {subcommand}"), REMOTES)),
        "remote" => {
            let action = words.find(|word| !word.starts_with('-'))?;
            matches!(action.as_str(), "add" | "set-url")
                .then(|| (format!("git remote {action}"), REMOTES))
        }
        _ => None,
    }
}

/// Why an allowlisted command's arguments are refused, if they are.
///
/// Allowlist patterns end in `*`, so the arguments after the matched words
/// are the model's choice; these checks keep an allowed program from being
/// steered:
///
/// - No argument, value after `=` in an argument, or attached value of a
///   short option (`-I/usr/include`) may name a path outside `root`: one
///   that starts with `~`, or that is absolute, rooted, a drive or UNC
///   path or climbs with `..` and resolves outside it.
/// - For git, no option may come before the subcommand except those in
///   [`GIT_SAFE_GLOBALS`], and none of [`GIT_REFUSED_OPTIONS`] (or an
///   abbreviation) nor `-o` or `-O` may follow it.
///
/// # Arguments
///
/// * `root` - The project root, canonical.
/// * `argv` - The split command, program first.
///
/// # Returns
///
/// The reason, or `None` when the arguments may run.
pub fn argument_refusal(root: &Path, argv: &[String]) -> Option<String> {
    let (program, args) = argv.split_first()?;
    if program_name(program) == "git"
        && let Some(reason) = git_argument_refusal(args)
    {
        return Some(reason);
    }
    args.iter().find_map(|arg| {
        outside_path(root, arg).map(|path| {
            format!(
                "`{arg}` names `{path}`, outside the workspace; under a shell allowlist an argument may not name a path outside it"
            )
        })
    })
}

/// Why git's arguments (after `git`) are refused under an allowlist.
fn git_argument_refusal(args: &[String]) -> Option<String> {
    let mut words = args.iter();
    for word in words.by_ref() {
        if !word.starts_with('-') {
            break;
        }
        if !GIT_SAFE_GLOBALS.contains(&word.as_str()) {
            return Some(format!(
                "git option `{word}` before the subcommand is refused under a shell allowlist: it can change git's config, repository or programs"
            ));
        }
    }
    for word in words {
        let refused = match word.strip_prefix("--") {
            Some(option) => {
                let name = option.split('=').next().unwrap_or_default();
                GIT_REFUSED_OPTIONS.iter().any(|refused| {
                    *refused == name
                        || (name.len() >= 3
                            && refused.starts_with(name)
                            && !GIT_OPTIONS_NOT_ABBREVIATIONS.contains(&name))
                })
            }
            None => word.starts_with("-o") || word.starts_with("-O"),
        };
        if refused {
            return Some(format!(
                "git option `{word}` is refused under a shell allowlist: it writes a file, reads outside the repository or runs another program"
            ));
        }
    }
    None
}

/// The part of `arg` that names a path outside `root`, if any: the whole
/// word, the value after its first `=`, or, for a short option, any text
/// after its first letter.
fn outside_path(root: &Path, arg: &str) -> Option<String> {
    let mut candidates = vec![arg];
    if let Some((_, value)) = arg.split_once('=') {
        candidates.push(value);
    }
    if arg.starts_with('-') && !arg.starts_with("--") {
        candidates.extend(
            (2..arg.len())
                .filter(|index| arg.is_char_boundary(*index))
                .map(|index| &arg[index..]),
        );
    }
    candidates
        .into_iter()
        .find(|candidate| !stays_inside(root, candidate))
        .map(str::to_owned)
}

/// Whether `candidate`, read as a path, stays inside `root`. A word that
/// is neither rooted nor climbs with `..` is relative to the project and
/// stays inside; one that starts with `~` names the home folder, which a
/// program may expand itself.
fn stays_inside(root: &Path, candidate: &str) -> bool {
    if candidate.starts_with('~') {
        return false;
    }
    let path = Path::new(candidate);
    let bytes = candidate.as_bytes();
    let drive = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    let rooted = drive || path.has_root() || candidate.starts_with(['/', '\\']);
    let climbs = path
        .components()
        .any(|component| component == std::path::Component::ParentDir);
    if !rooted && !climbs {
        return true;
    }
    super::permissions::resolve_path(root, candidate)
        .is_some_and(|resolved| super::workspace::is_within(root, &resolved))
}

/// How commands start: through the shell or directly, on the host or in a
/// container.
#[derive(Debug, Clone, Default)]
pub struct Runner {
    /// Split the command with the conservative parser and start the program
    /// directly, with no shell interpreter (allowlist mode).
    pub direct: bool,
    /// Run commands in a container instead of on the host.
    pub container: Option<Container>,
}

/// The container executor: the runtime program and its settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Container {
    /// The program that starts containers: `docker` or `podman`, or a
    /// stand-in in tests.
    pub program: String,
    /// The image and limits.
    pub settings: ContainerSettings,
}

impl Container {
    /// The executor for `settings`, started through its runtime.
    ///
    /// # Arguments
    ///
    /// * `settings` - The `[shell.container]` table.
    ///
    /// # Returns
    ///
    /// The executor.
    pub fn new(settings: ContainerSettings) -> Self {
        Self {
            program: settings.runtime.program().to_owned(),
            settings,
        }
    }
}

/// Where the workspace is mounted inside a container.
pub const CONTAINER_WORKSPACE: &str = "/workspace";

/// The arguments that run `command` in a container: no network, the
/// workspace as the only folder from the host (read-write, at
/// [`CONTAINER_WORKSPACE`], with its `.git` mounted again read-only when
/// it exists), a read-only root file system with an
/// in-memory `/tmp`, the configured non-root user, every capability
/// dropped, no privilege gain, and the process and memory limits. No
/// variable of the host reaches the container; it gets `HOME=/tmp` and
/// `PYTHONDONTWRITEBYTECODE=1`.
///
/// # Arguments
///
/// * `settings` - The image and limits.
/// * `name` - The container name, so a timed-out run can be killed.
/// * `workspace` - The project root on the host.
/// * `command` - What runs inside: the program and its arguments.
///
/// # Returns
///
/// The arguments for the runtime program, `run` first.
///
/// # Errors
///
/// When the workspace path holds a `,`, which would end the mount
/// specification early.
pub fn container_args(
    settings: &ContainerSettings,
    name: &str,
    workspace: &Path,
    command: &[String],
) -> Result<Vec<String>, String> {
    let shown = workspace.display().to_string();
    let source = shown.strip_prefix(r"\\?\").unwrap_or(&shown).to_owned();
    if source.contains(',') {
        return Err(format!(
            "the workspace path {source} holds a comma, which a container mount cannot name"
        ));
    }
    let pids = settings.pids_limit.to_string();
    let mount = format!("type=bind,source={source},target={CONTAINER_WORKSPACE}");
    // A later mount covers an earlier one, so the git folder stays
    // read-only: a command cannot plant a hook or a config program that
    // the user's own git would run later on the host.
    let git_mount = workspace.join(".git").exists().then(|| {
        let git_dir = Path::new(&source).join(".git");
        format!(
            "type=bind,source={},target={CONTAINER_WORKSPACE}/.git,readonly",
            git_dir.display()
        )
    });
    let bytecode = format!("{NO_BYTECODE}=1");
    let mut args: Vec<String> = [
        "run",
        "--rm",
        // The engine would otherwise fetch a missing image from a registry,
        // a destination no anvil rule covers; administrators pre-load it.
        "--pull=never",
        "--name",
        name,
        "--network",
        "none",
        "--read-only",
        "--tmpfs",
        "/tmp",
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges",
        "--user",
        &settings.user,
        "--pids-limit",
        &pids,
        "--memory",
        &settings.memory,
        "--mount",
        &mount,
    ]
    .iter()
    .map(|arg| (*arg).to_owned())
    .collect();
    if let Some(git_mount) = git_mount {
        args.extend(["--mount".to_owned(), git_mount]);
    }
    args.extend(
        [
            "--workdir",
            CONTAINER_WORKSPACE,
            "--env",
            "HOME=/tmp",
            "--env",
            &bytecode,
            &settings.image,
        ]
        .iter()
        .map(|arg| (*arg).to_owned()),
    );
    args.extend(command.iter().cloned());
    Ok(args)
}

/// A name for one container run, unique within this process.
fn container_name() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "anvil-{}-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_millis(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

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

    /// Prepares a command that runs `script` in this shell.
    ///
    /// Nothing is spawned; the caller sets the working directory and the
    /// standard streams. The environment starts cleared and refilled from
    /// [`child_env::vars`], so no caller can leave it out.
    ///
    /// # Arguments
    ///
    /// * `script` - The command line.
    ///
    /// # Returns
    ///
    /// The command, ready to configure.
    pub fn command(&self, script: &str) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(&self.program);
        command
            .args(&self.args)
            .arg(script)
            .env_clear()
            .envs(child_env::vars());
        command
    }
}

/// The variable that stops Python from writing `__pycache__` folders.
const NO_BYTECODE: &str = "PYTHONDONTWRITEBYTECODE";

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
    fn name(&self) -> &str {
        SHELL_TOOL_NAME
    }

    fn description(&self) -> &str {
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

    fn blocked_reason(&self, args: &Value) -> Option<Blocked> {
        let command = args.get("command").and_then(Value::as_str)?;
        let (phrase, category) = deny_category(command)?;
        Some(Blocked {
            reason: format!("command contains `{phrase}` ({category})"),
            phrase,
        })
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
#[derive(Debug)]
pub struct CommandRun {
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
    /// Whether the command exited with code zero before the timeout.
    ///
    /// # Returns
    ///
    /// `true` on success.
    pub fn succeeded(&self) -> bool {
        !self.timed_out && self.exit_code == Some(0)
    }

    /// The combined output as text.
    ///
    /// # Returns
    ///
    /// Lossy UTF-8 of stdout and stderr.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.output).into_owned()
    }

    /// Renders the run for the model.
    ///
    /// # Returns
    ///
    /// The output with the exit code or timeout note appended; an error when
    /// the command failed.
    pub fn into_output(self) -> ToolOutput {
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
            model_turns: Vec::new(),
        }
    }
}

/// The command that starts `script` as the context's runner says, with a
/// cleared environment, before its working folder and streams are set.
///
/// # Errors
///
/// When the command does not split in direct mode, or the container
/// cannot name the workspace.
fn prepare(
    ctx: &ToolContext,
    script: &str,
    container_name: &str,
) -> std::io::Result<tokio::process::Command> {
    let invalid = |reason: String| std::io::Error::new(std::io::ErrorKind::InvalidInput, reason);
    let argv = if ctx.runner.direct {
        Some(split_command(script).map_err(invalid)?)
    } else {
        None
    };
    if let Some(argv) = &argv {
        let root = super::workspace::canonical(&ctx.cwd);
        if let Some(reason) = argument_refusal(&root, argv) {
            return Err(invalid(reason));
        }
    }
    let git = |argv: &[String]| program_name(&argv[0]) == "git";
    let mut command = match (&ctx.runner.container, argv) {
        (Some(container), argv) => {
            let inside = match argv {
                Some(argv) if git(&argv) => std::iter::once("git".to_owned())
                    .chain(crate::git::SAFE_ARGS.iter().map(|arg| (*arg).to_owned()))
                    .chain(crate::git::allowlisted_args(&argv[1..]))
                    .collect(),
                Some(argv) => argv,
                None => vec!["sh".to_owned(), "-c".to_owned(), script.to_owned()],
            };
            let args = container_args(&container.settings, container_name, &ctx.cwd, &inside)
                .map_err(invalid)?;
            let mut command = tokio::process::Command::new(host_program(&container.program));
            command.args(args).env_clear().envs(child_env::vars());
            command
        }
        // Allowlisted git gets the safe settings and checks of anvil's own
        // git calls, with the identity variables they pass.
        (None, Some(argv)) if git(&argv) => tokio::process::Command::from(
            crate::git::allowlisted(&ctx.cwd, &argv[1..])
                .map_err(|error| invalid(error.to_string()))?,
        ),
        (None, Some(argv)) => {
            let mut command = tokio::process::Command::new(host_program(&argv[0]));
            command.args(&argv[1..]).env_clear().envs(child_env::vars());
            command
        }
        (None, None) => ctx.shell.command(script),
    };
    // Bytecode caches written by a test run would land in the session's diff,
    // where the reviewer reads them as unrelated changes.
    command.env(NO_BYTECODE, "1");
    Ok(command)
}

/// The program to start for `name`: on Windows the file found on the
/// `PATH` children receive, trying each `PATHEXT` extension, because the
/// system only adds `.exe` itself and tools such as `npm` are `.cmd`
/// files; elsewhere, and for a path, the name as given, which the system
/// looks up on that `PATH`.
fn host_program(name: &str) -> std::ffi::OsString {
    if cfg!(windows) && !name.contains(['/', '\\']) {
        let vars = child_env::vars();
        let value = |key: &str| {
            vars.iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(key))
                .map(|(_, value)| value.clone())
        };
        let extensions = value("PATHEXT")
            .map(|text| text.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_else(|| ".com;.exe;.bat;.cmd".to_owned());
        let has_extension = Path::new(name).extension().is_some();
        let dirs: Vec<std::path::PathBuf> = value("PATH")
            .map(|path| std::env::split_paths(&path).collect())
            .unwrap_or_default();
        for dir in dirs {
            if has_extension && dir.join(name).is_file() {
                return dir.join(name).into_os_string();
            }
            for extension in extensions.split(';').filter(|ext| !ext.is_empty()) {
                let candidate = dir.join(format!("{name}{extension}"));
                if candidate.is_file() {
                    return candidate.into_os_string();
                }
            }
        }
    }
    name.into()
}

/// Starts a command as the context's runner says and streams its output
/// until it exits or times out.
///
/// Every command anvil runs for the model, a check or an acceptance
/// command goes through here. The environment is cleared and refilled from
/// [`child_env::vars`]. With [`Runner::direct`] the command is split by the
/// conservative parser and its program started with no shell; otherwise it
/// runs in the context's shell. With [`Runner::container`] it runs inside a
/// container built by [`container_args`] instead, and a timed-out container
/// is killed by name.
///
/// # Arguments
///
/// * `ctx` - Supplies the runner, the shell and the working directory.
/// * `script` - The command line.
/// * `timeout` - When to kill the process.
///
/// # Returns
///
/// The run.
///
/// # Errors
///
/// When the command cannot be prepared (it does not split in direct mode)
/// or the program cannot be spawned.
pub async fn run_command(
    ctx: &ToolContext,
    script: &str,
    timeout: Duration,
) -> std::io::Result<CommandRun> {
    let name = container_name();
    let mut command = prepare(ctx, script, &name)?;
    let mut child = command
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
    if !run.timed_out {
        match tokio::time::timeout_at(deadline, child.wait()).await {
            Ok(Ok(status)) => run.exit_code = status.code(),
            Ok(Err(_)) => run.exit_code = None,
            Err(_elapsed) => run.timed_out = true,
        }
    }
    if run.timed_out {
        let _ = child.kill().await;
        if let Some(container) = &ctx.runner.container {
            kill_container(container, &name).await;
        }
    }
    Ok(run)
}

/// Stops a container whose runtime client was killed, because the client
/// going away does not stop the container itself.
async fn kill_container(container: &Container, name: &str) {
    let result = tokio::process::Command::new(host_program(&container.program))
        .args(["kill", name])
        .env_clear()
        .envs(child_env::vars())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
    if let Err(error) = result {
        tracing::warn!(%error, name, "could not kill the timed-out container");
    }
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
    use crate::tools::ToolOutcome;

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
    async fn commands_run_with_python_bytecode_writing_off() {
        let command = if cfg!(windows) {
            "$env:PYTHONDONTWRITEBYTECODE"
        } else {
            "echo $PYTHONDONTWRITEBYTECODE"
        };
        let run = run_command(&ctx(), command, Duration::from_secs(30))
            .await
            .expect("spawns");
        let output = run.into_output().content;
        assert!(output.starts_with('1'), "{output}");
    }

    #[tokio::test]
    async fn commands_see_only_the_passed_variables() {
        let command = if cfg!(windows) {
            "Get-ChildItem env: | ForEach-Object { $_.Name }"
        } else {
            "env | cut -d= -f1"
        };
        let run = run_command(&ctx(), command, Duration::from_secs(30))
            .await
            .expect("spawns");
        let output = run.into_output().content;
        let names: Vec<&str> = output.lines().map(str::trim).collect();
        assert!(
            names.iter().any(|name| name.eq_ignore_ascii_case("PATH")),
            "{output}"
        );
        for name in &names {
            assert!(
                !child_env::looks_secret(name),
                "{name} reached the command:\n{output}"
            );
        }
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

    /// A registry that allows every `shell` call and answers every prompt
    /// with yes: everything a rule, `--yes` or a person could grant.
    fn shell_registry(rules: &str) -> crate::tools::ToolRegistry {
        use crate::tools::{Permissions, PromptAnswer, StaticPrompt, ToolRegistry};
        ToolRegistry::with_builtins(
            Permissions::from_toml(rules, None).expect("rules"),
            Box::new(StaticPrompt(PromptAnswer::Yes)),
        )
    }

    async fn shell_outcome(
        registry: &crate::tools::ToolRegistry,
        ctx: &ToolContext,
        command: &str,
    ) -> crate::tools::Execution {
        let call = crate::provider::types::ToolCall {
            id: "c1".to_owned(),
            name: SHELL_TOOL_NAME.to_owned(),
            arguments_json: json!({ "command": command }).to_string(),
        };
        registry.execute(&call, ctx).await
    }

    /// One command per deny category, with the phrase each one names.
    const DENIED: [(&str, &str); 22] = [
        ("curl https://example.com", "curl"),
        ("wget -q https://example.com", "wget"),
        ("Invoke-WebRequest https://example.com", "invoke-webrequest"),
        ("iwr https://example.com", "iwr"),
        ("Invoke-RestMethod https://example.com", "invoke-restmethod"),
        ("irm https://example.com", "irm"),
        ("nc -l 4444", "nc"),
        ("ssh host", "ssh"),
        ("scp a host:b", "scp"),
        ("sftp host", "sftp"),
        ("ftp host", "ftp"),
        ("git push origin main", "git push"),
        (
            "git remote add evil https://example.com/r.git",
            "git remote add",
        ),
        (
            "git remote set-url origin https://example.com/r.git",
            "git remote set-url",
        ),
        ("git clone https://example.com/r.git", "git clone"),
        ("pip install requests", "pip install"),
        ("npm install left-pad", "npm install"),
        ("cargo install ripgrep", "cargo install"),
        ("sudo ls", "sudo"),
        ("runas /user:admin cmd", "runas"),
        ("rm -rf build", "rm -rf"),
        ("git reset --hard HEAD~1", "git reset --hard"),
    ];

    #[test]
    fn each_deny_category_is_recognised_and_hidden_words_are_found() {
        for (command, phrase) in DENIED {
            let (found, _) = deny_category(command).unwrap_or_else(|| panic!("{command}"));
            assert_eq!(found, phrase, "{command}");
        }
        for (command, phrase) in [
            ("echo x;curl y", "curl"),
            ("echo $(wget y)", "wget"),
            ("x=`curl y`", "curl"),
            (r"C:\tools\curl.exe -s y", "curl"),
            ("/usr/bin/ssh host", "ssh"),
            ("git -C repo push", "git push"),
            ("git -c a.b=c push", "git push"),
            ("python -m pip install x", "pip install"),
            ("npm i x", "npm install"),
            ("Remove-Item -Recurse x", "Remove-Item -Recurse"),
            ("git push --force", "git push --force"),
        ] {
            let (found, _) = deny_category(command).unwrap_or_else(|| panic!("{command}"));
            assert_eq!(found, phrase, "{command}");
        }
        for command in [
            "git status",
            "git remote -v",
            "git log -- docs/ssh.md",
            "cargo test -p anvil-core",
            "npm test",
            "pip list",
            "cat .ssh-notes",
        ] {
            assert_eq!(deny_category(command), None, "{command}");
        }
    }

    #[test]
    fn a_git_alias_defined_on_the_command_line_is_a_deny_category() {
        for command in [
            "git -c alias.p=push p origin main",
            "git -c alias.x=!curl x https://example.com",
            "git -c ALIAS.x=!sh x",
            "git --config-env=alias.x=E x",
            "git --config-env alias.x=E x",
            "git -C repo -c alias.p=push p",
        ] {
            let (_, category) = deny_category(command).unwrap_or_else(|| panic!("{command}"));
            assert_eq!(category, "git alias definition", "{command}");
        }
        for (command, phrase) in [
            ("git --exec-path=x --namespace ns push", "git push"),
            (
                "git --super-prefix p/ --attr-source HEAD clone x",
                "git clone",
            ),
        ] {
            let (found, _) = deny_category(command).unwrap_or_else(|| panic!("{command}"));
            assert_eq!(found, phrase, "{command}");
        }
        assert_eq!(deny_category("git -c user.name=x commit -m y"), None);
    }

    #[tokio::test]
    async fn each_deny_category_is_refused_under_yes_and_a_broad_allow() {
        let dir = tempfile::tempdir().expect("dir");
        let ctx = ToolContext::new(dir.path());
        for rules in [
            "allow = [\"shell\"]\n",
            "allow = [\"shell:*\"]\n",
            "default = \"allow\"\n",
        ] {
            let registry = shell_registry(rules);
            for (command, phrase) in DENIED {
                let run = shell_outcome(&registry, &ctx, command).await;
                assert_eq!(run.outcome, ToolOutcome::Blocked, "{rules} {command}");
                assert!(
                    run.result.content.contains(phrase),
                    "{command}: {}",
                    run.result.content
                );
            }
        }
    }

    #[tokio::test]
    async fn a_rule_that_names_the_command_lifts_its_deny_category() {
        let registry = shell_registry("allow = [\"shell:git push origin *\"]\n");
        let dir = tempfile::tempdir().expect("dir");
        let ctx = ToolContext::new(dir.path());
        let permit = |command: &str| {
            registry.permit(SHELL_TOOL_NAME, &json!({ "command": command }), dir.path())
        };
        assert_eq!(permit("git push origin main"), Ok(()), "named by the rule");
        let refused = permit("git push upstream main").expect_err("not named");
        assert!(refused.contains("git push"), "{refused}");
        let run = shell_outcome(&registry, &ctx, "curl https://example.com").await;
        assert_eq!(run.outcome, ToolOutcome::Blocked);
    }

    #[tokio::test]
    async fn allowlist_mode_refuses_shell_metacharacters() {
        let mut registry = shell_registry("allow = [\"shell\"]\n");
        registry
            .permissions()
            .limit_shell(&["git *".to_owned()], "the test's [shell] allow")
            .expect("patterns");
        let dir = tempfile::tempdir().expect("dir");
        let ctx = ToolContext::new(dir.path());
        for command in [
            "git status; echo pwned",
            "git status && echo pwned",
            "git status || echo pwned",
            "git status | echo pwned",
            "git status & echo pwned",
            "git status > pwned.txt",
            "git status < input",
            "git `echo status`",
            "git $(echo status)",
            "git ${X}",
            "git {status}",
            "git (status)",
            "git status\necho pwned",
            "git status\r\necho pwned",
            "\"git status",
        ] {
            let run = shell_outcome(&registry, &ctx, command).await;
            assert_eq!(run.outcome, ToolOutcome::Blocked, "{command:?}");
            assert!(!run.result.content.contains("pwned\n"), "{command:?} ran");
        }
        assert!(!dir.path().join("pwned.txt").exists());
        let run = shell_outcome(&registry, &ctx, "cargo build").await;
        assert_eq!(run.outcome, ToolOutcome::Blocked, "not on the allowlist");
        registry.unregister(SHELL_TOOL_NAME);
        let run = shell_outcome(&registry, &ctx, "git status").await;
        assert_eq!(run.outcome, ToolOutcome::UnknownTool, "mode off: no tool");
    }

    /// Whether `git` can be started, for the tests that need a real program.
    fn has_git() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
    }

    #[tokio::test]
    async fn allowlist_mode_starts_the_program_without_a_shell() {
        if !has_git() {
            return;
        }
        let registry = shell_registry("allow = [\"shell\"]\n");
        registry
            .permissions()
            .limit_shell(&["git *".to_owned()], "the test's [shell] allow")
            .expect("patterns");
        let dir = tempfile::tempdir().expect("dir");
        let ctx = ToolContext::new(dir.path());
        let run = shell_outcome(&registry, &ctx, "git rev-parse --sq-quote $HOME").await;
        assert_eq!(
            run.outcome,
            ToolOutcome::Completed,
            "{}",
            run.result.content
        );
        assert!(
            run.result.content.contains("'$HOME'"),
            "no shell expanded the variable: {}",
            run.result.content
        );
        let direct = ToolContext {
            runner: Runner {
                direct: true,
                container: None,
            },
            ..ToolContext::new(dir.path())
        };
        let error = run_command(&direct, "git status; id", Duration::from_secs(30))
            .await
            .expect_err("a direct run refuses metacharacters itself");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }

    /// The variable that makes a re-run of this test binary print what a
    /// child process sees.
    const ENV_PROBE: &str = "ANVIL_TEST_ENV_PROBE";

    /// Variables the outer run sets, with the value each must not leak.
    const PROBE_SECRETS: [(&str, &str); 6] = [
        ("OPENAI_API_KEY", "probe-openai-value"),
        ("ANVIL_ENDPOINT_KEY", "probe-endpoint-value"),
        ("GITHUB_TOKEN", "probe-github-value"),
        ("DB_PASSWORD", "probe-password-value"),
        ("CLIENT_SECRET", "probe-secret-value"),
        ("ANVIL_HOME", "probe-anvil-home-value"),
    ];

    #[tokio::test]
    async fn a_child_process_that_prints_its_environment_shows_no_key() {
        if std::env::var_os(ENV_PROBE).is_some() {
            child_env::set_pass_env(vec!["LISTED_TOKEN".to_owned()]);
            let command = if cfg!(windows) {
                "Get-ChildItem env: | ForEach-Object { \"$($_.Name)=$($_.Value)\" }"
            } else {
                "env"
            };
            let run = run_command(&ctx(), command, Duration::from_secs(30))
                .await
                .expect("spawns");
            let output = run.text();
            for (name, value) in PROBE_SECRETS {
                assert!(
                    !output.contains(value),
                    "{name} reached the child:\n{output}"
                );
            }
            assert!(
                output.contains("probe-listed-value"),
                "a name listed in pass_env is passed:\n{output}"
            );
            return;
        }
        let exe = std::env::current_exe().expect("test binary");
        let mut command = std::process::Command::new(exe);
        command
            .args([
                "--exact",
                "tools::shell::tests::a_child_process_that_prints_its_environment_shows_no_key",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(ENV_PROBE, "1")
            .env("LISTED_TOKEN", "probe-listed-value");
        for (name, value) in PROBE_SECRETS {
            command.env(name, value);
        }
        let output = command.output().expect("re-runs the test binary");
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(output.status.success(), "{text}");
        assert!(text.contains("1 passed"), "the probe ran: {text}");
    }

    fn container_settings() -> ContainerSettings {
        toml::from_str("image = \"registry.example/build@sha256:0123\"\n").expect("settings")
    }

    #[test]
    fn the_workspace_git_folder_is_mounted_read_only_over_the_workspace() {
        let dir = tempfile::tempdir().expect("dir");
        std::fs::create_dir_all(dir.path().join(".git").join("hooks")).expect("mkdir");
        let command = vec!["cargo".to_owned(), "test".to_owned()];
        let args = container_args(&container_settings(), "anvil-1", dir.path(), &command)
            .expect("arguments");
        let mounts: Vec<&String> = args
            .iter()
            .zip(args.iter().skip(1))
            .filter(|(flag, _)| matches!(flag.as_str(), "--mount" | "-v" | "--volume"))
            .map(|(_, value)| value)
            .collect();
        let source = dir.path().display().to_string();
        assert_eq!(
            mounts,
            [
                &format!("type=bind,source={source},target={CONTAINER_WORKSPACE}"),
                &format!(
                    "type=bind,source={},target={CONTAINER_WORKSPACE}/.git,readonly",
                    dir.path().join(".git").display()
                ),
            ],
            "the git folder is mounted read-only after the workspace, so it wins"
        );
    }

    #[test]
    fn the_container_never_pulls_an_image() {
        let workspace = std::path::PathBuf::from("/work/repo");
        let command = vec!["true".to_owned()];
        let args = container_args(&container_settings(), "anvil-1", &workspace, &command)
            .expect("arguments");
        let image = args
            .iter()
            .position(|arg| arg == &container_settings().image)
            .expect("the image is named");
        let pull = args
            .iter()
            .position(|arg| arg == "--pull=never")
            .expect("--pull=never is passed");
        assert!(pull < image, "an option after the image is the command's");
        assert!(
            !args
                .iter()
                .any(|arg| arg.starts_with("--pull") && arg != "--pull=never"),
            "{args:?}"
        );
    }

    #[test]
    fn the_container_arguments_drop_network_root_capabilities_and_host_mounts() {
        let workspace = std::path::PathBuf::from("/work/repo");
        let command = vec!["cargo".to_owned(), "test".to_owned()];
        let args = container_args(&container_settings(), "anvil-1", &workspace, &command)
            .expect("arguments");
        let pairs: Vec<(&str, &str)> = args
            .windows(2)
            .map(|pair| (pair[0].as_str(), pair[1].as_str()))
            .collect();
        for expected in [
            ("--network", "none"),
            ("--cap-drop", "ALL"),
            ("--security-opt", "no-new-privileges"),
            ("--user", "65534:65534"),
            ("--pids-limit", "256"),
            ("--memory", "2g"),
            ("--tmpfs", "/tmp"),
            ("--workdir", CONTAINER_WORKSPACE),
            ("--name", "anvil-1"),
        ] {
            assert!(pairs.contains(&expected), "{expected:?} in {args:?}");
        }
        assert_eq!(args[0], "run");
        assert!(args.contains(&"--rm".to_owned()));
        assert!(args.contains(&"--read-only".to_owned()));
        let mounts: Vec<&String> = args
            .iter()
            .zip(args.iter().skip(1))
            .filter(|(flag, _)| matches!(flag.as_str(), "--mount" | "-v" | "--volume"))
            .map(|(_, value)| value)
            .collect();
        assert_eq!(
            mounts,
            [&format!(
                "type=bind,source={},target={CONTAINER_WORKSPACE}",
                workspace.display()
            )],
            "the workspace is the only host folder"
        );
        assert!(!args.iter().any(|arg| arg.contains("privileged")));
        let env: Vec<&String> = args
            .iter()
            .zip(args.iter().skip(1))
            .filter(|(flag, _)| matches!(flag.as_str(), "--env" | "-e" | "--env-file"))
            .map(|(_, value)| value)
            .collect();
        assert_eq!(
            env,
            ["HOME=/tmp", "PYTHONDONTWRITEBYTECODE=1"],
            "no host variable reaches the container"
        );
        let image_at = args
            .iter()
            .position(|arg| arg == "registry.example/build@sha256:0123")
            .expect("image");
        assert_eq!(
            &args[image_at + 1..],
            ["cargo", "test"],
            "the command follows the image"
        );
        assert!(
            container_args(&container_settings(), "n", Path::new("/work/a,b"), &command).is_err()
        );
        let windows = container_args(
            &container_settings(),
            "n",
            Path::new(r"\\?\D:\work\repo"),
            &command,
        )
        .expect("arguments");
        assert!(
            windows
                .iter()
                .any(|arg| arg.contains(r"source=D:\work\repo,")),
            "{windows:?}"
        );
    }

    /// A program that prints each argument on its own line, standing in for
    /// `docker` or `podman`.
    fn stand_in_runtime(dir: &Path) -> String {
        if cfg!(windows) {
            let path = dir.join("runtime.cmd");
            std::fs::write(
                &path,
                "@echo off\r\n:next\r\nif \"%~1\"==\"\" goto :eof\r\necho(%~1\r\nshift\r\ngoto next\r\n",
            )
            .expect("write");
            path.display().to_string()
        } else {
            let path = dir.join("runtime");
            std::fs::write(
                &path,
                "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done\n",
            )
            .expect("write");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                    .expect("chmod");
            }
            path.display().to_string()
        }
    }

    #[tokio::test]
    async fn commands_run_through_a_stand_in_container_runtime() {
        let tools = tempfile::tempdir().expect("tools");
        let workspace = tempfile::tempdir().expect("workspace");
        let container = Container {
            program: stand_in_runtime(tools.path()),
            settings: container_settings(),
        };
        let mut ctx = ToolContext::new(workspace.path());
        ctx.runner.container = Some(container.clone());
        let run = run_command(&ctx, "cargo test -p x", Duration::from_secs(30))
            .await
            .expect("spawns");
        let text = run.text();
        let lines: Vec<&str> = text.lines().map(str::trim).collect();
        for expected in [
            "run",
            "--network",
            "none",
            "--read-only",
            "--cap-drop",
            "ALL",
            "no-new-privileges",
            "65534:65534",
            "registry.example/build@sha256:0123",
            "sh",
            "-c",
            "cargo test -p x",
        ] {
            assert!(lines.contains(&expected), "{expected} in {lines:?}");
        }
        ctx.runner.direct = true;
        let run = run_command(&ctx, "cargo test -p x", Duration::from_secs(30))
            .await
            .expect("spawns");
        let text = run.text();
        let lines: Vec<&str> = text.lines().map(str::trim).collect();
        let image_at = lines
            .iter()
            .position(|line| *line == "registry.example/build@sha256:0123")
            .expect("image");
        assert_eq!(
            &lines[image_at + 1..],
            ["cargo", "test", "-p", "x"],
            "allowlist mode passes the argument vector, no shell"
        );
    }

    /// The image the real container test uses unless
    /// `ANVIL_CONTAINER_IMAGE` names another.
    #[cfg(target_os = "linux")]
    const TEST_IMAGE: &str = "alpine:3.20";

    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[ignore = "needs docker; CI runs it with ANVIL_CONTAINER_TEST=1 and --ignored"]
    async fn real_container_has_no_network_no_root_and_a_read_only_root() {
        use std::os::unix::fs::PermissionsExt;
        if std::env::var_os("ANVIL_CONTAINER_TEST").is_none() {
            eprintln!("skipped: set ANVIL_CONTAINER_TEST=1 to run against docker");
            return;
        }
        let image =
            std::env::var("ANVIL_CONTAINER_IMAGE").unwrap_or_else(|_| TEST_IMAGE.to_owned());
        let workspace = tempfile::tempdir().expect("workspace");
        std::fs::set_permissions(workspace.path(), std::fs::Permissions::from_mode(0o755))
            .expect("chmod");
        std::fs::write(workspace.path().join("hello.txt"), "hello from the host\n").expect("write");
        let settings: ContainerSettings =
            toml::from_str(&format!("image = \"{image}\"\n")).expect("settings");
        let mut ctx = ToolContext::new(workspace.path());
        ctx.runner.container = Some(Container::new(settings));
        let run = |command: &'static str| {
            let ctx = ctx.clone();
            async move {
                run_command(&ctx, command, Duration::from_secs(120))
                    .await
                    .expect("docker starts")
            }
        };
        let id = run("id -u").await;
        assert!(id.succeeded(), "{}", id.text());
        assert_eq!(id.text().trim(), "65534", "not root");
        let read = run("cat hello.txt").await;
        assert!(
            read.text().contains("hello from the host"),
            "{}",
            read.text()
        );
        let write = run("touch /etc/anvil-probe").await;
        assert!(!write.succeeded(), "the root file system is read-only");
        let network = run("wget -q -T 5 -O /dev/null http://1.1.1.1").await;
        assert!(!network.succeeded(), "no network: {}", network.text());
        let interfaces = run("ls /sys/class/net").await;
        assert_eq!(interfaces.text().trim(), "lo", "only loopback");
        let env = run("env").await;
        assert!(
            !env.text().contains("CARGO_HOME"),
            "no host variable: {}",
            env.text()
        );
    }
}
