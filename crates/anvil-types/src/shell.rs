//! The `[shell]` table: how the `shell` tool runs commands, shared by the
//! user's configuration and the managed policy, and the conservative
//! command parser that `allowlist` mode depends on.
//!
//! Three modes:
//!
//! - `off`: the `shell` tool is not registered.
//! - `allowlist`: a command is split into an argument vector by
//!   [`split_command`] and started directly, without a shell interpreter.
//!   Any shell metacharacter is refused, and the vector must match one of
//!   the [`CommandPattern`]s, such as `cargo test *`.
//! - `ask`: the command line runs in the configured shell, after the
//!   permission rules and the built-in deny categories. Matching text on
//!   the host is a guard rail, not a security boundary, because a shell
//!   can hide a command; `allowlist` mode and the container executor are
//!   the boundaries.
//!
//! With `executor = "container"`, commands run through `docker` or
//! `podman` in an image the configuration names, without network access.

use serde::{Deserialize, Serialize};

/// How the `shell` tool may run commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShellMode {
    /// The tool is not registered.
    Off,
    /// Only commands that match an allowlist pattern, started without a shell.
    Allowlist,
    /// Every command, in the configured shell, after the permission rules.
    Ask,
}

impl ShellMode {
    /// The mode as written in a configuration file.
    ///
    /// # Returns
    ///
    /// `off`, `allowlist` or `ask`.
    pub fn as_str(self) -> &'static str {
        match self {
            ShellMode::Off => "off",
            ShellMode::Allowlist => "allowlist",
            ShellMode::Ask => "ask",
        }
    }

    /// How strict the mode is, for comparing two layers.
    ///
    /// # Returns
    ///
    /// 0 for `ask`, 1 for `allowlist`, 2 for `off`.
    pub fn strictness(self) -> u8 {
        match self {
            ShellMode::Ask => 0,
            ShellMode::Allowlist => 1,
            ShellMode::Off => 2,
        }
    }
}

/// Where commands run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Executor {
    /// On the developer's machine, with the user's rights.
    #[default]
    Host,
    /// In a container started by `docker` or `podman`.
    Container,
}

impl Executor {
    /// The executor as written in a configuration file.
    ///
    /// # Returns
    ///
    /// `host` or `container`.
    pub fn as_str(self) -> &'static str {
        match self {
            Executor::Host => "host",
            Executor::Container => "container",
        }
    }
}

/// The program that starts containers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContainerRuntime {
    /// `docker`.
    #[default]
    Docker,
    /// `podman`.
    Podman,
}

impl ContainerRuntime {
    /// The program name.
    ///
    /// # Returns
    ///
    /// `docker` or `podman`.
    pub fn program(self) -> &'static str {
        match self {
            ContainerRuntime::Docker => "docker",
            ContainerRuntime::Podman => "podman",
        }
    }
}

/// The user a container runs as unless the configuration names another:
/// `nobody` on most images.
pub const DEFAULT_CONTAINER_USER: &str = "65534:65534";

/// The memory limit unless the configuration names another.
pub const DEFAULT_CONTAINER_MEMORY: &str = "2g";

/// The process limit unless the configuration names another.
pub const DEFAULT_CONTAINER_PIDS: u32 = 256;

/// `[shell.container]`: the image and limits of the container executor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContainerSettings {
    /// `docker` or `podman`. Default `docker`.
    #[serde(default)]
    pub runtime: ContainerRuntime,
    /// The image commands run in, ideally pinned by digest. Required.
    pub image: String,
    /// The user inside the container, `uid` or `uid:gid`; never root.
    /// Default `65534:65534`.
    #[serde(default = "default_user")]
    pub user: String,
    /// The memory limit, as the runtime's `--memory` takes it. Default `2g`.
    #[serde(default = "default_memory")]
    pub memory: String,
    /// The most processes the container may hold. Default 256.
    #[serde(default = "default_pids")]
    pub pids_limit: u32,
}

/// The serde default for [`ContainerSettings::user`].
fn default_user() -> String {
    DEFAULT_CONTAINER_USER.to_owned()
}

/// The serde default for [`ContainerSettings::memory`].
fn default_memory() -> String {
    DEFAULT_CONTAINER_MEMORY.to_owned()
}

/// The serde default for [`ContainerSettings::pids_limit`].
fn default_pids() -> u32 {
    DEFAULT_CONTAINER_PIDS
}

