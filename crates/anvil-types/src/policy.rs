//! The managed policy: rules an administrator installs and the user cannot
//! change.
//!
//! The file sits at a fixed system path that only an administrator can
//! write ([`system_path`]). No environment variable or flag points anvil at
//! another one. When it exists, it is a floor under the user's and the
//! repository's settings: they can add restrictions, never remove one.
//! When it exists but cannot be read or is invalid, anvil refuses to start.
//! When it does not exist, anvil runs in personal mode.
//!
//! Every key has a default, and the defaults are the restrictive choice:
//! with a policy in place, anything that runs extra programs, contacts
//! anything or skips a prompt is off unless the policy turns it on.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::PermissionRules;

/// The policy file name inside the system folder.
pub const POLICY_FILE: &str = "policy.toml";

/// The system folder that holds the policy, per operating system. These
/// folders are writable only by administrators on a standard install.
///
/// # Returns
///
/// `C:\ProgramData\Anvil` on Windows, `/Library/Application Support/Anvil`
/// on macOS, `/etc/anvil` elsewhere. The Windows path is fixed rather than
/// read from `%ProgramData%`, which a user can change for their own
/// processes.
pub fn system_dir() -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(r"C:\ProgramData\Anvil")
    } else if cfg!(target_os = "macos") {
        PathBuf::from("/Library/Application Support/Anvil")
    } else {
        PathBuf::from("/etc/anvil")
    }
}

/// The one path anvil reads a policy from.
///
/// # Returns
///
/// [`system_dir`] joined with [`POLICY_FILE`].
pub fn system_path() -> PathBuf {
    system_dir().join(POLICY_FILE)
}

/// Why a policy file cannot be used. Each one stops anvil from starting.
#[derive(Debug, Error)]
pub enum PolicyError {
    /// The file exists but cannot be read.
    #[error("the managed policy {path} exists but cannot be read: {source}")]
    Read {
        /// The file.
        path: PathBuf,
        /// The I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The file is not valid TOML for the policy schema.
    #[error("the managed policy {path} is invalid: {source}")]
    Parse {
        /// The file.
        path: PathBuf,
        /// The TOML error.
        #[source]
        source: toml::de::Error,
    },
    /// The file parses but breaks a rule of its own.
    #[error("the managed policy {path} is invalid: {detail}")]
    Invalid {
        /// The file.
        path: PathBuf,
        /// What is wrong.
        detail: String,
    },
}

/// The model endpoints anvil may call.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointsPolicy {
    /// The only base URLs requests may go under. Required, at least one.
    pub allowed_base_urls: Vec<String>,
    /// Hosts that may be called over plain http; none by default.
    #[serde(default)]
    pub allow_http_hosts: Vec<String>,
}

/// What the repository may contribute.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ProjectPolicy {
    /// Whether the repository's `.anvil/` is read at all, and whether the
    /// repository can be trusted. `false` ignores its rules and loads
    /// nothing it ships. Default `true`: a repository can still only
    /// tighten, and only a trusted one loads files.
    pub config: bool,
}

impl Default for ProjectPolicy {
    fn default() -> Self {
        Self { config: true }
    }
}

/// How prompts and permissions work.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PermissionsPolicy {
    /// Whether `--yes`, `--allow-tool`, `permissions.allow_tools` and
    /// `--profile` may be used. Default `false`.
    pub allow_skip_prompts: bool,
    /// A profile file in the policy folder whose `[permissions]` table
    /// replaces the user's own rules. The user's `deny`, `ask` and
    /// `shell_programs` still apply on top.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Whether a plan must be approved by a person even under `--yes`.
    /// In `anvil build` this covers the top-level build plan only; the
    /// per-item sessions approve their own plans. Default `false`; without
    /// `allow_skip_prompts` every plan is asked about anyway.
    pub require_plan_approval: bool,
}

/// What may load and run besides the built-in tools.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ExtensionsPolicy {
    /// Whether hooks run, the user's and a trusted repository's. Default `false`.
    pub hooks: bool,
    /// The only MCP server programs that may start, compared with the
    /// definition's `command` exactly (case aside). Default none.
    pub mcp_commands: Vec<String>,
    /// The only folders skills load from. Default none.
    pub skill_dirs: Vec<PathBuf>,
    /// Whether herdr reporting may run. Default `false`.
    pub herdr: bool,
}

/// Where sessions are kept and what runs around them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SessionsPolicy {
    /// Where session folders (transcripts, tool output, status) go, one
    /// subfolder per repository. Default: the repository's `.anvil/sessions`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcript_dir: Option<PathBuf>,
    /// Session folders older than this many days are deleted when a
    /// session starts. Default: kept.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retention_days: Option<u32>,
    /// Whether the automatic summarizer calls run: the rule proposals at
    /// session start and the memory extraction at session end. Default
    /// `false`.
    pub summarizer_calls: bool,
}

