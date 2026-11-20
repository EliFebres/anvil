//! Every git command anvil runs in the user's project, with the settings
//! that stop the repository from choosing a program for git to run.
//!
//! anvil runs git in the project to snapshot and diff a session's changes,
//! to commit the items of `anvil build`, to list changed files for
//! `anvil rules check`, and to list tracked files for `anvil scan-keys`.
//! Like every other program anvil starts, git begins with a cleared
//! environment: the variables of [`anvil_types::child_env`] (search path,
//! home, temp, locale, what Windows needs) plus [`GIT_VARS`], the author and
//! committer identity and the variables that say where the user's own git
//! config is. Provider keys and tokens never reach it, nor do variables
//! such as `GIT_DIR` or `GIT_SSH_COMMAND` that would change what git does.
//! The repository's own config (`.git/config`, the files it includes, and
//! `config.worktree`) is not the user's, so every call made through
//! [`command`]:
//!
//! - runs no hooks (`core.hooksPath` points at a folder that cannot exist)
//!   and no file system monitor (`core.fsmonitor=false`);
//! - never starts a pager (`--no-pager`) or an editor (`core.editor=:` and
//!   `sequence.editor=:`, which git treats as "no editor");
//! - may not contact a remote (`protocol.allow=never`), so
//!   `core.sshCommand`, credential helpers and proxies never run;
//! - refuses to start when the repository's own config names a filter or
//!   a signing program, which no command-line setting can switch off.
//!
//! A setting given with `-c` beats every config file, so the repository
//! cannot undo these. Diffs also pass [`NO_DIFF_PROGRAMS`]. Aliases need
//! nothing: git never lets an alias replace a built-in command, and anvil
//! runs built-in commands only.

use std::path::Path;
use std::process::Command;

use anvil_types::child_env;
use thiserror::Error;

/// Variables git receives on top of [`child_env::vars`] when they are set:
/// the identity a build commit is made with, and where the user's own
/// config lives (git reads the identity from there otherwise). anvil's git
/// calls never contact a remote, so no SSH agent or credential variable
/// is passed.
pub const GIT_VARS: &[&str] = &[
    "GIT_AUTHOR_NAME",
    "GIT_AUTHOR_EMAIL",
    "GIT_COMMITTER_NAME",
    "GIT_COMMITTER_EMAIL",
    "XDG_CONFIG_HOME",
    "GIT_CONFIG_GLOBAL",
    "GIT_CONFIG_SYSTEM",
    "GIT_CONFIG_NOSYSTEM",
];

/// Options given before every git command anvil runs in a project.
pub const SAFE_ARGS: &[&str] = &[
    "--no-pager",
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.editor=:",
    "-c",
    "sequence.editor=:",
    "-c",
    "protocol.allow=never",
];

/// Diff options that stop git from running an external diff program or a
/// text conversion program.
pub const NO_DIFF_PROGRAMS: &[&str] = &["--no-ext-diff", "--no-textconv"];

/// Config keys whose value git runs as a program and that [`SAFE_ARGS`]
/// cannot turn off: clean and smudge filters, and commit signing programs.
/// git lowercases section and key names before matching.
const PROGRAM_KEYS: &str = r"^(filter\..+\.(clean|smudge|process)|gpg\.(.+\.)?program)$";

/// Config keys an allowlisted git command refuses when the repository's
/// own config sets them: [`PROGRAM_KEYS`], aliases, the diff, merge and
/// credential programs, and the settings that move git's work tree. A
/// model that may run `git config` could set any of them, and the next
/// allowlisted git call would run the program or write outside the
/// workspace.
const ALLOWLIST_KEYS: &str = r"^(filter\..+\.(clean|smudge|process)|gpg\.(.+\.)?program|alias\..+|diff\.external|diff\..+\.(textconv|command)|merge\..+\.driver|(diff|merge)tool\..+\.(cmd|path)|credential\..+|core\.(askpass|alternaterefscommand|worktree|gitproxy|sshcommand)|interactive\.difffilter|submodule\..+\.update)$";

/// Subcommands that show diffs; an allowlisted call gets
/// [`NO_DIFF_PROGRAMS`] right after the subcommand.
const DIFF_SUBCOMMANDS: [&str; 8] = [
    "diff",
    "log",
    "show",
    "whatchanged",
    "format-patch",
    "diff-tree",
    "diff-index",
    "diff-files",
];

/// Subcommands that convert text by default but take no external diff
/// option; an allowlisted call gets `--no-textconv`.
const TEXTCONV_SUBCOMMANDS: [&str; 2] = ["blame", "annotate"];