impl ContainerSettings {
    /// Checks the settings.
    ///
    /// # Errors
    ///
    /// A message naming the key: an empty image, an image or memory value
    /// that starts with `-` (it would be read as an option), a root user,
    /// an empty memory limit, or a process limit of 0.
    pub fn validate(&self) -> Result<(), String> {
        let image = self.image.trim();
        if image.is_empty() || image.starts_with('-') || image.contains(char::is_whitespace) {
            return Err(format!(
                "[shell.container] image `{}` must name an image",
                self.image
            ));
        }
        let uid = self.user.split(':').next().unwrap_or_default().trim();
        if uid.is_empty() || uid == "0" || uid.eq_ignore_ascii_case("root") {
            return Err(format!(
                "[shell.container] user `{}` must name a user other than root",
                self.user
            ));
        }
        if self.user.starts_with('-') || self.user.contains(char::is_whitespace) {
            return Err(format!(
                "[shell.container] user `{}` is not a user",
                self.user
            ));
        }
        let memory = self.memory.trim();
        if memory.is_empty() || memory.starts_with('-') || memory.contains(char::is_whitespace) {
            return Err(format!(
                "[shell.container] memory `{}` must be a size such as 2g",
                self.memory
            ));
        }
        if self.pids_limit == 0 {
            return Err("[shell.container] pids_limit must be at least 1".to_owned());
        }
        Ok(())
    }
}

/// `[shell]` in the user's configuration or a profile. Every key is
/// optional; a missing key takes the default (`ask` on the host) unless a
/// managed policy decides it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ShellConfig {
    /// `off`, `allowlist` or `ask`. Default `ask`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<ShellMode>,
    /// Argument-prefix patterns for `allowlist` mode, such as `cargo test *`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
    /// `host` or `container`. Default `host`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub executor: Option<Executor>,
    /// The container settings for `executor = "container"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<ContainerSettings>,
}

impl ShellConfig {
    /// Checks that the keys fit together.
    ///
    /// # Errors
    ///
    /// A message naming the key: allowlist mode without patterns, patterns
    /// in another mode, an invalid pattern, the container executor without
    /// `[shell.container]` or the other way round, or invalid container
    /// settings.
    pub fn validate(&self) -> Result<(), String> {
        validate_shell(
            self.mode.unwrap_or(ShellMode::Ask),
            &self.allow,
            self.executor.unwrap_or_default(),
            self.container.as_ref(),
        )
    }
}

/// The checks [`ShellConfig::validate`] and the policy share.
///
/// # Arguments
///
/// * `mode` - The mode in force.
/// * `allow` - The allowlist patterns.
/// * `executor` - The executor in force.
/// * `container` - The container settings, if any.
///
/// # Errors
///
/// A message naming the key that is wrong.
pub fn validate_shell(
    mode: ShellMode,
    allow: &[String],
    executor: Executor,
    container: Option<&ContainerSettings>,
) -> Result<(), String> {
    match mode {
        ShellMode::Allowlist if allow.is_empty() => {
            return Err(
                "[shell] mode = \"allowlist\" needs at least one pattern in allow".to_owned(),
            );
        }
        ShellMode::Off | ShellMode::Ask if !allow.is_empty() => {
            return Err("[shell] allow is only used with mode = \"allowlist\"".to_owned());
        }
        _ => {}
    }
    for pattern in allow {
        CommandPattern::parse(pattern)
            .map_err(|error| format!("`{pattern}` in [shell] allow: {error}"))?;
    }
    match (executor, container) {
        (Executor::Container, None) => Err(
            "[shell] executor = \"container\" needs a [shell.container] table with an image"
                .to_owned(),
        ),
        (Executor::Host, Some(_)) => {
            Err("[shell.container] is only used with executor = \"container\"".to_owned())
        }
        (Executor::Container, Some(settings)) => settings.validate(),
        (Executor::Host, None) => Ok(()),
    }
}

/// Characters a command may not contain in `allowlist` mode, because some
/// shell would read them as chaining, redirection, substitution, grouping
/// or a second line: `;`, `&`, `|`, `<`, `>`, the backtick, `$(` (through
/// `(`), parentheses, braces, line breaks and NUL.
pub const METACHARACTERS: &[char] = &[
    ';', '&', '|', '<', '>', '`', '(', ')', '{', '}', '\n', '\r', '\0',
];

