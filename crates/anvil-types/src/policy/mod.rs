//! The managed policy: limits an administrator installs and nothing below
//! it can loosen.
//!
//! The file sits at a fixed system path ([`system_path`]). No environment
//! variable, flag, profile or configuration key moves it or turns it off;
//! tests pass another path to [`load`] as a function argument. Before it
//! is read, [`ownership::check`] makes sure only an administrator can
//! change it. When it exists, it outranks the user's configuration, a
//! profile, environment variables, command-line flags and the repository:
//! a lower layer that conflicts with it is an error naming the policy key.
//! When it exists but cannot be read, is not safely owned, does not parse,
//! or has an unknown key, anvil refuses to run. When it does not exist,
//! anvil runs unmanaged.
//!
//! Every key has a default, and the defaults are the restrictive choice:
//! with a policy in place, anything that runs extra programs, keeps data
//! or skips a prompt is off unless the policy turns it on.

pub mod ownership;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::shell::{ContainerSettings, Executor, ShellMode, validate_shell};

/// The policy file name inside the system folder.
pub const POLICY_FILE: &str = "policy.toml";

/// The one schema version this build understands.
pub const POLICY_VERSION: u32 = 1;

/// What audit records and `anvil doctor` say when no policy is installed.
pub const UNMANAGED: &str = "unmanaged";

/// The system folder that holds the policy.
///
/// # Returns
///
/// `C:\ProgramData\anvil` on Windows and `/etc/anvil` everywhere else.
/// The Windows path is a fixed literal rather than `%ProgramData%`, which
/// a user can change for their own processes.
pub fn system_dir() -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(r"C:\ProgramData\anvil")
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

/// Why a policy file cannot be used. Each one stops anvil.
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
    /// The file or its folder could be changed by someone other than an
    /// administrator.
    #[error("the managed policy {path} is not safely owned: {detail}")]
    Unsafe {
        /// The file.
        path: PathBuf,
        /// Which check failed.
        detail: String,
    },
    /// The file is not valid TOML for the policy schema, or has an unknown key.
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

/// `[network]`: where requests may go and how the connection is made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkPolicy {
    /// The only `host:port` pairs a provider's `base_url` or an HTTP MCP
    /// server's `url` may name, compared exactly (host without regard to
    /// case). Required, at least one.
    pub allowed_hosts: Vec<String>,
    /// Whether every request must use https. Default `true`; plain http is
    /// then accepted only to a loopback address that is also allowed.
    #[serde(default = "yes")]
    pub require_tls: bool,
    /// A PEM file of certificate authorities. When set, server certificates
    /// are verified against these alone instead of the system's store.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_bundle: Option<PathBuf>,
    /// A PEM client certificate chain for mutual TLS; needs `client_key`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_cert: Option<PathBuf>,
    /// The PEM private key of `client_cert`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_key: Option<PathBuf>,
    /// Which proxy requests go through. Default `none`.
    #[serde(default)]
    pub proxy: ProxyMode,
}

/// Which proxy anvil's HTTP requests go through.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum ProxyMode {
    /// No proxy; proxy variables such as `HTTPS_PROXY` are ignored.
    #[default]
    None,
    /// The proxy the environment variables (`HTTPS_PROXY`, `HTTP_PROXY`,
    /// `ALL_PROXY`, `NO_PROXY`) or the system settings name.
    System,
    /// This proxy, as an `http://` or `https://` URL without credentials.
    Url(String),
}

impl TryFrom<String> for ProxyMode {
    type Error = String;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        match text.as_str() {
            "none" => Ok(Self::None),
            "system" => Ok(Self::System),
            _ => {
                let rest = text
                    .strip_prefix("http://")
                    .or_else(|| text.strip_prefix("https://"))
                    .ok_or_else(|| {
                        format!(
                            "proxy `{text}` is not \"none\", \"system\" or an http:// or https:// URL"
                        )
                    })?;
                let authority = rest.split('/').next().unwrap_or_default();
                if authority.is_empty() {
                    return Err(format!("proxy `{text}` has no host"));
                }
                if authority.contains('@') {
                    return Err(format!(
                        "proxy `{text}` carries credentials, and the policy file is readable by every user"
                    ));
                }
                Ok(Self::Url(text))
            }
        }
    }
}

