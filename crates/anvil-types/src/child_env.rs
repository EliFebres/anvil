//! The environment anvil gives every program it starts: `shell` tool
//! commands, check and acceptance commands, hooks, MCP servers, rule checks
//! and anvil's own git calls.
//!
//! Those programs do not inherit anvil's environment. They get an explicit
//! list of variables (the search path, the home and temp folders, the
//! locale, what Windows needs to start a program, and the Rust toolchain's
//! folders) plus the names the user lists in `[tools] pass_env`. Everything
//! else stays out, in particular the provider keys anvil read from
//! `~/.anvil/.env` and any token in the user's own environment.
//!
//! anvil's own `git` calls (the session diff, build commits) get this list
//! too, plus the author and committer identity and the location of the
//! user's git config (`anvil_core::git::GIT_VARS`); they never contact a
//! remote, so no credential helper or SSH agent is needed.

use std::ffi::OsString;
use std::sync::OnceLock;

/// Variables every child gets when they are set.
pub const BASE_VARS: &[&str] = &[
    // Finding programs and the user's folders.
    "PATH",
    "HOME",
    "USERPROFILE",
    "USER",
    "USERNAME",
    "LOGNAME",
    "SHELL",
    // Temporary files.
    "TMPDIR",
    "TEMP",
    "TMP",
    // Language, time zone and terminal.
    "LANG",
    "LANGUAGE",
    "TZ",
    "TERM",
    "NO_COLOR",
    // What Windows programs need to start and find their own files.
    "SYSTEMROOT",
    "WINDIR",
    "COMSPEC",
    "PATHEXT",
    "SYSTEMDRIVE",
    "HOMEDRIVE",
    "HOMEPATH",
    "APPDATA",
    "LOCALAPPDATA",
    "PROGRAMDATA",
    "PROGRAMFILES",
    "PROGRAMFILES(X86)",
    "PROGRAMW6432",
    "COMMONPROGRAMFILES",
    "COMMONPROGRAMFILES(X86)",
    "COMMONPROGRAMW6432",
    "NUMBER_OF_PROCESSORS",
    "PROCESSOR_ARCHITECTURE",
    "PROCESSOR_IDENTIFIER",
    "OS",
    "PSMODULEPATH",
    // The Rust toolchain installed by rustup.
    "CARGO_HOME",
    "RUSTUP_HOME",
];

/// Prefixes of variables every child gets: the locale categories.
pub const BASE_PREFIXES: &[&str] = &["LC_"];

/// Words that mark a variable as a secret. A variable matched only by a
/// prefix is still dropped when its name contains one of these.
pub const SECRET_WORDS: &[&str] = &["TOKEN", "KEY", "SECRET", "PASSWORD", "PASSWD", "CREDENTIAL"];

/// The names from `[tools] pass_env`, set once at startup.
static EXTRA: OnceLock<Vec<String>> = OnceLock::new();

/// Records the names the user's `[tools] pass_env` adds to [`BASE_VARS`].
/// The first call wins; anvil makes it once, before any child starts.
///
/// # Arguments
///
/// * `names` - The variable names.
pub fn set_pass_env(names: Vec<String>) {
    let _ = EXTRA.set(names);
}

/// Whether a variable name looks like it holds a secret.
///
/// # Arguments
///
/// * `name` - The variable name.
///
/// # Returns
///
/// `true` when the upper-cased name contains one of [`SECRET_WORDS`].
pub fn looks_secret(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    SECRET_WORDS.iter().any(|word| upper.contains(word))
}

/// Whether a child may receive a variable.
///
/// Names compare without regard to case, because Windows treats `Path` and
/// `PATH` as one variable.
///
/// # Arguments
///
/// * `name` - The variable name.
/// * `extra` - Names the user added.
///
/// # Returns
///
/// `true` for a name in [`BASE_VARS`] or `extra`, or one with a prefix in
/// [`BASE_PREFIXES`] that does not look like a secret.
pub fn is_passed(name: &str, extra: &[String]) -> bool {
    let upper = name.to_ascii_uppercase();
    if BASE_VARS.contains(&upper.as_str())
        || extra
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(name))
    {
        return true;
    }
    BASE_PREFIXES.iter().any(|prefix| upper.starts_with(prefix)) && !looks_secret(name)
}

/// Whether a child receives a variable, counting the names the user's
/// `[tools] pass_env` adds.
///
/// # Arguments
///
/// * `name` - The variable name.
///
/// # Returns
///
/// `true` when [`vars`] would pass the variable on.
pub fn may_pass(name: &str) -> bool {
    is_passed(name, EXTRA.get().map(Vec::as_slice).unwrap_or_default())
}

/// The variables of anvil's own environment a child may receive.
///
/// Call `env_clear()` on the command and then `envs(vars())`.
///
/// # Returns
///
/// The name and value pairs.
pub fn vars() -> Vec<(OsString, OsString)> {
    let extra = EXTRA.get().map(Vec::as_slice).unwrap_or_default();
    std::env::vars_os()
        .filter(|(name, _)| name.to_str().is_some_and(|name| is_passed(name, extra)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_base_list_passes_and_keys_do_not() {
        for name in [
            "PATH",
            "Path",
            "HOME",
            "TEMP",
            "SystemRoot",
            "CARGO_HOME",
            "LC_ALL",
        ] {
            assert!(is_passed(name, &[]), "{name}");
        }
        for name in [
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "GITHUB_TOKEN",
            "AWS_SECRET_ACCESS_KEY",
            "DB_PASSWORD",
            "HTTPS_PROXY",
            "ANVIL_HOME",
            "LC_SECRET_TOKEN",
        ] {
            assert!(!is_passed(name, &[]), "{name}");
        }
    }

    #[test]
    fn the_user_can_add_a_name_even_one_that_looks_secret() {
        let extra = vec!["JAVA_HOME".to_owned(), "NPM_TOKEN".to_owned()];
        assert!(is_passed("JAVA_HOME", &extra));
        assert!(is_passed("npm_token", &extra));
        assert!(!is_passed("GITHUB_TOKEN", &extra));
    }

    #[test]
    fn no_base_name_looks_secret() {
        for name in BASE_VARS {
            assert!(!looks_secret(name), "{name}");
            assert_eq!(*name, name.to_ascii_uppercase(), "{name} is upper case");
        }
    }
}