/// Splits a command line into an argument vector, refusing anything a shell
/// would treat specially.
///
/// Words are separated by spaces and tabs. A word may be quoted with `"` or
/// `'` to keep spaces; the quotes are removed and nothing inside is
/// interpreted. A backslash is an ordinary character, so Windows paths
/// work.
///
/// # Arguments
///
/// * `command` - The command line.
///
/// # Returns
///
/// The words, the program first.
///
/// # Errors
///
/// A message when the line is empty, holds a character from
/// [`METACHARACTERS`], or has an unterminated quote.
pub fn split_command(command: &str) -> Result<Vec<String>, String> {
    if let Some(found) = command.chars().find(|c| METACHARACTERS.contains(c)) {
        return Err(format!(
            "`{}` is a shell metacharacter; in allowlist mode a command is one program with plain arguments",
            found.escape_debug()
        ));
    }
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    for c in command.chars() {
        match quote {
            Some(open) if c == open => quote = None,
            Some(_) => current.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                in_word = true;
            }
            None if c == ' ' || c == '\t' => {
                if in_word {
                    words.push(std::mem::take(&mut current));
                    in_word = false;
                }
            }
            None => {
                current.push(c);
                in_word = true;
            }
        }
    }
    if quote.is_some() {
        return Err("the command has an unterminated quote".to_owned());
    }
    if in_word {
        words.push(current);
    }
    if words.is_empty() {
        return Err("the command is empty".to_owned());
    }
    Ok(words)
}

/// A program's name without its folder and without the extension Windows
/// adds, in lower case.
///
/// # Arguments
///
/// * `program` - The program as written.
///
/// # Returns
///
/// The name, such as `cargo` for `cargo.exe`.
pub fn program_name(program: &str) -> String {
    let base = program
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(program)
        .to_ascii_lowercase();
    for extension in [".exe", ".cmd", ".bat", ".com", ".ps1"] {
        if let Some(stem) = base.strip_suffix(extension) {
            return stem.to_owned();
        }
    }
    base
}

/// One allowlist pattern: literal words that must start the argument
/// vector, optionally followed by `*` for any further arguments.
///
/// `cargo test *` allows `cargo test` and `cargo test -p anvil-core`;
/// `git status` allows exactly `git status`. The program is compared by
/// [`program_name`] (so `cargo.exe` matches `cargo`), the other words
/// exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandPattern {
    /// The literal words, the program first.
    words: Vec<String>,
    /// Whether further arguments are allowed.
    any_rest: bool,
}