/// Why anvil did not run git.
#[derive(Debug, Error)]
pub enum GitError {
    /// git could not be started.
    #[error("cannot run git: {0}")]
    Spawn(#[from] std::io::Error),
    /// git could not report the repository's config.
    #[error("cannot read the repository's git config: {0}")]
    Config(String),
    /// The repository's own config names a program git would run.
    #[error(
        "this repository's own git config names programs anvil will not let git run ({}); remove them from .git/config to let anvil use git here",
        keys.join(", ")
    )]
    RepositoryProgram {
        /// The config keys, as git names them.
        keys: Vec<String>,
    },
    /// An allowlisted command names an alias, which can run any program.
    #[error(
        "`git {name}` runs an alias from git's config; under a shell allowlist git runs built-in commands only"
    )]
    Alias {
        /// The alias name, as git stores it.
        name: String,
    },
}

/// A git command for `cwd`, with [`SAFE_ARGS`] already given, ready for
/// its subcommand.
///
/// # Arguments
///
/// * `cwd` - The folder git runs in, normally the project root.
///
/// # Returns
///
/// The command. Add the subcommand and its arguments, then run it.
///
/// # Errors
///
/// [`GitError::RepositoryProgram`] when the repository's own config names a
/// filter or signing program; [`GitError::Spawn`] or [`GitError::Config`]
/// when git cannot be asked.
pub fn command(cwd: &Path) -> Result<Command, GitError> {
    let keys = repository_keys_matching(cwd, PROGRAM_KEYS)?;
    if !keys.is_empty() {
        return Err(GitError::RepositoryProgram { keys });
    }
    Ok(safe_git(cwd))
}

/// The git command a shell allowlist lets the model run, with the same
/// safe settings as [`command`] and more checks, because the model chose
/// the arguments: the repository's own config may not set any key of
/// [`ALLOWLIST_KEYS`], the subcommand may not be an alias from any config
/// file, and diff subcommands get [`NO_DIFF_PROGRAMS`].
///
/// The caller has already refused options before the subcommand and the
/// options that write files or run programs
/// (`crate::tools::shell::argument_refusal`).
///
/// # Arguments
///
/// * `cwd` - The folder git runs in, the project root.
/// * `args` - The model's arguments after `git`.
///
/// # Returns
///
/// The command, ready to run.
///
/// # Errors
///
/// [`GitError::RepositoryProgram`] when the repository's own config sets
/// a refused key, [`GitError::Alias`] when the subcommand is an alias,
/// [`GitError::Spawn`] or [`GitError::Config`] when git cannot be asked.
pub fn allowlisted(cwd: &Path, args: &[String]) -> Result<Command, GitError> {
    let keys = repository_keys_matching(cwd, ALLOWLIST_KEYS)?;
    if !keys.is_empty() {
        return Err(GitError::RepositoryProgram { keys });
    }
    if let Some(subcommand) = args.iter().find(|arg| !arg.starts_with('-')) {
        let output = safe_git(cwd)
            .args(["config", "--name-only", "--get-regexp", r"^alias\."])
            .output()?;
        if !matches!(output.status.code(), Some(0 | 1)) {
            return Err(GitError::Config(
                String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            ));
        }
        if let Some(name) = alias_named(&String::from_utf8_lossy(&output.stdout), subcommand) {
            return Err(GitError::Alias { name });
        }
    }
    let mut git = safe_git(cwd);
    git.args(allowlisted_args(args));
    Ok(git)
}

/// The model's git arguments with the diff programs turned off: after a
/// subcommand of [`DIFF_SUBCOMMANDS`] come [`NO_DIFF_PROGRAMS`], after one
/// of [`TEXTCONV_SUBCOMMANDS`] comes `--no-textconv`.
///
/// # Arguments
///
/// * `args` - The arguments after `git`.
///
/// # Returns
///
/// The arguments to pass after [`SAFE_ARGS`].
pub fn allowlisted_args(args: &[String]) -> Vec<String> {
    let mut out = args.to_vec();
    if let Some(index) = args.iter().position(|arg| !arg.starts_with('-')) {
        let subcommand = args[index].as_str();
        let extra: &[&str] = if DIFF_SUBCOMMANDS.contains(&subcommand) {
            NO_DIFF_PROGRAMS
        } else if TEXTCONV_SUBCOMMANDS.contains(&subcommand) {
            &["--no-textconv"]
        } else {
            &[]
        };
        for (offset, option) in extra.iter().enumerate() {
            out.insert(index + 1 + offset, (*option).to_owned());
        }
    }
    out
}