impl From<ProxyMode> for String {
    fn from(mode: ProxyMode) -> Self {
        match mode {
            ProxyMode::None => "none".to_owned(),
            ProxyMode::System => "system".to_owned(),
            ProxyMode::Url(url) => url,
        }
    }
}

/// `[models]`: which providers and models the configuration may name.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ModelsPolicy {
    /// Allowed provider names (the keys of `[providers]`). Empty: any name.
    pub providers: Vec<String>,
    /// Allowed model names (each provider's `model`). Empty: any model.
    pub models: Vec<String>,
}

/// `[permissions]`: rules and prompt switches nothing can loosen.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PermissionsPolicy {
    /// Rules (`tool` or `tool:glob`) that refuse a call, whatever else allows it.
    pub deny: Vec<String>,
    /// Rules that make a call ask a person, whatever the user allows.
    pub ask: Vec<String>,
    /// Whether `--yes` may be used. Default `false`.
    pub allow_yes: bool,
    /// Whether `--allow-tool` and `permissions.allow_tools` may be used.
    /// Default `false`.
    pub allow_allow_tool: bool,
    /// Whether the `always` prompt answer may save a rule, and saved rules
    /// under `~/.anvil/permissions/` are read. Default `false`.
    pub allow_always: bool,
    /// Whether a person must approve every plan, even under `--yes`.
    /// Default `false`.
    pub require_plan_approval: bool,
}

/// `[files]`: what the file tools may open beyond the workspace.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct FilesPolicy {
    /// Folders outside the workspace the file tools may read, never write.
    pub extra_roots: Vec<PathBuf>,
    /// Path globs the file tools refuse and searches skip, on top of the
    /// built-in list of secret files.
    pub deny: Vec<String>,
}

/// Which MCP servers may start.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpMode {
    /// None.
    #[default]
    Off,
    /// Only those listed in `mcp_servers`.
    Allowlist,
    /// Every server the user's files, and a trusted repository's, define.
    On,
}

/// One MCP server a policy allows: its name and either its program (a
/// local server) or its URL (an HTTP server) must match.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpServerRule {
    /// The server name in the definition file.
    pub name: String,
    /// The `command` of a local definition, compared without regard to case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// The `url` of an HTTP definition, compared exactly apart from a
    /// trailing `/`. Its `host:port` must also be in `[network] allowed_hosts`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// Where skills may load from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillsMode {
    /// None.
    #[default]
    Off,
    /// Only the skills that ship with anvil.
    Bundled,
    /// The bundled skills, the user's own and a trusted repository's.
    Local,
}

/// `[features]`: what may run besides the built-in tools.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct FeaturesPolicy {
    /// Which MCP servers may start. Default `off`.
    pub mcp: McpMode,
    /// The servers `mcp = "allowlist"` allows.
    pub mcp_servers: Vec<McpServerRule>,
    /// Whether hooks run. Default `false`.
    pub hooks: bool,
    /// Where skills load from. Default `off`.
    pub skills: SkillsMode,
    /// Whether remembered facts are read and the end-of-session summarizer
    /// proposes new ones; when `false` nothing under `~/.anvil/memory` is
    /// read or written. Default `false`.
    pub memory: bool,
    /// Whether `anvil build` may run. Default `false`.
    pub build: bool,
    /// Whether the `delegate` tool (subagents) is available. Default `false`.
    pub delegation: bool,
}

/// `[projects]`: what a repository may gain from the user's trust.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ProjectsPolicy {
    /// Whether a repository can be trusted at all. When `false`, no
    /// repository's hooks, MCP servers, skills, commands, agents or
    /// instruction files load, whatever the user decided. Its `deny` and
    /// `ask` rules still apply. Default `false`.
    pub allow_trust: bool,
}

/// `[data]`: where session records go and how long they stay.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DataPolicy {
    /// Where session folders go, one subfolder per repository. Default:
    /// the repository's `.anvil/sessions`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcript_dir: Option<PathBuf>,
    /// Session folders older than this many days are deleted when a
    /// session starts. Default: kept.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retention_days: Option<u32>,
}