impl CommandPattern {
    /// Parses a pattern.
    ///
    /// # Arguments
    ///
    /// * `text` - The pattern, such as `cargo test *`.
    ///
    /// # Returns
    ///
    /// The pattern.
    ///
    /// # Errors
    ///
    /// A message when the text does not split ([`split_command`]), has no
    /// program, names the program by a path, or uses `*` anywhere but as
    /// the last word.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut words = split_command(text)?;
        let any_rest = words.last().is_some_and(|word| word == "*");
        if any_rest {
            words.pop();
        }
        let Some(program) = words.first() else {
            return Err("a pattern names a program first".to_owned());
        };
        if program.contains(['/', '\\']) {
            return Err(format!(
                "`{program}` is a path; name a program found on PATH"
            ));
        }
        if words.iter().any(|word| word.contains('*')) {
            return Err("`*` may only be the last word".to_owned());
        }
        words[0] = program_name(program);
        Ok(Self { words, any_rest })
    }

    /// Whether an argument vector fits the pattern.
    ///
    /// # Arguments
    ///
    /// * `argv` - The words of a command, the program first.
    ///
    /// # Returns
    ///
    /// `true` when the program and the following literal words match and
    /// there are no further arguments, or the pattern ends in `*`.
    pub fn matches(&self, argv: &[String]) -> bool {
        let Some((program, args)) = argv.split_first() else {
            return false;
        };
        if program_name(program) != self.words[0] {
            return false;
        }
        let literal = &self.words[1..];
        if args.len() < literal.len() || (!self.any_rest && args.len() != literal.len()) {
            return false;
        }
        literal.iter().zip(args).all(|(want, got)| want == got)
    }

    /// The pattern as text, for messages.
    ///
    /// # Returns
    ///
    /// The words joined by spaces, with ` *` when further arguments are allowed.
    pub fn text(&self) -> String {
        let mut text = self.words.join(" ");
        if self.any_rest {
            text.push_str(" *");
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_owned()).collect()
    }

    #[test]
    fn plain_commands_split_into_words_and_quotes_keep_spaces() {
        assert_eq!(
            split_command("cargo test -p anvil-core").expect("splits"),
            argv(&["cargo", "test", "-p", "anvil-core"])
        );
        assert_eq!(
            split_command("  git   diff\t--stat ").expect("splits"),
            argv(&["git", "diff", "--stat"])
        );
        assert_eq!(
            split_command(r#"git commit -m "two words" 'x y' C:\a\b"#).expect("splits"),
            argv(&["git", "commit", "-m", "two words", "x y", r"C:\a\b"])
        );
        assert_eq!(
            split_command(r#"echo "" a"#).expect("splits"),
            argv(&["echo", "", "a"])
        );
        assert!(split_command("   ").is_err());
        assert!(split_command("echo \"open").is_err());
    }

    #[test]
    fn shell_metacharacters_are_refused_in_allowlist_mode() {
        for command in [
            "cargo test; rm -rf /",
            "cargo test && curl x",
            "cargo test || curl x",
            "cargo test | sh",
            "cargo test & calc",
            "cargo test > out",
            "cargo test < in",
            "cargo test `id`",
            "cargo test $(id)",
            "cargo test ${HOME}",
            "cargo test {a,b}",
            "cargo test @(1)",
            "cargo test\nid",
            "cargo test\r\nid",
            "cargo \"test;id\"",
            "cargo test\0",
        ] {
            assert!(split_command(command).is_err(), "{command:?}");
        }
    }

    #[test]
    fn patterns_match_argument_prefixes() {
        let test = CommandPattern::parse("cargo test *").expect("parses");
        assert!(test.matches(&argv(&["cargo", "test"])));
        assert!(test.matches(&argv(&["cargo", "test", "-p", "x"])));
        assert!(test.matches(&argv(&["CARGO.EXE", "test"])));
        assert!(!test.matches(&argv(&["cargo", "build"])));
        assert!(!test.matches(&argv(&["cargo"])));
        assert!(!test.matches(&argv(&["cargox", "test"])));
        let exact = CommandPattern::parse("git status").expect("parses");
        assert!(exact.matches(&argv(&["git", "status"])));
        assert!(!exact.matches(&argv(&["git", "status", "-s"])));
        assert_eq!(exact.text(), "git status");
        assert_eq!(test.text(), "cargo test *");
        let any = CommandPattern::parse("just *").expect("parses");
        assert!(any.matches(&argv(&["just"])));
        assert!(any.matches(&argv(&["just", "check"])));
    }

    #[test]
    fn bad_patterns_are_errors() {
        for pattern in [
            "",
            "*",
            "cargo * test",
            "cargo te*",
            "/usr/bin/cargo test",
            r"C:\tools\cargo.exe",
            "cargo test; id",
        ] {
            assert!(CommandPattern::parse(pattern).is_err(), "{pattern}");
        }
    }

    #[test]
    fn the_shell_table_is_checked_as_a_whole() {
        let container = ContainerSettings {
            runtime: ContainerRuntime::Podman,
            image: "registry.example/build@sha256:abc".to_owned(),
            user: default_user(),
            memory: default_memory(),
            pids_limit: default_pids(),
        };
        let ok = [
            validate_shell(ShellMode::Ask, &[], Executor::Host, None),
            validate_shell(ShellMode::Off, &[], Executor::Host, None),
            validate_shell(
                ShellMode::Allowlist,
                &["cargo test *".to_owned()],
                Executor::Container,
                Some(&container),
            ),
        ];
        for result in ok {
            assert!(result.is_ok(), "{result:?}");
        }
        let root = ContainerSettings {
            user: "0:0".to_owned(),
            ..container.clone()
        };
        let no_pids = ContainerSettings {
            pids_limit: 0,
            ..container.clone()
        };
        let option_image = ContainerSettings {
            image: "--privileged".to_owned(),
            ..container.clone()
        };
        let errors = [
            validate_shell(ShellMode::Allowlist, &[], Executor::Host, None),
            validate_shell(ShellMode::Ask, &["git *".to_owned()], Executor::Host, None),
            validate_shell(
                ShellMode::Allowlist,
                &["git;*".to_owned()],
                Executor::Host,
                None,
            ),
            validate_shell(ShellMode::Ask, &[], Executor::Container, None),
            validate_shell(ShellMode::Ask, &[], Executor::Host, Some(&container)),
            validate_shell(ShellMode::Ask, &[], Executor::Container, Some(&root)),
            validate_shell(ShellMode::Ask, &[], Executor::Container, Some(&no_pids)),
            validate_shell(
                ShellMode::Ask,
                &[],
                Executor::Container,
                Some(&option_image),
            ),
        ];
        for result in errors {
            assert!(result.is_err());
        }
    }

    #[test]
    fn container_settings_take_restrictive_defaults() {
        let settings: ContainerSettings = toml::from_str("image = \"build:1\"\n").expect("parses");
        assert_eq!(settings.runtime, ContainerRuntime::Docker);
        assert_eq!(settings.user, DEFAULT_CONTAINER_USER);
        assert_eq!(settings.memory, DEFAULT_CONTAINER_MEMORY);
        assert_eq!(settings.pids_limit, DEFAULT_CONTAINER_PIDS);
        assert!(toml::from_str::<ContainerSettings>("image = \"x\"\nnetwork = true\n").is_err());
    }
}