/// The whole policy file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    /// The model endpoints anvil may call.
    pub endpoints: EndpointsPolicy,
    /// What the repository may contribute.
    #[serde(default)]
    pub project: ProjectPolicy,
    /// How prompts and permissions work.
    #[serde(default)]
    pub permissions: PermissionsPolicy,
    /// What may load and run besides the built-in tools.
    #[serde(default)]
    pub extensions: ExtensionsPolicy,
    /// Where sessions are kept and what runs around them.
    #[serde(default)]
    pub sessions: SessionsPolicy,
}

/// A policy read from disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedPolicy {
    /// The policy.
    pub policy: Policy,
    /// Where it was read from.
    pub path: PathBuf,
    /// The `[permissions]` table of the forced profile, when one is named.
    pub profile_rules: Option<PermissionRules>,
}

impl LoadedPolicy {
    /// The folder a repository's sessions go in.
    ///
    /// # Arguments
    ///
    /// * `project` - The repository root.
    ///
    /// # Returns
    ///
    /// `<transcript_dir>/<repository key>` when the policy moves
    /// transcripts, otherwise `None`.
    pub fn sessions_root(&self, project: &Path) -> Option<PathBuf> {
        let dir = self.policy.sessions.transcript_dir.as_ref()?;
        let name: String = crate::trust::repo_key(project)
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        Some(dir.join(name.trim_matches('-')))
    }

