//! What a repository may say about how anvil runs in it.
//!
//! anvil does not trust the repository it runs in. The repository's own
//! `.anvil/config.toml` may only make anvil stricter: its `[permissions]`
//! table can add `deny`, `ask` and `shell_programs`. A key that would
//! change providers, roles, the shell or check program, the shell mode,
//! folders or network rules, or loosen permissions is an error that names
//! the key, so a cloned
//! repository can neither change those settings nor seem to. Any other key
//! is ignored with a warning.
//!
//! `.anvil/permissions.toml` may add `deny`, `ask` and `shell_programs`
//! the same way, and `allow` rules, which take effect only once the user
//! trusts the repository (see [`crate::trust`]).

use std::path::{Path, PathBuf};

use crate::config::{CONFIG_DIR, CONFIG_FILE};
use crate::error::ConfigError;

/// The repository's permission file, relative to its root.
pub const PROJECT_PERMISSIONS_FILE: &str = ".anvil/permissions.toml";

/// The permission keys a repository may set in either file; each one only tightens.
pub const PROJECT_RULE_KEYS: [&str; 3] = ["deny", "ask", "shell_programs"];

/// Top-level tables of `.anvil/config.toml` that would loosen anvil or
/// redirect it: providers and their URLs, roles, the shell and check
/// programs and extra folders (`[tools]`), plain-http hosts, and the shell
/// mode and executor (`[shell]`).
const FORBIDDEN_TABLES: [&str; 5] = ["providers", "roles", "tools", "network", "shell"];

/// Permission keys that loosen: refused in `.anvil/config.toml`, and in
/// `.anvil/permissions.toml` except `allow`, which waits for trust there.
const LOOSENING_PERMISSION_KEYS: [&str; 3] = ["allow", "default", "allow_tools"];

/// The commented template `anvil config init` writes into a repository.
pub const PROJECT_TEMPLATE: &str = r#"# anvil settings for this repository.
#
# A repository can only make anvil stricter. This file may add permission
# rules that refuse calls (deny) or make them ask first (ask). Providers,
# roles, tools, network rules and permission `allow` rules are refused here:
# they live in your own ~/.anvil/config.toml. Allow rules for this
# repository go in .anvil/permissions.toml and apply only once you trust it.
#
# A rule is `tool` or `tool:glob`; the glob matches the path for file tools
# and the command line for `shell`.
[permissions]
# deny = ["shell:git push*"]
# ask = ["write_file:migrations/**"]
# Limit `shell` to single programs from this list (it can only narrow yours).
# shell_programs = ["cargo", "git"]
"#;

/// The rules a repository supplies.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectRules {
    /// Calls the repository wants refused.
    pub deny: Vec<String>,
    /// Calls the repository wants asked about, even when the user allows them.
    pub ask: Vec<String>,
    /// Programs the repository limits `shell` to; it can only narrow the
    /// user's own list.
    pub shell_programs: Vec<String>,
    /// Calls the repository's `.anvil/permissions.toml` wants allowed. They
    /// apply only when the user trusts the repository, and only where no
    /// rule of the user's own decides the call.
    pub allow: Vec<String>,
    /// One line per key that was ignored, to show the user.
    pub ignored: Vec<String>,
}