/// The repository-scope keys in `cwd`'s git config that match `regex`.
fn repository_keys_matching(cwd: &Path, regex: &str) -> Result<Vec<String>, GitError> {
    let output = safe_git(cwd)
        .args([
            "config",
            "--show-scope",
            "--name-only",
            "--get-regexp",
            regex,
        ])
        .output()?;
    // git config exits with 1 when no key matches.
    if !matches!(output.status.code(), Some(0 | 1)) {
        return Err(GitError::Config(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    Ok(repository_keys(&String::from_utf8_lossy(&output.stdout)))
}

/// The alias in a `git config --name-only` listing of `alias.*` keys that
/// `subcommand` would run; git compares alias names without regard to case.
fn alias_named(listing: &str, subcommand: &str) -> Option<String> {
    listing
        .lines()
        .filter_map(|line| line.trim().strip_prefix("alias."))
        .find(|name| name.eq_ignore_ascii_case(subcommand))
        .map(str::to_owned)
}

/// A git command that reads no repository, such as `git --version`, with
/// [`SAFE_ARGS`] and the same cleared environment as [`command`]. It runs
/// in the system temp folder, so no project's config applies.
///
/// # Returns
///
/// The command. Add the arguments, then run it.
pub fn standalone() -> Command {
    safe_git(&std::env::temp_dir())
}

/// `git` with [`SAFE_ARGS`], run in `cwd` with a cleared environment
/// refilled from [`child_env::vars`] and [`GIT_VARS`].
fn safe_git(cwd: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .args(SAFE_ARGS)
        .current_dir(cwd)
        .env_clear()
        .envs(child_env::vars())
        .envs(GIT_VARS.iter().filter_map(|name| {
            std::env::var_os(name).map(|value| (std::ffi::OsString::from(name), value))
        }));
    command
}

/// The keys in a `git config --show-scope --name-only` listing that come
/// from the repository's own config rather than the user's or the system's.
///
/// # Arguments
///
/// * `listing` - Lines of `<scope>\t<key>`.
///
/// # Returns
///
/// The keys whose scope is `local` or `worktree`. Files a repository config
/// includes report the scope of the file that includes them.
pub fn repository_keys(listing: &str) -> Vec<String> {
    listing
        .lines()
        .filter_map(|line| {
            let (scope, key) = line.split_once('\t')?;
            matches!(scope, "local" | "worktree").then(|| key.to_owned())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// The variable that makes a re-run of this test binary print what git
    /// passes on to a program it starts.
    const GIT_ENV_PROBE: &str = "ANVIL_TEST_GIT_ENV_PROBE";

    #[test]
    fn git_helpers_start_from_a_cleared_environment() {
        let dir = tempfile::tempdir().expect("dir");
        if std::env::var_os(GIT_ENV_PROBE).is_some() {
            let Ok(mut git) = command(dir.path()) else {
                return;
            };
            let output = git
                .args(["-c", "alias.anvil-probe=!env", "anvil-probe"])
                .output()
                .expect("git runs");
            let text = String::from_utf8_lossy(&output.stdout);
            assert!(output.status.success(), "{text}");
            for value in [
                "probe-openai-value",
                "probe-github-value",
                "probe-ssh-value",
            ] {
                assert!(!text.contains(value), "{value} reached git:\n{text}");
            }
            assert!(
                text.contains("GIT_AUTHOR_NAME=Probe Author"),
                "the identity is passed:\n{text}"
            );
            return;
        }
        if !run(dir.path(), &["--version"]) {
            return;
        }
        let exe = std::env::current_exe().expect("test binary");
        let output = std::process::Command::new(exe)
            .args([
                "--exact",
                "git::tests::git_helpers_start_from_a_cleared_environment",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(GIT_ENV_PROBE, "1")
            .env("OPENAI_API_KEY", "probe-openai-value")
            .env("GITHUB_TOKEN", "probe-github-value")
            .env("GIT_SSH_COMMAND", "probe-ssh-value")
            .env("GIT_AUTHOR_NAME", "Probe Author")
            .output()
            .expect("re-runs the test binary");
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(output.status.success(), "{text}");
        assert!(text.contains("1 passed"), "the probe ran: {text}");
    }

    #[test]
    fn only_the_repository_scopes_count() {
        let listing = "system\tfilter.lfs.clean\nglobal\tgpg.program\nlocal\tfilter.x.clean\nworktree\tgpg.ssh.program\ncommand\tfilter.y.smudge\n";
        assert_eq!(
            repository_keys(listing),
            vec!["filter.x.clean".to_owned(), "gpg.ssh.program".to_owned()]
        );
        assert!(repository_keys("").is_empty());
    }

    /// Runs plain git for test setup; `false` when git is unavailable.
    fn setup(dir: &Path, args: &[&str]) -> bool {
        Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(args)
            .current_dir(dir)
            .output()
            .is_ok_and(|output| output.status.success())
    }

    /// A repository with one commit, or `None` without git.
    fn repository() -> Option<tempfile::TempDir> {
        let dir = tempfile::tempdir().expect("tempdir");
        if !setup(dir.path(), &["init", "-q"]) {
            eprintln!("git not available; skipping");
            return None;
        }
        std::fs::write(dir.path().join("a.txt"), "one\n").expect("write");
        assert!(setup(dir.path(), &["add", "a.txt"]));
        assert!(setup(dir.path(), &["commit", "-q", "-m", "init"]));
        Some(dir)
    }

    /// A shell command that creates `marker`, quoted for git's shell.
    fn touch(marker: &Path) -> String {
        format!("touch \"{}\"", marker.to_string_lossy().replace('\\', "/"))
    }

    /// Sets a key in the repository's own config.
    fn set_local(dir: &Path, key: &str, value: &str) {
        assert!(setup(dir, &["config", "--local", key, value]), "{key}");
    }

    /// Runs one git command through [`command`] and says whether it succeeded.
    fn run(dir: &Path, args: &[&str]) -> bool {
        let mut git = command(dir).expect("no repository programs");
        git.args(["-c", "user.name=t", "-c", "user.email=t@example.com"]);
        git.args(args)
            .output()
            .is_ok_and(|output| output.status.success())
    }

    #[test]
    fn allowlisted_args_turn_off_diff_programs_after_the_subcommand() {
        let args = |words: &[&str]| {
            allowlisted_args(&words.iter().map(|w| (*w).to_owned()).collect::<Vec<_>>())
        };
        assert_eq!(
            args(&["diff", "HEAD"]),
            ["diff", "--no-ext-diff", "--no-textconv", "HEAD"]
        );
        assert_eq!(
            args(&["--no-pager", "log", "-p"]),
            ["--no-pager", "log", "--no-ext-diff", "--no-textconv", "-p"]
        );
        assert_eq!(
            args(&["blame", "a.txt"]),
            ["blame", "--no-textconv", "a.txt"]
        );
        assert_eq!(args(&["status", "-s"]), ["status", "-s"]);
        assert_eq!(args(&["--version"]), ["--version"]);
    }

    #[test]
    fn an_alias_from_any_scope_is_found_by_name() {
        let listing = "alias.co
alias.lg
";
        assert_eq!(alias_named(listing, "co"), Some("co".to_owned()));
        assert_eq!(alias_named(listing, "CO"), Some("co".to_owned()));
        assert_eq!(alias_named(listing, "commit"), None);
        assert_eq!(alias_named("", "co"), None);
    }

    #[test]
    fn allowlisted_git_refuses_repository_aliases_and_programs() {
        let Some(dir) = repository() else { return };
        let words = |list: &[&str]| list.iter().map(|w| (*w).to_owned()).collect::<Vec<_>>();
        assert!(allowlisted(dir.path(), &words(&["status"])).is_ok());
        for (key, value) in [
            ("alias.x", "!echo ALIAS-RAN"),
            ("alias.st", "status"),
            ("diff.external", "evil"),
            ("diff.foo.textconv", "evil"),
            ("diff.foo.command", "evil"),
            ("merge.foo.driver", "evil"),
            ("credential.helper", "evil"),
            ("core.worktree", ".."),
            ("core.askPass", "evil"),
            ("submodule.x.update", "!evil"),
            ("interactive.diffFilter", "evil"),
            ("filter.x.clean", "evil"),
        ] {
            set_local(dir.path(), key, value);
            let refused = allowlisted(dir.path(), &words(&["status"]));
            assert!(
                matches!(refused, Err(GitError::RepositoryProgram { .. })),
                "{key}: {:?}",
                refused.map(|_| ())
            );
            assert!(
                setup(dir.path(), &["config", "--local", "--unset", key]),
                "{key}"
            );
        }
        let marker = dir.path().join("alias-ran");
        let alias = format!("!{}", touch(&marker));
        assert!(setup(dir.path(), &["config", "--local", "alias.x", &alias]));
        assert!(allowlisted(dir.path(), &words(&["x"])).is_err());
        assert!(!marker.exists(), "the alias ran");
    }

    /// Writes an executable hook script that creates `marker`.
    fn plant_hook(dir: &Path, name: &str, marker: &Path) {
        let hook = dir.join(".git").join("hooks").join(name);
        std::fs::write(&hook, format!("#!/bin/sh\n{}\n", touch(marker))).expect("hook");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
    }

    #[test]
    fn hooks_planted_in_the_repository_do_not_run() {
        let Some(dir) = repository() else { return };
        let marker = dir.path().join("hook-ran");
        for name in ["pre-commit", "commit-msg", "post-commit", "post-checkout"] {
            plant_hook(dir.path(), name, &marker);
        }
        std::fs::write(dir.path().join("a.txt"), "two\n").expect("write");
        assert!(run(dir.path(), &["add", "a.txt"]));
        assert!(run(dir.path(), &["commit", "-q", "-m", "change"]));
        assert!(run(dir.path(), &["checkout", "-q", "-b", "side"]));
        assert!(!marker.exists(), "a hook ran");

        // Plain git runs the same hook, so the planted hook is real.
        assert!(setup(dir.path(), &["checkout", "-q", "-b", "other"]));
        assert!(marker.exists(), "the control hook did not run");
    }

    #[test]
    fn config_programs_the_repository_names_do_not_run() {
        let Some(dir) = repository() else { return };
        let marker = dir.path().join("program-ran");
        let program = touch(&marker);
        set_local(dir.path(), "core.fsmonitor", &format!("{program}; true"));
        set_local(dir.path(), "core.pager", &program);
        set_local(dir.path(), "core.editor", &program);
        set_local(dir.path(), "sequence.editor", &program);
        set_local(dir.path(), "core.sshCommand", &program);
        set_local(dir.path(), "alias.status", &format!("!{program}"));
        set_local(dir.path(), "diff.external", &program);
        set_local(dir.path(), "diff.evil.textconv", &program);
        set_local(dir.path(), "diff.evil.command", &program);
        std::fs::write(dir.path().join(".gitattributes"), "*.txt diff=evil\n").expect("write");
        std::fs::write(dir.path().join("a.txt"), "two\n").expect("write");

        assert!(run(dir.path(), &["status", "--porcelain"]));
        let mut diff = vec!["diff"];
        diff.extend(NO_DIFF_PROGRAMS);
        assert!(run(dir.path(), &diff));
        diff.push("HEAD");
        assert!(run(dir.path(), &diff));
        assert!(run(dir.path(), &["add", "-A"]));
        assert!(run(dir.path(), &["commit", "-q", "-m", "change"]));
        let mut fetch = command(dir.path()).expect("no repository programs");
        let fetched = fetch
            .args(["fetch", "ssh://example.invalid/repo.git"])
            .output()
            .expect("git runs");
        assert!(!fetched.status.success(), "protocol.allow=never refuses");
        assert!(!marker.exists(), "a repository program ran");

        // Plain git runs the configured external diff, so the setup is real.
        std::fs::write(dir.path().join("a.txt"), "three\n").expect("write");
        assert!(setup(dir.path(), &["diff"]));
        assert!(marker.exists(), "the control program did not run");
    }

    #[test]
    fn filter_and_signing_programs_in_the_repository_config_are_refused() {
        let Some(dir) = repository() else { return };
        for (key, value) in [
            ("filter.evil.clean", "cat"),
            ("filter.evil.process", "cat"),
            ("gpg.program", "cat"),
            ("gpg.ssh.program", "cat"),
        ] {
            set_local(dir.path(), key, value);
            let refused = command(dir.path()).expect_err(key);
            assert!(
                matches!(&refused, GitError::RepositoryProgram { keys } if keys.contains(&key.to_owned())),
                "{key}: {refused}"
            );
            assert!(setup(dir.path(), &["config", "--local", "--unset", key]));
        }
        assert!(command(dir.path()).is_ok(), "clean again after unset");
    }

    #[test]
    fn a_filter_reached_through_an_include_is_refused() {
        let Some(dir) = repository() else { return };
        let extra: PathBuf = dir.path().join(".git").join("extra.cfg");
        std::fs::write(&extra, "[filter \"evil\"]\n\tsmudge = cat\n").expect("write");
        set_local(dir.path(), "include.path", "extra.cfg");
        let refused = command(dir.path()).expect_err("included filter");
        assert!(
            refused.to_string().contains("filter.evil.smudge"),
            "{refused}"
        );
    }
}