/// `[limits]`: ceilings on spending.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct LimitsPolicy {
    /// The most `[limits] session_tokens` may be.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_tokens: Option<u64>,
    /// The most `anvil build --budget-tokens` may be; also the cap when the
    /// flag is not given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build_tokens: Option<u64>,
    /// The most `anvil build --budget-usd` may be; also the cap when the
    /// flag is not given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build_usd: Option<f64>,
}

/// `[shell]`: how the `shell` tool may run commands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ShellPolicy {
    /// `off`, `allowlist` or `ask`. Default `off`: the tool is not
    /// registered unless the policy turns it on.
    pub mode: ShellMode,
    /// Argument-prefix patterns for `allowlist` mode, such as `cargo test *`.
    pub allow: Vec<String>,
    /// `host` or `container`. Default `host`.
    pub executor: Executor,
    /// The image and limits for `executor = "container"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<ContainerSettings>,
}

impl Default for ShellPolicy {
    fn default() -> Self {
        Self {
            mode: ShellMode::Off,
            allow: Vec::new(),
            executor: Executor::Host,
            container: None,
        }
    }
}

/// The whole policy file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    /// The schema version; must be [`POLICY_VERSION`].
    pub version: u32,
    /// Where model requests may go.
    pub network: NetworkPolicy,
    /// Which providers and models may be configured.
    #[serde(default)]
    pub models: ModelsPolicy,
    /// Rules and prompt switches.
    #[serde(default)]
    pub permissions: PermissionsPolicy,
    /// What the file tools may open beyond the workspace.
    #[serde(default)]
    pub files: FilesPolicy,
    /// What may run besides the built-in tools.
    #[serde(default)]
    pub features: FeaturesPolicy,
    /// What a repository may gain from trust.
    #[serde(default)]
    pub projects: ProjectsPolicy,
    /// Where session records go and how long they stay.
    #[serde(default)]
    pub data: DataPolicy,
    /// Ceilings on spending.
    #[serde(default)]
    pub limits: LimitsPolicy,
    /// How the `shell` tool may run commands.
    #[serde(default)]
    pub shell: ShellPolicy,
}

/// The serde default for `true` fields.
fn yes() -> bool {
    true
}

/// A policy read from disk.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadedPolicy {
    /// The policy.
    pub policy: Policy,
    /// Where it was read from.
    pub path: PathBuf,
    /// The SHA-256 of the file's bytes, lowercase hex.
    pub sha256: String,
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
        let dir = self.policy.data.transcript_dir.as_ref()?;
        let name: String = crate::trust::repo_key(project)
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        Some(dir.join(name.trim_matches('-')))
    }

    /// Whether `host_port` (`host:port`, host in lowercase) is allowed.
    ///
    /// # Arguments
    ///
    /// * `host_port` - The pair to look up.
    ///
    /// # Returns
    ///
    /// `true` when `[network] allowed_hosts` lists it.
    pub fn allows_host(&self, host_port: &str) -> bool {
        self.policy
            .network
            .allowed_hosts
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(host_port))
    }

    /// Whether an MCP server may start under `[features] mcp`.
    ///
    /// # Arguments
    ///
    /// * `name` - The server name.
    /// * `command` - The program its definition starts.
    ///
    /// # Returns
    ///
    /// `true` when the mode is `on`, or `allowlist` with a rule naming
    /// both the server and its program.
    pub fn allows_mcp_server(&self, name: &str, command: &str) -> bool {
        let features = &self.policy.features;
        match features.mcp {
            McpMode::Off => false,
            McpMode::On => true,
            McpMode::Allowlist => features.mcp_servers.iter().any(|rule| {
                rule.name == name
                    && rule
                        .command
                        .as_deref()
                        .is_some_and(|c| c.eq_ignore_ascii_case(command))
            }),
        }
    }

    /// Whether an HTTP MCP server may be contacted under `[features] mcp`.
    ///
    /// Its `host:port` must also pass `[network] allowed_hosts`, which the
    /// egress check enforces on every request.
    ///
    /// # Arguments
    ///
    /// * `name` - The server name.
    /// * `url` - The `url` of its definition.
    ///
    /// # Returns
    ///
    /// `true` when the mode is `on`, or `allowlist` with a rule naming both
    /// the server and its URL.
    pub fn allows_mcp_url(&self, name: &str, url: &str) -> bool {
        let features = &self.policy.features;
        match features.mcp {
            McpMode::Off => false,
            McpMode::On => true,
            McpMode::Allowlist => features.mcp_servers.iter().any(|rule| {
                rule.name == name
                    && rule
                        .url
                        .as_deref()
                        .is_some_and(|u| u.trim_end_matches('/') == url.trim_end_matches('/'))
            }),
        }
    }
}