    /// What the policy turns off, one plain sentence each, for `anvil doctor`.
    ///
    /// # Returns
    ///
    /// The sentences.
    pub fn disables(&self) -> Vec<String> {
        let p = &self.policy;
        let mut lines = vec![format!(
            "model endpoints other than: {}",
            p.endpoints.allowed_base_urls.join(", ")
        )];
        if !p.project.config {
            lines.push("the repository's .anvil/ and everything the repository ships".to_owned());
        }
        if !p.permissions.allow_skip_prompts {
            lines.push("--yes, --allow-tool, permissions.allow_tools and --profile".to_owned());
        }
        if let Some(profile) = &p.permissions.profile {
            lines.push(format!(
                "your own allow rules and default (replaced by {profile})"
            ));
        }
        if p.permissions.require_plan_approval {
            lines.push("approving plans without a person".to_owned());
        }
        if !p.extensions.hooks {
            lines.push("hooks".to_owned());
        }
        lines.push(if p.extensions.mcp_commands.is_empty() {
            "MCP servers".to_owned()
        } else {
            format!(
                "MCP servers other than: {}",
                p.extensions.mcp_commands.join(", ")
            )
        });
        lines.push(if p.extensions.skill_dirs.is_empty() {
            "skills".to_owned()
        } else {
            format!(
                "skills outside: {}",
                p.extensions
                    .skill_dirs
                    .iter()
                    .map(|dir| dir.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        });
        if !p.extensions.herdr {
            lines.push("herdr reporting".to_owned());
        }
        if !p.sessions.summarizer_calls {
            lines.push("the automatic summarizer calls at session start and end".to_owned());
        }
        lines
    }
}

/// Reads and checks the policy at `path`.
///
/// Production code passes [`system_path`]; only tests pass another path.
///
/// # Arguments
///
/// * `path` - The policy file.
///
/// # Returns
///
/// The policy, or `None` when the file does not exist (personal mode).
///
/// # Errors
///
/// A [`PolicyError`] when the file exists but cannot be read, does not
/// parse, names no endpoint, or names a profile that is not a plain file
/// name in the same folder or cannot be read.
pub fn load(path: &Path) -> Result<Option<LoadedPolicy>, PolicyError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(PolicyError::Read {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let policy: Policy = toml::from_str(&text).map_err(|source| PolicyError::Parse {
        path: path.to_path_buf(),
        source,
    })?;
    let invalid = |detail: String| PolicyError::Invalid {
        path: path.to_path_buf(),
        detail,
    };
    if policy.endpoints.allowed_base_urls.is_empty() {
        return Err(invalid(
            "[endpoints] allowed_base_urls must name at least one endpoint".to_owned(),
        ));
    }
    for url in &policy.endpoints.allowed_base_urls {
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            return Err(invalid(format!(
                "`{url}` in allowed_base_urls is not an http or https URL"
            )));
        }
    }
    let profile_rules = match &policy.permissions.profile {
        Some(name) => Some(load_profile_rules(path, name).map_err(invalid)?),
        None => None,
    };
    Ok(Some(LoadedPolicy {
        policy,
        path: path.to_path_buf(),
        profile_rules,
    }))
}

/// Reads the `[permissions]` table of a profile that sits next to the policy.
fn load_profile_rules(policy_path: &Path, name: &str) -> Result<PermissionRules, String> {
    if name.contains(['/', '\\']) || name.contains("..") {
        return Err(format!(
            "profile `{name}` must be a file name in the policy's own folder"
        ));
    }
    let file = policy_path
        .parent()
        .map_or_else(|| PathBuf::from(name), |dir| dir.join(name));
    let text = std::fs::read_to_string(&file)
        .map_err(|error| format!("profile {} cannot be read: {error}", file.display()))?;
    let mut table: toml::Table = toml::from_str(&text)
        .map_err(|error| format!("profile {} is not TOML: {error}", file.display()))?;
    let permissions = table
        .remove("permissions")
        .ok_or_else(|| format!("profile {} has no [permissions] table", file.display()))?;
    permissions
        .try_into()
        .map_err(|error| format!("profile {} [permissions]: {error}", file.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = "[endpoints]\nallowed_base_urls = [\"https://models.corp.example/v1\"]\n";

    fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, text).expect("write");
        path
    }

    #[test]
    fn no_file_is_personal_mode() {
        let dir = tempfile::tempdir().expect("dir");
        assert_eq!(load(&dir.path().join(POLICY_FILE)).expect("loads"), None);
    }

    #[test]
    fn a_minimal_policy_turns_everything_optional_off() {
        let dir = tempfile::tempdir().expect("dir");
        let path = write(dir.path(), POLICY_FILE, MINIMAL);
        let loaded = load(&path).expect("loads").expect("present");
        let p = &loaded.policy;
        assert!(p.project.config);
        assert!(!p.permissions.allow_skip_prompts);
        assert!(!p.extensions.hooks);
        assert!(p.extensions.mcp_commands.is_empty());
        assert!(p.extensions.skill_dirs.is_empty());
        assert!(!p.extensions.herdr);
        assert!(!p.sessions.summarizer_calls);
        assert_eq!(loaded.sessions_root(dir.path()), None);
        let disabled = loaded.disables().join("\n");
        for word in ["--yes", "hooks", "MCP servers", "skills", "summarizer"] {
            assert!(disabled.contains(word), "{word} in:\n{disabled}");
        }
    }

    #[test]
    fn broken_policies_are_errors() {
        let dir = tempfile::tempdir().expect("dir");
        for (text, kind) in [
            ("endpoints = [", "parse"),
            ("[endpoints]\nallowed_base_urls = []\n", "invalid"),
            (
                "[endpoints]\nallowed_base_urls = [\"models.corp\"]\n",
                "invalid",
            ),
            (&format!("{MINIMAL}[unknown]\nx = 1\n") as &str, "parse"),
            (
                &format!("{MINIMAL}[permissions]\nprofile = \"../x.toml\"\n"),
                "invalid",
            ),
            (
                &format!("{MINIMAL}[permissions]\nprofile = \"missing.toml\"\n"),
                "invalid",
            ),
        ] {
            let path = write(dir.path(), POLICY_FILE, text);
            let error = load(&path).expect_err(text);
            let matches = match kind {
                "parse" => matches!(error, PolicyError::Parse { .. }),
                _ => matches!(error, PolicyError::Invalid { .. }),
            };
            assert!(matches, "{text}: {error}");
        }
    }

    #[test]
    fn a_forced_profile_is_read_from_the_policy_folder() {
        let dir = tempfile::tempdir().expect("dir");
        write(
            dir.path(),
            "strict.toml",
            "[permissions]\ndefault = \"ask\"\nshell_programs = [\"cargo\"]\n",
        );
        let path = write(
            dir.path(),
            POLICY_FILE,
            &format!("{MINIMAL}[permissions]\nprofile = \"strict.toml\"\n"),
        );
        let loaded = load(&path).expect("loads").expect("present");
        let rules = loaded.profile_rules.expect("profile rules");
        assert_eq!(rules.shell_programs, ["cargo"]);
    }

    #[test]
    fn transcripts_move_to_one_folder_per_repository() {
        let dir = tempfile::tempdir().expect("dir");
        let path = write(
            dir.path(),
            POLICY_FILE,
            &format!("{MINIMAL}[sessions]\ntranscript_dir = \"/var/anvil\"\n"),
        );
        let loaded = load(&path).expect("loads").expect("present");
        let root = loaded.sessions_root(dir.path()).expect("moved");
        assert!(root.starts_with("/var/anvil"), "{root:?}");
    }

    #[test]
    fn the_system_path_is_fixed_per_system() {
        let path = system_path();
        assert!(path.ends_with(POLICY_FILE));
        assert!(path.is_absolute());
    }

    #[test]
    fn the_example_policy_loads_with_its_profile() {
        let dir = tempfile::tempdir().expect("dir");
        let path = write(
            dir.path(),
            POLICY_FILE,
            include_str!("../../../docs/enterprise/policy.example.toml"),
        );
        write(
            dir.path(),
            "strict.toml",
            include_str!("../../../profiles/strict.toml"),
        );
        let loaded = load(&path).expect("loads").expect("present");
        assert_eq!(loaded.policy.sessions.retention_days, Some(30));
        assert!(loaded.profile_rules.is_some());
    }
}