/// Reads the repository's `.anvil/config.toml` and `.anvil/permissions.toml`.
///
/// From `config.toml` only the `[permissions]` table is read, and only
/// `deny`, `ask` and `shell_programs` in it. From `permissions.toml` the
/// whole file is the table, and `allow` is kept as well. A key that would
/// loosen anvil or redirect it is an error; other keys are recorded in
/// [`ProjectRules::ignored`].
///
/// # Arguments
///
/// * `root` - The repository root.
///
/// # Returns
///
/// The rules; empty when neither file exists.
///
/// # Errors
///
/// [`ConfigError::Read`] when a file exists but cannot be read,
/// [`ConfigError::Parse`] when it is not TOML, and [`ConfigError::Project`]
/// when it sets a forbidden key (providers, roles, `[tools]`, `[network]`,
/// `allow` in `config.toml`, `default` or `allow_tools` in either file) or
/// a kept key is not an array of strings.
pub fn load(root: &Path) -> Result<ProjectRules, ConfigError> {
    let mut rules = ProjectRules::default();
    let config_file = root.join(CONFIG_DIR).join(CONFIG_FILE);
    if let Some(mut table) = read_table(&config_file)? {
        let permissions = table.remove("permissions");
        for key in table.keys() {
            if FORBIDDEN_TABLES.contains(&key.as_str()) {
                return Err(forbidden(
                    &config_file,
                    &format!("[{key}]"),
                    "providers, roles, tools, network rules and the shell mode come only from your own ~/.anvil/config.toml",
                ));
            }
            rules.ignored.push(format!(
                "{}: [{key}] is ignored; a repository can only add `deny`, `ask` and `shell_programs` permission rules",
                config_file.display()
            ));
        }
        match permissions {
            Some(toml::Value::Table(permissions)) => {
                take_rules(&mut rules, permissions, &config_file, false)?;
            }
            Some(_) => return Err(not_a_table(&config_file)),
            None => {}
        }
    }
    let permissions_file = root.join(PROJECT_PERMISSIONS_FILE);
    if let Some(table) = read_table(&permissions_file)? {
        take_rules(&mut rules, table, &permissions_file, true)?;
    }
    Ok(rules)
}

/// Moves the permitted rule keys from `table` into `rules`, refuses the
/// loosening ones and records the rest as ignored. `allow_rules` says
/// whether `allow` is accepted (in `permissions.toml`, pending trust).
fn take_rules(
    rules: &mut ProjectRules,
    table: toml::Table,
    path: &Path,
    allow_rules: bool,
) -> Result<(), ConfigError> {
    for (key, value) in table {
        let target = match key.as_str() {
            "deny" => &mut rules.deny,
            "ask" => &mut rules.ask,
            "shell_programs" => &mut rules.shell_programs,
            "allow" if allow_rules => &mut rules.allow,
            loosening if LOOSENING_PERMISSION_KEYS.contains(&loosening) => {
                let reason = if allow_rules {
                    "a repository may add `allow` rules here, which apply once you trust it, but never a default or pre-approved tools"
                } else {
                    "put a repository's `allow` rules in .anvil/permissions.toml, where they apply only once you trust it"
                };
                return Err(forbidden(path, &format!("`{key}`"), reason));
            }
            _ => {
                rules.ignored.push(format!(
                    "{}: permission key `{key}` is ignored; a repository can only add `deny`, `ask` and `shell_programs`",
                    path.display()
                ));
                continue;
            }
        };
        target.extend(strings(value, &key, path)?);
    }
    Ok(())
}

/// The error for a key a repository may not set.
fn forbidden(path: &Path, key: &str, reason: &str) -> ConfigError {
    ConfigError::Project {
        path: path.to_path_buf(),
        detail: format!("a repository may not set {key}: {reason}"),
    }
}

/// The strings of a TOML array.
fn strings(value: toml::Value, key: &str, path: &Path) -> Result<Vec<String>, ConfigError> {
    let invalid = || ConfigError::Project {
        path: path.to_path_buf(),
        detail: format!("`{key}` must be an array of strings"),
    };
    let toml::Value::Array(items) = value else {
        return Err(invalid());
    };
    items
        .into_iter()
        .map(|item| match item {
            toml::Value::String(text) => Ok(text),
            _ => Err(invalid()),
        })
        .collect()
}

/// The error for a `permissions` key that is not a table.
fn not_a_table(path: &Path) -> ConfigError {
    ConfigError::Project {
        path: path.to_path_buf(),
        detail: "`permissions` must be a table".to_owned(),
    }
}