/// The label audit records and `anvil doctor` use for the policy in force.
///
/// # Arguments
///
/// * `policy` - The policy, if any.
///
/// # Returns
///
/// `sha256:<hex>` of the policy file, or [`UNMANAGED`].
pub fn label(policy: Option<&LoadedPolicy>) -> String {
    policy.map_or_else(
        || UNMANAGED.to_owned(),
        |loaded| format!("sha256:{}", loaded.sha256),
    )
}

/// Whether the user may trust a repository so that what it ships loads.
///
/// # Arguments
///
/// * `policy` - The policy, if any.
///
/// # Returns
///
/// `true` when unmanaged, otherwise `[projects] allow_trust`.
pub fn allows_project_trust(policy: Option<&LoadedPolicy>) -> bool {
    policy.is_none_or(|loaded| loaded.policy.projects.allow_trust)
}

/// Splits `host:port` into its parts, `[v6]:port` included.
///
/// # Arguments
///
/// * `text` - The pair.
///
/// # Returns
///
/// The host (an IPv6 host keeps its brackets, as a URL prints it) and the
/// port, or `None` when there is no numeric port.
pub fn split_host_port(text: &str) -> Option<(&str, u16)> {
    let (host, port) = text.rsplit_once(':')?;
    let port: u16 = port.parse().ok()?;
    let bracketed = host.starts_with('[') && host.ends_with(']');
    if host.is_empty() || (host.contains(':') && !bracketed) || host.contains('/') {
        return None;
    }
    Some((host, port))
}

/// Parses and checks policy text.
///
/// # Arguments
///
/// * `text` - The file's contents.
/// * `path` - The file, for error messages.
///
/// # Returns
///
/// The policy.
///
/// # Errors
///
/// [`PolicyError::Parse`] for bad TOML or an unknown key, and
/// [`PolicyError::Invalid`] when a value breaks a rule: a wrong version,
/// no allowed host, a host without a numeric port, an allowlist mode
/// without servers, an empty rule, a ceiling of zero, or a `[shell]` table
/// whose keys do not fit together.
pub fn parse(text: &str, path: &Path) -> Result<Policy, PolicyError> {
    let policy: Policy = toml::from_str(text).map_err(|source| PolicyError::Parse {
        path: path.to_path_buf(),
        source,
    })?;
    let invalid = |detail: String| PolicyError::Invalid {
        path: path.to_path_buf(),
        detail,
    };
    if policy.version != POLICY_VERSION {
        return Err(invalid(format!(
            "version = {} is not supported; this anvil reads version = {POLICY_VERSION}",
            policy.version
        )));
    }
    if policy.network.allowed_hosts.is_empty() {
        return Err(invalid(
            "[network] allowed_hosts must name at least one host:port".to_owned(),
        ));
    }
    for host in &policy.network.allowed_hosts {
        if split_host_port(host).is_none() {
            return Err(invalid(format!(
                "`{host}` in [network] allowed_hosts is not host:port with a numeric port"
            )));
        }
    }
    validate_network(&policy.network).map_err(invalid)?;
    let features = &policy.features;
    for rule in &features.mcp_servers {
        let url_ok = rule
            .url
            .as_deref()
            .is_none_or(|url| url.starts_with("https://") || url.starts_with("http://"));
        if rule.command.is_some() == rule.url.is_some() || !url_ok {
            return Err(invalid(format!(
                "[features] mcp_servers entry `{}` needs exactly one of `command` and an http(s) `url`",
                rule.name
            )));
        }
    }
    if features.mcp == McpMode::Allowlist && features.mcp_servers.is_empty() {
        return Err(invalid(
            "[features] mcp = \"allowlist\" needs at least one entry in mcp_servers".to_owned(),
        ));
    }
    if features.mcp != McpMode::Allowlist && !features.mcp_servers.is_empty() {
        return Err(invalid(
            "[features] mcp_servers is only used with mcp = \"allowlist\"".to_owned(),
        ));
    }
    for rule in policy
        .permissions
        .deny
        .iter()
        .chain(&policy.permissions.ask)
    {
        if rule.trim().is_empty() || rule.split(':').next().is_some_and(str::is_empty) {
            return Err(invalid(format!(
                "`{rule}` in [permissions] is not a rule (`tool` or `tool:glob`)"
            )));
        }
    }
    let shell = &policy.shell;
    validate_shell(
        shell.mode,
        &shell.allow,
        shell.executor,
        shell.container.as_ref(),
    )
    .map_err(invalid)?;
    let limits = &policy.limits;
    if limits.session_tokens == Some(0) || limits.build_tokens == Some(0) {
        return Err(invalid("a token ceiling in [limits] is 0".to_owned()));
    }
    if limits
        .build_usd
        .is_some_and(|usd| !(usd > 0.0 && usd.is_finite()))
    {
        return Err(invalid(
            "[limits] build_usd must be a positive number".to_owned(),
        ));
    }
    Ok(policy)
}

/// Checks the `[network]` files: absolute paths, and a client certificate
/// only together with its key.
fn validate_network(network: &NetworkPolicy) -> Result<(), String> {
    for (key, path) in [
        ("ca_bundle", &network.ca_bundle),
        ("client_cert", &network.client_cert),
        ("client_key", &network.client_key),
    ] {
        if let Some(path) = path
            && !path.is_absolute()
        {
            return Err(format!(
                "[network] {key} = {} must be an absolute path",
                path.display()
            ));
        }
    }
    if network.client_cert.is_some() != network.client_key.is_some() {
        return Err("[network] client_cert and client_key must be set together".to_owned());
    }
    Ok(())
}

/// Reads and checks the policy at `path`, without the ownership check.
///
/// Startup calls [`ownership::check`] first; `anvil policy check` and
/// tests call this alone.
///
/// # Arguments
///
/// * `path` - The policy file.
///
/// # Returns
///
/// The policy, or `None` when the file does not exist (unmanaged).
///
/// # Errors
///
/// A [`PolicyError`] when the file exists but cannot be read or
/// [`parse`] refuses it.
pub fn load(path: &Path) -> Result<Option<LoadedPolicy>, PolicyError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(PolicyError::Read {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let text = String::from_utf8(bytes.clone()).map_err(|_| PolicyError::Invalid {
        path: path.to_path_buf(),
        detail: "the file is not UTF-8 text".to_owned(),
    })?;
    let policy = parse(&text, path)?;
    let sha256 = Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok(Some(LoadedPolicy {
        policy,
        path: path.to_path_buf(),
        sha256,
    }))
}

/// Checks who can change the policy at `path`, then reads it.
///
/// # Arguments
///
/// * `path` - The policy file; production passes [`system_path`].
///
/// # Returns
///
/// The policy, or `None` when the file does not exist.
///
/// # Errors
///
/// [`PolicyError::Read`] when the file or its folder cannot be inspected
/// (permission denied, the folder is not a folder), [`PolicyError::Unsafe`]
/// when either could be changed by someone other than an administrator,
/// otherwise as [`load`].
pub fn load_checked(path: &Path) -> Result<Option<LoadedPolicy>, PolicyError> {
    if !is_present(path)? {
        return Ok(None);
    }
    ownership::check(path).map_err(|detail| PolicyError::Unsafe {
        path: path.to_path_buf(),
        detail,
    })?;
    load(path)
}

/// Whether the policy file is there. Only a file that is not found, in a
/// folder that is not found or is a folder, counts as absent: any other
/// error, such as a folder the user may not list, stops anvil instead of
/// running it unmanaged.
fn is_present(path: &Path) -> Result<bool, PolicyError> {
    use std::io::ErrorKind::NotFound;
    let read_error = |source: std::io::Error| PolicyError::Read {
        path: path.to_path_buf(),
        source,
    };
    match std::fs::symlink_metadata(path) {
        Ok(_) => return Ok(true),
        Err(error) if error.kind() == NotFound => {}
        Err(error) => return Err(read_error(error)),
    }
    let Some(folder) = path.parent().filter(|p| !p.as_os_str().is_empty()) else {
        return Ok(false);
    };
    let folder_meta = match std::fs::symlink_metadata(folder) {
        Ok(meta) if meta.file_type().is_symlink() => std::fs::metadata(folder),
        other => other,
    };
    match folder_meta {
        Ok(meta) if meta.is_dir() => Ok(false),
        Err(error) if error.kind() == NotFound => Ok(false),
        Ok(_) => Err(read_error(std::io::Error::other(format!(
            "{} is not a folder",
            folder.display()
        )))),
        Err(error) => Err(read_error(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = "version = 1\n[network]\nallowed_hosts = [\"models.corp.example:443\"]\n";

    fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, text).expect("write");
        path
    }

    #[test]
    fn no_file_is_unmanaged() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join(POLICY_FILE);
        assert_eq!(load(&path).expect("loads"), None);
        assert_eq!(load_checked(&path).expect("loads"), None);
        assert_eq!(label(None), UNMANAGED);
        assert!(allows_project_trust(None));
    }

    #[test]
    fn a_policy_path_that_cannot_be_inspected_stops_anvil() {
        let dir = tempfile::tempdir().expect("dir");
        let file = write(dir.path(), "file.txt", "not a folder");
        let error = load_checked(&file.join(POLICY_FILE))
            .expect_err("a path that cannot be inspected is not an absent policy");
        assert!(matches!(error, PolicyError::Read { .. }), "{error}");
        assert!(error.to_string().contains("cannot be read"), "{error}");
        assert_eq!(
            load_checked(&dir.path().join("absent").join(POLICY_FILE)).expect("absent"),
            None,
            "a missing folder is unmanaged"
        );
    }

    #[test]
    fn a_minimal_policy_turns_everything_optional_off() {
        let dir = tempfile::tempdir().expect("dir");
        let path = write(dir.path(), POLICY_FILE, MINIMAL);
        let loaded = load(&path).expect("loads").expect("present");
        let p = &loaded.policy;
        assert!(p.network.require_tls);
        let perms = &p.permissions;
        assert!(!perms.allow_yes && !perms.allow_allow_tool && !perms.allow_always);
        let f = &p.features;
        assert_eq!(f.mcp, McpMode::Off);
        assert_eq!(f.skills, SkillsMode::Off);
        assert!(!f.hooks && !f.memory && !f.build && !f.delegation);
        assert!(!p.projects.allow_trust);
        assert_eq!(p.shell.mode, ShellMode::Off, "no shell unless turned on");
        assert_eq!(p.shell.executor, Executor::Host);
        assert!(!allows_project_trust(Some(&loaded)));
        assert_eq!(loaded.sessions_root(dir.path()), None);
        assert!(loaded.allows_host("MODELS.corp.example:443"));
        assert!(!loaded.allows_host("models.corp.example:8443"));
        assert!(!loaded.allows_mcp_server("git", "mcp-git"));
        assert_eq!(loaded.sha256.len(), 64);
        assert!(label(Some(&loaded)).starts_with("sha256:"));
    }

    #[test]
    fn broken_policies_are_errors() {
        let dir = tempfile::tempdir().expect("dir");
        let parse_errors = [
            "version = 1\n[network\n".to_owned(),
            "[network]\nallowed_hosts = [\"a:1\"]\n".to_owned(),
            format!("{MINIMAL}[unknown]\nx = 1\n"),
            format!("{MINIMAL}[features]\nhooks = true\nherdr = true\n"),
            format!("{MINIMAL}[features]\nmcp = \"sometimes\"\n"),
            format!("{MINIMAL}[shell]\nmode = \"sometimes\"\n"),
            format!("{MINIMAL}[shell]\nmode = \"ask\"\nsandbox = true\n"),
        ];
        let invalid = [
            MINIMAL.replace("version = 1", "version = 2"),
            "version = 1\n[network]\nallowed_hosts = []\n".to_owned(),
            "version = 1\n[network]\nallowed_hosts = [\"models.corp.example\"]\n".to_owned(),
            "version = 1\n[network]\nallowed_hosts = [\"https://m.example:443\"]\n".to_owned(),
            format!("{MINIMAL}[features]\nmcp = \"allowlist\"\n"),
            format!(
                "{MINIMAL}[features]\nmcp_servers = [{{ name = \"git\", command = \"mcp-git\" }}]\n"
            ),
            format!("{MINIMAL}[permissions]\ndeny = [\":x\"]\n"),
            format!("{MINIMAL}[limits]\nsession_tokens = 0\n"),
            format!("{MINIMAL}[limits]\nbuild_usd = -1.0\n"),
            format!("{MINIMAL}[shell]\nmode = \"allowlist\"\n"),
            format!("{MINIMAL}[shell]\nmode = \"ask\"\nallow = [\"git *\"]\n"),
            format!("{MINIMAL}[shell]\nmode = \"allowlist\"\nallow = [\"git status; id\"]\n"),
            format!("{MINIMAL}[shell]\nmode = \"ask\"\nexecutor = \"container\"\n"),
            format!(
                "{MINIMAL}[shell]\nmode = \"ask\"\nexecutor = \"container\"\n[shell.container]\nimage = \"b:1\"\nuser = \"root\"\n"
            ),
        ];
        for text in &parse_errors {
            let path = write(dir.path(), POLICY_FILE, text);
            let error = load(&path).expect_err(text);
            assert!(
                matches!(error, PolicyError::Parse { .. }),
                "{text}: {error}"
            );
        }
        for text in &invalid {
            let path = write(dir.path(), POLICY_FILE, text);
            let error = load(&path).expect_err(text);
            assert!(
                matches!(error, PolicyError::Invalid { .. }),
                "{text}: {error}"
            );
        }
    }

    #[test]
    fn a_shell_table_with_an_allowlist_and_a_container_parses() {
        let dir = tempfile::tempdir().expect("dir");
        let path = write(
            dir.path(),
            POLICY_FILE,
            &format!(
                "{MINIMAL}[shell]\nmode = \"allowlist\"\nallow = [\"cargo test *\", \"git status\"]\nexecutor = \"container\"\n[shell.container]\nruntime = \"podman\"\nimage = \"registry.corp.example/build@sha256:0123\"\nmemory = \"4g\"\npids_limit = 128\n"
            ),
        );
        let loaded = load(&path).expect("loads").expect("present");
        let shell = &loaded.policy.shell;
        assert_eq!(shell.mode, ShellMode::Allowlist);
        assert_eq!(shell.allow.len(), 2);
        assert_eq!(shell.executor, Executor::Container);
        let container = shell.container.as_ref().expect("container");
        assert_eq!(container.runtime, crate::shell::ContainerRuntime::Podman);
        assert_eq!(container.user, crate::shell::DEFAULT_CONTAINER_USER);
        assert_eq!(container.pids_limit, 128);
    }

    #[test]
    fn host_port_pairs_need_a_numeric_port() {
        assert_eq!(split_host_port("gpu.lan:8000"), Some(("gpu.lan", 8000)));
        assert_eq!(split_host_port("[::1]:8000"), Some(("[::1]", 8000)));
        assert_eq!(split_host_port("gpu.lan"), None);
        assert_eq!(split_host_port("::1:80"), None);
        assert_eq!(split_host_port("gpu.lan:http"), None);
    }

    #[test]
    fn an_mcp_allowlist_needs_name_and_program() {
        let dir = tempfile::tempdir().expect("dir");
        let path = write(
            dir.path(),
            POLICY_FILE,
            &format!(
                "{MINIMAL}[features]\nmcp = \"allowlist\"\nmcp_servers = [{{ name = \"git\", command = \"mcp-git\" }}]\n"
            ),
        );
        let loaded = load(&path).expect("loads").expect("present");
        assert!(loaded.allows_mcp_server("git", "MCP-GIT"));
        assert!(!loaded.allows_mcp_server("git", "node"));
        assert!(!loaded.allows_mcp_server("other", "mcp-git"));
        assert!(
            !loaded.allows_mcp_url("git", "https://mcp-git"),
            "a command rule allows no URL"
        );
    }

    #[test]
    fn an_mcp_allowlist_names_an_http_server_by_its_url() {
        let dir = tempfile::tempdir().expect("dir");
        let path = write(
            dir.path(),
            POLICY_FILE,
            &format!(
                "{MINIMAL}[features]\nmcp = \"allowlist\"\nmcp_servers = [{{ name = \"docs\", url = \"https://mcp.corp.example/mcp\" }}]\n"
            ),
        );
        let loaded = load(&path).expect("loads").expect("present");
        assert!(loaded.allows_mcp_url("docs", "https://mcp.corp.example/mcp/"));
        assert!(!loaded.allows_mcp_url("docs", "https://mcp.corp.example/other"));
        assert!(!loaded.allows_mcp_url("other", "https://mcp.corp.example/mcp"));
        assert!(
            !loaded.allows_mcp_server("docs", "https://mcp.corp.example/mcp"),
            "a URL rule allows no program"
        );
        for bad in [
            "{ name = \"x\" }",
            "{ name = \"x\", command = \"a\", url = \"https://b\" }",
            "{ name = \"x\", url = \"ftp://b\" }",
        ] {
            let text = format!("{MINIMAL}[features]\nmcp = \"allowlist\"\nmcp_servers = [{bad}]\n");
            assert!(
                matches!(parse(&text, &path), Err(PolicyError::Invalid { .. })),
                "{bad}"
            );
        }
    }

    #[test]
    fn the_network_table_takes_tls_files_and_a_proxy_mode() {
        let path = Path::new(POLICY_FILE);
        let root = if cfg!(windows) { r"C:\pki" } else { "/pki" };
        let files = format!(
            "ca_bundle = '{root}/ca.pem'\nclient_cert = '{root}/me.pem'\nclient_key = '{root}/me.key'\n"
        );
        let base = "version = 1\n[network]\nallowed_hosts = [\"m.example:443\"]\n";
        let policy = parse(&format!("{base}{files}proxy = \"system\"\n"), path).expect("parses");
        assert_eq!(policy.network.proxy, ProxyMode::System);
        assert!(policy.network.ca_bundle.is_some() && policy.network.client_key.is_some());
        let defaults = parse(base, path).expect("parses");
        assert_eq!(
            defaults.network.proxy,
            ProxyMode::None,
            "no proxy by default"
        );
        let url =
            parse(&format!("{base}proxy = \"http://proxy.corp:3128\"\n"), path).expect("parses");
        assert_eq!(
            url.network.proxy,
            ProxyMode::Url("http://proxy.corp:3128".to_owned())
        );
        for proxy in [
            "env",
            "socks5://p:1",
            "http://",
            "http://user:pw@proxy.corp:3128",
        ] {
            assert!(
                matches!(
                    parse(&format!("{base}proxy = \"{proxy}\"\n"), path),
                    Err(PolicyError::Parse { .. })
                ),
                "{proxy}"
            );
        }
        for invalid in [
            format!("{base}ca_bundle = 'relative/ca.pem'\n"),
            format!("{base}client_cert = '{root}/me.pem'\n"),
            format!("{base}client_key = '{root}/me.key'\n"),
        ] {
            assert!(
                matches!(parse(&invalid, path), Err(PolicyError::Invalid { .. })),
                "{invalid}"
            );
        }
    }

    #[test]
    fn transcripts_move_to_one_folder_per_repository() {
        let dir = tempfile::tempdir().expect("dir");
        let path = write(
            dir.path(),
            POLICY_FILE,
            &format!("{MINIMAL}[data]\ntranscript_dir = \"/var/anvil\"\n"),
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
        if cfg!(windows) {
            assert_eq!(path, PathBuf::from(r"C:\ProgramData\anvil\policy.toml"));
        } else {
            assert_eq!(path, PathBuf::from("/etc/anvil/policy.toml"));
        }
    }

    #[test]
    fn every_shipped_policy_parses() {
        let dir = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../policies"));
        let mut seen = 0;
        for entry in std::fs::read_dir(&dir).expect("policies/ exists") {
            let path = entry.expect("entry").path();
            if path.extension().is_some_and(|ext| ext == "toml") {
                load(&path)
                    .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
                    .expect("present");
                seen += 1;
            }
        }
        assert!(seen >= 2, "restricted.toml and standard.toml ship");
    }
}