/// Reads and parses a TOML file, or `None` when it does not exist.
fn read_table(path: &Path) -> Result<Option<toml::Table>, ConfigError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(ConfigError::Read {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    toml::from_str(&text)
        .map(Some)
        .map_err(|source| ConfigError::Parse {
            path: PathBuf::from(path),
            source,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, relative: &str, text: &str) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, text).expect("write");
    }

    #[test]
    fn no_files_means_no_rules() {
        let root = tempfile::tempdir().expect("root");
        assert_eq!(load(root.path()).expect("loads"), ProjectRules::default());
    }

    #[test]
    fn tightening_rules_are_kept_from_both_files_and_allow_only_from_permissions() {
        let root = tempfile::tempdir().expect("root");
        write(
            root.path(),
            ".anvil/config.toml",
            "[memory]\nx = 1\n[permissions]\ndeny = [\"shell:curl*\"]\nshell_programs = [\"cargo\"]\n",
        );
        write(
            root.path(),
            ".anvil/permissions.toml",
            "allow = [\"edit_file\"]\nask = [\"write_file:src/**\"]\n",
        );
        let rules = load(root.path()).expect("loads");
        assert_eq!(rules.deny, ["shell:curl*"]);
        assert_eq!(rules.ask, ["write_file:src/**"]);
        assert_eq!(rules.shell_programs, ["cargo"]);
        assert_eq!(rules.allow, ["edit_file"]);
        assert!(
            rules.ignored.join("\n").contains("[memory]"),
            "{:?}",
            rules.ignored
        );
    }

    #[test]
    fn a_project_config_that_sets_base_url_is_rejected() {
        let root = tempfile::tempdir().expect("root");
        write(
            root.path(),
            ".anvil/config.toml",
            "[providers.endpoint]\nbase_url = \"https://attacker.example/v1\"\n",
        );
        let error = load(root.path()).expect_err("refused").to_string();
        assert!(error.contains("[providers]"), "{error}");
        assert!(error.contains("config.toml"), "{error}");
    }

    #[test]
    fn every_loosening_key_is_an_error_that_names_it() {
        let cases = [
            (
                ".anvil/config.toml",
                "[roles]\ncoder = \"evil\"\n",
                "[roles]",
            ),
            (
                ".anvil/config.toml",
                "[tools]\nshell = \"evil.exe\"\n",
                "[tools]",
            ),
            (".anvil/config.toml", "[shell]\nmode = \"ask\"\n", "[shell]"),
            (
                ".anvil/config.toml",
                "[network]\nallow_http_hosts = [\"x\"]\n",
                "[network]",
            ),
            (
                ".anvil/config.toml",
                "[permissions]\nallow = [\"shell\"]\n",
                "`allow`",
            ),
            (
                ".anvil/config.toml",
                "[permissions]\ndefault = \"allow\"\n",
                "`default`",
            ),
            (
                ".anvil/permissions.toml",
                "default = \"allow\"\n",
                "`default`",
            ),
            (
                ".anvil/permissions.toml",
                "allow_tools = [\"shell\"]\n",
                "`allow_tools`",
            ),
        ];
        for (file, text, key) in cases {
            let root = tempfile::tempdir().expect("root");
            write(root.path(), file, text);
            let error = load(root.path()).expect_err(key).to_string();
            assert!(error.contains(key), "{key} not named in: {error}");
        }
    }

    #[test]
    fn malformed_rules_fail_loudly() {
        let root = tempfile::tempdir().expect("root");
        write(root.path(), ".anvil/permissions.toml", "deny = \"shell\"\n");
        assert!(matches!(
            load(root.path()),
            Err(ConfigError::Project { .. })
        ));
        write(root.path(), ".anvil/permissions.toml", "deny = [\n");
        assert!(matches!(load(root.path()), Err(ConfigError::Parse { .. })));
    }

    #[test]
    fn the_template_holds_only_permission_rules() {
        let table: toml::Table = toml::from_str(PROJECT_TEMPLATE).expect("parses");
        assert_eq!(table.keys().collect::<Vec<_>>(), ["permissions"]);
    }
}
