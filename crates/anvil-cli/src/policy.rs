//! Applying the managed policy at startup: loading it from the system path
//! after checking who can change it, refusing flags and configuration that
//! conflict with it, placing and pruning session folders, and the policy
//! commands (`anvil policy show`, `anvil policy check`).
//!
//! What the policy changes inside a session (egress, hooks, MCP servers,
//! skills, rules, features, files, limits) is applied in one place,
//! [`crate::wiring::Wiring::with_policy`].

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anvil_core::context::project_sessions_root;
use anvil_core::net::host_port;
use anvil_types::AnvilConfig;
use anvil_types::config::{LoadedConfig, PermissionRules, Unmatched};
use anvil_types::policy::{self, LoadedPolicy, Policy, ShellPolicy};
use anvil_types::shell::{CommandPattern, ShellMode, split_command};

/// Loads the policy from the one system path, after checking that only an
/// administrator can change it. A file that exists but cannot be used
/// stops anvil.
///
/// # Returns
///
/// The policy, or `None` when unmanaged.
///
/// # Errors
///
/// When the file exists but is not safely owned, cannot be read or is invalid.
pub fn load_system() -> anyhow::Result<Option<LoadedPolicy>> {
    load_checked_from(&policy::system_path())
}

/// [`load_system`] for a given path, so a test can show that an unsafe
/// file stops anvil.
///
/// # Arguments
///
/// * `path` - The policy file.
///
/// # Returns
///
/// The policy, or `None` when the file does not exist.
///
/// # Errors
///
/// As [`load_system`]; the message says that anvil does not start.
pub fn load_checked_from(path: &Path) -> anyhow::Result<Option<LoadedPolicy>> {
    policy::load_checked(path).map_err(stop)
}

/// Loads a policy without the ownership check, for tests.
///
/// # Arguments
///
/// * `path` - The policy file.
///
/// # Returns
///
/// The policy, or `None` when the file does not exist.
///
/// # Errors
///
/// When the file cannot be read or is invalid.
#[cfg(test)]
pub fn load_from(path: &Path) -> anyhow::Result<Option<LoadedPolicy>> {
    policy::load(path).map_err(stop)
}

/// The startup error for a policy that cannot be used.
fn stop(error: policy::PolicyError) -> anyhow::Error {
    anyhow::anyhow!("{error}; anvil does not start until an administrator fixes or removes it")
}

/// The error for a lower layer that conflicts with the policy.
///
/// # Arguments
///
/// * `key` - The policy key, such as `[features] hooks = false`.
/// * `what` - What conflicts with it and where it is set.
///
/// # Returns
///
/// An error naming both.
pub fn conflict(key: &str, what: &str) -> anyhow::Error {
    anyhow::anyhow!("the managed policy's {key} forbids {what}")
}

/// The flags of this invocation that skip prompts.
#[derive(Debug, Clone, Copy, Default)]
pub struct SkipFlags {
    /// `--yes`.
    pub yes: bool,
    /// Any `--allow-tool`.
    pub allow_tool: bool,
}

/// Refuses the flags the policy disables.
///
/// # Arguments
///
/// * `policy` - The policy, if any.
/// * `flags` - What was passed.
///
/// # Errors
///
/// Naming the first disabled flag and the policy key that disables it.
pub fn check_flags(policy: Option<&LoadedPolicy>, flags: SkipFlags) -> anyhow::Result<()> {
    let Some(loaded) = policy else {
        return Ok(());
    };
    let p = &loaded.policy.permissions;
    if flags.yes && !p.allow_yes {
        return Err(conflict(
            "[permissions] allow_yes = false",
            "--yes on the command line",
        ));
    }
    if flags.allow_tool && !p.allow_allow_tool {
        return Err(conflict(
            "[permissions] allow_allow_tool = false",
            "--allow-tool on the command line",
        ));
    }
    Ok(())
}

/// The tool and pattern of a rule (`tool` or `tool:glob`).
fn split_rule(rule: &str) -> (&str, Option<&str>) {
    match rule.split_once(':') {
        Some((tool, pattern)) => (tool, Some(pattern)),
        None => (rule, None),
    }
}

/// Whether an `allow` rule asks for something a policy `deny` or `ask`
/// rule covers: the same tool, and the policy rule either names no pattern
/// or the same one.
///
/// # Arguments
///
/// * `allow` - The lower layer's allow rule.
/// * `policy_rule` - The policy's deny or ask rule.
///
/// # Returns
///
/// `true` when they conflict.
pub fn rule_conflicts(allow: &str, policy_rule: &str) -> bool {
    let (allow_tool, allow_pattern) = split_rule(allow.trim());
    let (policy_tool, policy_pattern) = split_rule(policy_rule.trim());
    allow_tool.eq_ignore_ascii_case(policy_tool)
        && (policy_pattern.is_none() || policy_pattern == allow_pattern)
}

/// Checks allow rules from one source against the policy's deny and ask rules.
///
/// # Arguments
///
/// * `policy` - The policy.
/// * `allows` - The source's allow rules.
/// * `source` - Where they are set, for the message.
///
/// # Errors
///
/// Naming the first conflicting pair.
pub fn check_allow_rules(policy: &Policy, allows: &[String], source: &str) -> anyhow::Result<()> {
    let p = &policy.permissions;
    for allow in allows {
        for (list, rules) in [("deny", &p.deny), ("ask", &p.ask)] {
            if let Some(rule) = rules.iter().find(|rule| rule_conflicts(allow, rule)) {
                return Err(conflict(
                    &format!("[permissions] {list} rule `{rule}`"),
                    &format!("the allow rule `{allow}` in {source}"),
                ));
            }
        }
    }
    Ok(())
}

/// Refuses `default = "allow"` when the policy forbids `--yes`: it
/// approves every call no rule names without a person, which is `--yes`
/// in effect.
///
/// # Arguments
///
/// * `policy` - The policy.
/// * `rules` - The source's permission rules.
/// * `source` - Where they are set, for the message.
///
/// # Errors
///
/// Naming `[permissions] allow_yes = false` and the source.
pub fn check_unmatched(
    policy: &Policy,
    rules: &PermissionRules,
    source: &str,
) -> anyhow::Result<()> {
    if rules.unmatched == Some(Unmatched::Allow) && !policy.permissions.allow_yes {
        return Err(conflict(
            "[permissions] allow_yes = false",
            &format!(
                "[permissions] default = \"allow\" in {source}; it approves every unnamed call without a person"
            ),
        ));
    }
    Ok(())
}

/// Where a configuration was read from, for messages.
fn config_label(loaded: &LoadedConfig) -> String {
    if loaded.sources.is_empty() {
        "your configuration".to_owned()
    } else {
        let files: Vec<String> = loaded
            .sources
            .iter()
            .map(|path| path.display().to_string())
            .collect();
        format!("your configuration ({})", files.join(", "))
    }
}

/// Whether the merged configuration sets `[table] key` itself, rather than
/// taking the default.
fn sets(loaded: &LoadedConfig, table: &str, key: &str) -> bool {
    loaded
        .merged
        .get(table)
        .and_then(|value| value.as_table())
        .is_some_and(|table| table.contains_key(key))
}

/// Refuses a user configuration (with its profile) that conflicts with the
/// policy. Run before anything is built from it.
///
/// # Arguments
///
/// * `policy` - The policy.
/// * `loaded` - The merged user configuration and profile.
///
/// # Errors
///
/// Naming the policy key and the setting that conflicts with it.
pub fn check_config(policy: &LoadedPolicy, loaded: &LoadedConfig) -> anyhow::Result<()> {
    let p = &policy.policy;
    let config = &loaded.config;
    let source = config_label(loaded);
    for (name, provider) in &config.providers {
        if !p.models.providers.is_empty() && !p.models.providers.iter().any(|n| n == name) {
            return Err(conflict(
                "[models] providers",
                &format!("the provider `{name}` in {source}"),
            ));
        }
        if !p.models.models.is_empty() && !p.models.models.contains(&provider.model) {
            return Err(conflict(
                "[models] models",
                &format!(
                    "the model `{}` of provider `{name}` in {source}",
                    provider.model
                ),
            ));
        }
        let pair = host_port(&provider.base_url)?;
        if !policy.allows_host(&pair) {
            return Err(conflict(
                "[network] allowed_hosts",
                &format!("{pair}, the host of provider `{name}` in {source}"),
            ));
        }
    }
    if !config.network.allow_http_hosts.is_empty() {
        return Err(conflict(
            "[network] require_tls",
            &format!("[network] allow_http_hosts in {source}; the policy alone decides plain http"),
        ));
    }
    if !config.tools.extra_dirs.is_empty() {
        return Err(conflict(
            "[files] extra_roots",
            &format!(
                "[tools] extra_dirs in {source}; the policy alone names folders outside the workspace"
            ),
        ));
    }
    check_shell(&p.shell, config, &source)?;
    if let Some(rules) = &config.permissions {
        if !rules.allow_tools.is_empty() && !p.permissions.allow_allow_tool {
            return Err(conflict(
                "[permissions] allow_allow_tool = false",
                &format!("permissions.allow_tools in {source}"),
            ));
        }
        check_allow_rules(p, &rules.allow, &source)?;
        check_unmatched(p, rules, &source)?;
    }
    if config.memory.always_summarize && !p.features.memory {
        return Err(conflict(
            "[features] memory = false",
            &format!("[memory] always_summarize in {source}"),
        ));
    }
    if let Some(ceiling) = p.limits.session_tokens
        && sets(loaded, "limits", "session_tokens")
        && config.limits.session_tokens > ceiling
    {
        return Err(conflict(
            &format!("[limits] session_tokens = {ceiling}"),
            &format!(
                "[limits] session_tokens = {} in {source}",
                config.limits.session_tokens
            ),
        ));
    }
    Ok(())
}

/// Refuses user `[shell]` settings and a `[tools] check` command that the
/// policy's `[shell]` table forbids: a looser mode, another executor, a
/// container of the user's own, and a check command that the shell mode
/// would not let run.
///
/// # Arguments
///
/// * `policy` - The policy's `[shell]` table.
/// * `config` - The user's configuration.
/// * `source` - Where the configuration was read from, for messages.
///
/// # Errors
///
/// Naming the policy key and the setting that conflicts with it.
fn check_shell(policy: &ShellPolicy, config: &AnvilConfig, source: &str) -> anyhow::Result<()> {
    let user = &config.shell;
    let mode_key = format!("[shell] mode = \"{}\"", policy.mode.as_str());
    if let Some(mode) = user.mode
        && mode.strictness() < policy.mode.strictness()
    {
        return Err(conflict(
            &mode_key,
            &format!("[shell] mode = \"{}\" in {source}", mode.as_str()),
        ));
    }
    if let Some(executor) = user.executor
        && executor != policy.executor
    {
        return Err(conflict(
            &format!("[shell] executor = \"{}\"", policy.executor.as_str()),
            &format!(
                "[shell] executor = \"{}\" in {source}; the policy alone decides where commands run",
                executor.as_str()
            ),
        ));
    }
    if user.container.is_some() {
        return Err(conflict(
            &format!("[shell] executor = \"{}\"", policy.executor.as_str()),
            &format!("[shell.container] in {source}; the policy alone names the container"),
        ));
    }
    let Some(check) = &config.tools.check else {
        return Ok(());
    };
    match policy.mode {
        ShellMode::Off => Err(conflict(
            &mode_key,
            &format!("[tools] check = \"{check}\" in {source}, which runs a command"),
        )),
        ShellMode::Allowlist => {
            let allowed = split_command(check).is_ok_and(|argv| {
                policy.allow.iter().any(|pattern| {
                    CommandPattern::parse(pattern).is_ok_and(|pattern| pattern.matches(&argv))
                })
            });
            if allowed {
                Ok(())
            } else {
                Err(conflict(
                    "[shell] allow",
                    &format!("[tools] check = \"{check}\" in {source}, which no pattern allows"),
                ))
            }
        }
        ShellMode::Ask => Ok(()),
    }
}

/// Refuses `anvil build` when the shell is off: every work item is done
/// only when its acceptance command has run.
///
/// # Arguments
///
/// * `policy` - The policy, if any.
/// * `shell` - The shell settings in force.
///
/// # Errors
///
/// Naming the policy key, or the user's own setting when unmanaged.
pub fn check_build_shell(
    policy: Option<&LoadedPolicy>,
    shell: &crate::wiring::ShellSettings,
) -> anyhow::Result<()> {
    if shell.mode != ShellMode::Off {
        return Ok(());
    }
    if policy.is_some_and(|loaded| loaded.policy.shell.mode == ShellMode::Off) {
        return Err(conflict(
            "[shell] mode = \"off\"",
            "anvil build, which runs each work item's acceptance command",
        ));
    }
    Err(anyhow::anyhow!(
        "anvil build runs each work item's acceptance command, and [shell] mode = \"off\" in your configuration turns the shell off"
    ))
}

/// The token and dollar caps of an `anvil build`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct BuildCaps {
    /// `--budget-tokens`.
    pub tokens: Option<u64>,
    /// `--budget-usd`.
    pub usd: Option<f64>,
}

/// Applies the policy to `anvil build`: refuses it when `[features] build`
/// is off, refuses caps above the ceilings, and fills in a ceiling as the
/// cap when the flag is not given.
///
/// # Arguments
///
/// * `policy` - The policy, if any.
/// * `caps` - The caps from the command line.
///
/// # Returns
///
/// The caps to use.
///
/// # Errors
///
/// Naming the policy key.
pub fn check_build(policy: Option<&LoadedPolicy>, caps: BuildCaps) -> anyhow::Result<BuildCaps> {
    let Some(loaded) = policy else {
        return Ok(caps);
    };
    let p = &loaded.policy;
    if !p.features.build {
        return Err(conflict("[features] build = false", "anvil build"));
    }
    let tokens = match (caps.tokens, p.limits.build_tokens) {
        (Some(asked), Some(ceiling)) if asked > ceiling => {
            return Err(conflict(
                &format!("[limits] build_tokens = {ceiling}"),
                &format!("--budget-tokens {asked}"),
            ));
        }
        (asked, ceiling) => asked.or(ceiling),
    };
    let usd = match (caps.usd, p.limits.build_usd) {
        (Some(asked), Some(ceiling)) if asked > ceiling => {
            return Err(conflict(
                &format!("[limits] build_usd = {ceiling}"),
                &format!("--budget-usd {asked}"),
            ));
        }
        (asked, ceiling) => asked.or(ceiling),
    };
    Ok(BuildCaps { tokens, usd })
}

/// The folder a repository's sessions live in.
///
/// # Arguments
///
/// * `project` - The repository root.
/// * `policy` - The policy, if any.
///
/// # Returns
///
/// The policy's `transcript_dir` subfolder for the repository, or
/// `<project>/.anvil/sessions`.
pub fn sessions_root(project: &Path, policy: Option<&LoadedPolicy>) -> PathBuf {
    policy
        .and_then(|loaded| loaded.sessions_root(project))
        .unwrap_or_else(|| project_sessions_root(project))
}

/// Deletes session (or build) folders older than `days`.
///
/// Only folders whose name is a session id (26 letters and digits) are
/// touched, so nothing else in the folder can be removed by mistake. A
/// folder whose status lock a running process holds is skipped, and a
/// folder's age is that of the newest file anywhere in it, because
/// appending to a log does not change the folder's own time.
///
/// # Arguments
///
/// * `root` - The sessions folder, or `.anvil/builds`.
/// * `days` - The retention.
/// * `now` - The current time.
///
/// # Returns
///
/// How many folders were removed.
pub fn prune_sessions(root: &Path, days: u32, now: SystemTime) -> usize {
    let Ok(entries) = std::fs::read_dir(root) else {
        return 0;
    };
    let keep = Duration::from_secs(u64::from(days) * 24 * 60 * 60);
    let mut removed = 0;
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_session = name.len() == 26 && name.chars().all(|c| c.is_ascii_alphanumeric());
        let path = entry.path();
        let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
        if !is_session || !is_dir {
            continue;
        }
        let old = newest_change(&path)
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > keep);
        if !old {
            continue;
        }
        if !crate::status::lock_is_free(&path) {
            tracing::info!(
                session = name,
                "not pruning a session that is still running"
            );
            continue;
        }
        match std::fs::remove_dir_all(&path) {
            Ok(()) => removed += 1,
            Err(error) => tracing::warn!(%error, session = name, "cannot prune session"),
        }
    }
    removed
}

/// The newest modification time of a folder or anything under it, links
/// not followed.
fn newest_change(path: &Path) -> Option<SystemTime> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    let mut newest = meta.modified().ok();
    if meta.is_dir()
        && let Ok(entries) = std::fs::read_dir(path)
    {
        for entry in entries.filter_map(Result::ok) {
            if let Some(time) = newest_change(&entry.path()) {
                newest = Some(newest.map_or(time, |current| current.max(time)));
            }
        }
    }
    newest
}

/// Deletes session folders, and the project's build folders, older than
/// `days`, with the rules of [`prune_sessions`].
///
/// # Arguments
///
/// * `sessions_root` - The sessions folder.
/// * `project` - The project root, whose `.anvil/builds` is pruned too.
/// * `days` - The retention.
/// * `now` - The current time.
///
/// # Returns
///
/// How many session folders and how many build folders were removed.
pub fn prune_all(
    sessions_root: &Path,
    project: &Path,
    days: u32,
    now: SystemTime,
) -> (usize, usize) {
    (
        prune_sessions(sessions_root, days, now),
        prune_sessions(&crate::build::state::builds_root(project), days, now),
    )
}

/// The one line every session prints at startup about the policy.
///
/// # Arguments
///
/// * `policy` - The policy, if any.
///
/// # Returns
///
/// `policy: unmanaged` or `policy: managed (<path>, sha256:<hex>)`.
pub fn startup_line(policy: Option<&LoadedPolicy>) -> String {
    match policy {
        None => format!("policy: {}", policy::UNMANAGED),
        Some(loaded) => format!(
            "policy: managed ({}, {})",
            loaded.path.display(),
            policy::label(Some(loaded))
        ),
    }
}

/// Policy keys that may be left unset, with what an unset key means.
const UNSET_POLICY_KEYS: &[(&str, &str, &str)] = &[
    ("data", "transcript_dir", "<project>/.anvil/sessions"),
    ("data", "retention_days", "kept until deleted"),
    ("limits", "session_tokens", "no ceiling"),
    ("limits", "build_tokens", "no ceiling"),
    ("limits", "build_usd", "no ceiling"),
    ("shell", "container", "none; commands run on the host"),
    ("network", "ca_bundle", "the system's trust store"),
    (
        "network",
        "client_cert",
        "none; no client certificate is presented",
    ),
    ("network", "client_key", "none"),
    (
        "audit",
        "mirror_dir",
        "none; records stay in the session folder",
    ),
];

/// Every effective policy setting, one `key = value  (layer)` line each,
/// sorted by table and key.
///
/// The layer is `policy` when the file sets the key and `default` when it
/// takes the default; a key left unset shows what that means, as
/// `(default: ...)`.
///
/// # Arguments
///
/// * `loaded` - The policy.
///
/// # Returns
///
/// The lines.
pub fn policy_lines(loaded: &LoadedPolicy) -> Vec<String> {
    let set_in_file: toml::Table = std::fs::read_to_string(&loaded.path)
        .ok()
        .and_then(|text| toml::from_str(&text).ok())
        .unwrap_or_default();
    let effective = toml::Table::try_from(&loaded.policy).unwrap_or_default();
    let mut lines = std::collections::BTreeMap::new();
    for (table, value) in &effective {
        let Some(keys) = value.as_table() else {
            lines.insert(
                (String::new(), table.clone()),
                format!("{table} = {value}  (policy)"),
            );
            continue;
        };
        for (key, value) in keys {
            let layer = if set_in_file
                .get(table)
                .and_then(toml::Value::as_table)
                .is_some_and(|t| t.contains_key(key))
            {
                "policy"
            } else {
                "default"
            };
            lines.insert(
                (table.clone(), key.clone()),
                format!("[{table}] {key} = {value}  ({layer})"),
            );
        }
    }
    for (table, key, meaning) in UNSET_POLICY_KEYS {
        lines
            .entry(((*table).to_owned(), (*key).to_owned()))
            .or_insert_with(|| format!("[{table}] {key} = (default: {meaning})  (default)"));
    }
    lines.into_values().collect()
}

/// A string value as TOML would show it, or nothing when unset.
fn quoted(value: Option<&str>) -> String {
    value
        .map(|value| format!("\"{value}\""))
        .unwrap_or_default()
}

/// One user setting the policy constrains, with its layer: the user's
/// when their configuration or profile sets it, otherwise `default`, shown
/// as `(default: meaning)` when `unset` says what an unset key means.
fn user_line(
    loaded: &LoadedConfig,
    layer: &str,
    (table, key): (&str, &str),
    value: &str,
    unset: Option<&str>,
) -> String {
    match (sets(loaded, table, key), unset) {
        (true, _) => format!("[{table}] {key} = {value}  ({layer})"),
        (false, Some(meaning)) => format!("[{table}] {key} = (default: {meaning})  (default)"),
        (false, None) => format!("[{table}] {key} = {value}  (default)"),
    }
}

/// What `anvil policy show` prints: the policy's settings with their layer,
/// then every user setting the policy constrains, with theirs.
///
/// # Arguments
///
/// * `policy` - The policy, if any.
/// * `loaded` - The merged user configuration, if it loads.
/// * `profile` - The `--profile` name, if any.
///
/// # Returns
///
/// The text.
pub fn show(
    policy: Option<&LoadedPolicy>,
    loaded: Option<&LoadedConfig>,
    profile: Option<&str>,
) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{}", startup_line(policy));
    if let Some(policy) = policy {
        for line in policy_lines(policy) {
            let _ = writeln!(out, "{line}");
        }
    } else {
        let _ = writeln!(
            out,
            "no policy file at {}; every limit below is the user's own",
            policy::system_path().display()
        );
    }
    let Some(loaded) = loaded else {
        let _ = writeln!(out, "user configuration: none loaded");
        return out;
    };
    let layer = match profile {
        Some(name) => format!("{} + profile {name}", config_label(loaded)),
        None => config_label(loaded),
    };
    let _ = writeln!(out, "user settings the policy constrains:");
    let config = &loaded.config;
    for (name, provider) in &config.providers {
        let _ = writeln!(
            out,
            "provider {name} = {} model {}  ({layer})",
            provider.base_url, provider.model
        );
    }
    let roles = &config.roles;
    let _ = writeln!(
        out,
        "roles coder = {}, summarizer = {}  ({layer})",
        roles.coder, roles.summarizer
    );
    for (role, value, meaning) in [
        ("planner", &roles.planner, "the coder"),
        ("worker", &roles.worker, "the coder"),
        ("reviewer", &roles.reviewer, "none"),
    ] {
        let _ = match value {
            Some(provider) => writeln!(out, "roles {role} = {provider}  ({layer})"),
            None => writeln!(out, "roles {role} = (default: {meaning})  (default)"),
        };
    }
    let list = |items: &[String]| format!("{items:?}");
    let shown: Vec<String> = config
        .tools
        .extra_dirs
        .iter()
        .map(|dir| dir.display().to_string())
        .collect();
    let rules = config.permissions.clone().unwrap_or_default();
    let unmatched = match rules.unmatched {
        Some(Unmatched::Allow) => "\"allow\"",
        Some(Unmatched::Ask) => "\"ask\"",
        Some(Unmatched::Deny) => "\"deny\"",
        None => "",
    };
    let lines = [
        (
            ("network", "allow_http_hosts"),
            list(&config.network.allow_http_hosts),
            None,
        ),
        (("tools", "extra_dirs"), list(&shown), None),
        (("permissions", "allow"), list(&rules.allow), None),
        (("permissions", "ask"), list(&rules.ask), None),
        (("permissions", "deny"), list(&rules.deny), None),
        (
            ("permissions", "default"),
            unmatched.to_owned(),
            Some("the tool's own; reads run, writes ask"),
        ),
        (
            ("permissions", "allow_tools"),
            list(&rules.allow_tools),
            None,
        ),
        (
            ("memory", "always_summarize"),
            config.memory.always_summarize.to_string(),
            None,
        ),
        (
            ("tools", "check"),
            quoted(config.tools.check.as_deref()),
            Some("detected from the project's files"),
        ),
        (
            ("shell", "mode"),
            quoted(config.shell.mode.map(ShellMode::as_str)),
            Some("the policy's mode"),
        ),
        (("shell", "allow"), list(&config.shell.allow), None),
        (
            ("shell", "executor"),
            quoted(config.shell.executor.map(|executor| executor.as_str())),
            Some("the policy's executor"),
        ),
        (
            ("shell", "container"),
            config
                .shell
                .container
                .as_ref()
                .map(|container| format!("{{ image = \"{}\" }}", container.image))
                .unwrap_or_default(),
            Some("none; the policy names the container"),
        ),
    ];
    for (key, value, unset) in lines {
        let _ = writeln!(out, "{}", user_line(loaded, &layer, key, &value, unset));
    }
    let ceiling = policy.and_then(|p| p.policy.limits.session_tokens);
    let tokens = ceiling.map_or(config.limits.session_tokens, |c| {
        c.min(config.limits.session_tokens)
    });
    let tokens_layer = if ceiling.is_some_and(|c| c < config.limits.session_tokens) {
        "policy ceiling"
    } else if sets(loaded, "limits", "session_tokens") {
        layer.as_str()
    } else {
        "default"
    };
    let _ = writeln!(out, "[limits] session_tokens = {tokens}  ({tokens_layer})");
    out
}

/// `anvil policy check <file>`: validates a policy for an administrator.
///
/// # Arguments
///
/// * `path` - The file to check.
/// * `ownership` - Whether to check who can change it, as startup does.
///
/// # Returns
///
/// The report and the exit code: 0 when the file is valid, 1 otherwise.
pub fn check_file(path: &Path, ownership: bool) -> (String, i32) {
    if !path.exists() {
        return (format!("error: {} does not exist\n", path.display()), 1);
    }
    let loaded = if ownership {
        policy::load_checked(path)
    } else {
        policy::load(path)
    };
    match loaded {
        Ok(Some(loaded)) => {
            let mut out = format!(
                "ok: {} is a valid version {} policy, {}\n",
                path.display(),
                loaded.policy.version,
                policy::label(Some(&loaded))
            );
            for line in policy_lines(&loaded) {
                let _ = writeln!(out, "  {line}");
            }
            (out, 0)
        }
        Ok(None) => (format!("error: {} does not exist\n", path.display()), 1),
        Err(error) => (format!("error: {error}\n"), 1),
    }
}

/// The lines `anvil doctor` prints about the policy, above its rows.
///
/// # Arguments
///
/// * `policy` - The policy, if any.
///
/// # Returns
///
/// Where it was loaded from and what it sets, or that none is installed.
pub fn report(policy: Option<&LoadedPolicy>) -> Vec<String> {
    let Some(loaded) = policy else {
        return vec![format!(
            "policy: {} (no file at {})",
            policy::UNMANAGED,
            policy::system_path().display()
        )];
    };
    let mut lines = vec![startup_line(Some(loaded))];
    lines.extend(
        policy_lines(loaded)
            .into_iter()
            .map(|line| format!("policy: {line}")),
    );
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = "version = 1\n[network]\nallowed_hosts = [\"models.corp.example:443\"]\n";

    fn policy(dir: &Path, extra: &str) -> LoadedPolicy {
        let path = dir.join("policy.toml");
        std::fs::write(&path, format!("{MINIMAL}{extra}")).expect("write");
        load_from(&path).expect("loads").expect("present")
    }

    #[test]
    fn a_broken_policy_stops_startup_with_the_reason() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("policy.toml");
        std::fs::write(&path, "[network\n").expect("write");
        let error = load_from(&path).expect_err("broken");
        let text = format!("{error:#}");
        assert!(text.contains("does not start"), "{text}");
        assert!(text.contains("policy.toml"), "{text}");
        assert!(
            load_from(&dir.path().join("absent.toml"))
                .expect("ok")
                .is_none()
        );
    }

    #[test]
    fn a_policy_file_with_bad_ownership_stops_anvil() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("policy.toml");
        std::fs::write(&path, MINIMAL).expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if std::fs::metadata(&path).expect("meta").uid() == 0 {
                return;
            }
        }
        let error = load_checked_from(&path).expect_err("a user-owned policy is refused");
        let text = format!("{error:#}");
        assert!(text.contains("not safely owned"), "{text}");
        assert!(text.contains("does not start"), "{text}");
        assert!(
            load_checked_from(&dir.path().join("absent.toml"))
                .expect("ok")
                .is_none()
        );
    }

    #[test]
    fn skip_flags_are_refused_with_the_policy_key() {
        let dir = tempfile::tempdir().expect("dir");
        let strict = policy(dir.path(), "");
        let yes = SkipFlags {
            yes: true,
            ..SkipFlags::default()
        };
        let tool = SkipFlags {
            allow_tool: true,
            ..SkipFlags::default()
        };
        let error = check_flags(Some(&strict), yes).expect_err("refused");
        assert!(
            error.to_string().contains("[permissions] allow_yes"),
            "{error}"
        );
        let error = check_flags(Some(&strict), tool).expect_err("refused");
        assert!(error.to_string().contains("allow_allow_tool"), "{error}");
        assert!(check_flags(Some(&strict), SkipFlags::default()).is_ok());
        let open = policy(
            dir.path(),
            "[permissions]\nallow_yes = true\nallow_allow_tool = true\n",
        );
        let all = SkipFlags {
            yes: true,
            allow_tool: true,
        };
        assert!(check_flags(Some(&open), all).is_ok());
        assert!(check_flags(None, all).is_ok(), "unmanaged");
    }

    #[test]
    fn allow_rules_conflict_with_a_covering_deny_or_ask() {
        assert!(rule_conflicts("shell", "shell"));
        assert!(rule_conflicts("shell:cargo *", "shell"));
        assert!(rule_conflicts("shell:cargo *", "shell:cargo *"));
        assert!(rule_conflicts("SHELL", "shell"));
        assert!(!rule_conflicts("shell:cargo *", "shell:curl *"));
        assert!(!rule_conflicts("write_file", "shell"));
    }

    #[test]
    fn build_follows_the_feature_switch_and_the_ceilings() {
        let dir = tempfile::tempdir().expect("dir");
        let off = policy(dir.path(), "");
        let error = check_build(Some(&off), BuildCaps::default()).expect_err("off");
        assert!(error.to_string().contains("[features] build"), "{error}");
        let on = policy(
            dir.path(),
            "[features]\nbuild = true\n[limits]\nbuild_tokens = 1000\nbuild_usd = 2.0\n",
        );
        let filled = check_build(Some(&on), BuildCaps::default()).expect("allowed");
        assert_eq!(
            filled,
            BuildCaps {
                tokens: Some(1000),
                usd: Some(2.0)
            }
        );
        let error = check_build(
            Some(&on),
            BuildCaps {
                tokens: Some(5000),
                usd: None,
            },
        )
        .expect_err("above the ceiling");
        assert!(error.to_string().contains("build_tokens"), "{error}");
        let error = check_build(
            Some(&on),
            BuildCaps {
                tokens: None,
                usd: Some(3.0),
            },
        )
        .expect_err("above the ceiling");
        assert!(error.to_string().contains("build_usd"), "{error}");
        assert_eq!(
            check_build(None, BuildCaps::default()).expect("unmanaged"),
            BuildCaps::default()
        );
    }

    #[test]
    fn transcripts_go_where_the_policy_says() {
        let dir = tempfile::tempdir().expect("dir");
        let project = dir.path().join("repo");
        assert_eq!(
            sessions_root(&project, None),
            project.join(".anvil").join("sessions")
        );
        let moved = policy(
            dir.path(),
            &format!(
                "[data]\ntranscript_dir = '{}'\n",
                dir.path().join("transcripts").display()
            ),
        );
        assert!(sessions_root(&project, Some(&moved)).starts_with(dir.path().join("transcripts")));
    }

    #[test]
    fn pruning_removes_only_old_session_folders() {
        let dir = tempfile::tempdir().expect("dir");
        let old = dir.path().join("01ARZ3NDEKTSV4RRFFQ69G5FAV");
        let other = dir.path().join("notes");
        std::fs::create_dir_all(&old).expect("mkdir");
        std::fs::create_dir_all(&other).expect("mkdir");
        let later = SystemTime::now() + Duration::from_secs(40 * 24 * 60 * 60);
        assert_eq!(
            prune_sessions(dir.path(), 30, SystemTime::now()),
            0,
            "still fresh"
        );
        assert_eq!(prune_sessions(dir.path(), 30, later), 1);
        assert!(!old.exists());
        assert!(other.exists(), "a folder that is not a session stays");
    }

    #[test]
    fn pruning_skips_a_session_whose_lock_is_held() {
        let dir = tempfile::tempdir().expect("dir");
        let live = dir.path().join("01ARZ3NDEKTSV4RRFFQ69G5FAV");
        std::fs::create_dir_all(&live).expect("mkdir");
        let lock = std::fs::File::create(live.join(anvil_types::status::LOCK_FILE)).expect("lock");
        lock.lock().expect("locks");
        let later = SystemTime::now() + Duration::from_secs(40 * 24 * 60 * 60);
        assert_eq!(prune_sessions(dir.path(), 30, later), 0);
        assert!(live.is_dir(), "a running session is never pruned");
        drop(lock);
        assert_eq!(prune_sessions(dir.path(), 30, later), 1);
    }

    #[test]
    fn a_session_is_as_old_as_its_newest_file() {
        let dir = tempfile::tempdir().expect("dir");
        let session = dir.path().join("01ARZ3NDEKTSV4RRFFQ69G5FAV");
        std::fs::create_dir_all(session.join("tool-output")).expect("mkdir");
        let old = SystemTime::now() - Duration::from_secs(40 * 24 * 60 * 60);
        let set_time = |path: &Path, time: SystemTime| {
            std::fs::File::options()
                .write(true)
                .open(path)
                .and_then(|file| file.set_modified(time))
                .expect("set time");
        };
        std::fs::write(session.join("session.jsonl"), "{}\n").expect("write");
        std::fs::write(session.join("tool-output").join("c1.txt"), "x").expect("write");
        set_time(&session.join("session.jsonl"), old);
        assert_eq!(
            prune_sessions(dir.path(), 30, SystemTime::now()),
            0,
            "a recent file deep inside keeps the session"
        );
        set_time(&session.join("tool-output").join("c1.txt"), old);
        let later = SystemTime::now() + Duration::from_secs(40 * 24 * 60 * 60);
        assert_eq!(prune_sessions(dir.path(), 30, later), 1);
    }

    #[test]
    fn pruning_covers_the_projects_build_folders() {
        let dir = tempfile::tempdir().expect("dir");
        let sessions = dir.path().join(".anvil").join("sessions");
        let session = sessions.join("01ARZ3NDEKTSV4RRFFQ69G5FAV");
        let build = crate::build::state::builds_root(dir.path()).join("01ARZ3NDEKTSV4RRFFQ69G5FAW");
        std::fs::create_dir_all(&session).expect("mkdir");
        std::fs::create_dir_all(&build).expect("mkdir");
        std::fs::write(build.join("state.json"), "{}").expect("write");
        let later = SystemTime::now() + Duration::from_secs(40 * 24 * 60 * 60);
        assert_eq!(prune_all(&sessions, dir.path(), 30, later), (1, 1));
        assert!(!build.exists());
    }

    #[test]
    fn show_and_report_name_each_setting_and_its_layer() {
        let dir = tempfile::tempdir().expect("dir");
        let loaded = policy(dir.path(), "[features]\nhooks = false\nmemory = true\n");
        let lines = policy_lines(&loaded).join("\n");
        assert!(
            lines.contains("[features] memory = true  (policy)"),
            "{lines}"
        );
        assert!(
            lines.contains("[features] build = false  (default)"),
            "{lines}"
        );
        assert!(
            lines.contains("[network] require_tls = true  (default)"),
            "{lines}"
        );
        let text = report(Some(&loaded)).join("\n");
        assert!(text.contains("policy: managed"), "{text}");
        assert!(text.contains("sha256:"), "{text}");
        assert!(report(None)[0].starts_with("policy: unmanaged"));
        assert_eq!(startup_line(None), "policy: unmanaged");
        let shown = show(None, None, None);
        assert!(shown.starts_with("policy: unmanaged"), "{shown}");
    }

    /// The `[table] key` of each `policy_lines` line.
    fn keys_of(lines: &[String]) -> std::collections::BTreeSet<String> {
        lines
            .iter()
            .filter_map(|line| line.split_once(" = ").map(|(key, _)| key.to_owned()))
            .collect()
    }

    #[test]
    fn policy_lines_name_every_key_of_the_schema_set_or_not() {
        let dir = tempfile::tempdir().expect("dir");
        let pki = if cfg!(windows) { r"C:\pki" } else { "/pki" };
        let network = format!(
            "ca_bundle = '{pki}/ca.pem'
client_cert = '{pki}/me.pem'
client_key = '{pki}/me.key'
proxy = \"system\"
"
        );
        let everything = policy(
            dir.path(),
            &(network
                + "[data]
transcript_dir = '/srv/t'
retention_days = 3
[limits]
session_tokens = 1
build_tokens = 1
build_usd = 1.0
[shell]
mode = \"ask\"
executor = \"container\"
[shell.container]
image = \"build:1\"
"),
        );
        let every_key = keys_of(&policy_lines(&everything));
        let minimal = policy(dir.path(), "");
        let lines = policy_lines(&minimal);
        assert_eq!(keys_of(&lines), every_key, "{lines:#?}");
        let text = lines.join(
            "
",
        );
        for unset in [
            "[data] transcript_dir = (default: <project>/.anvil/sessions)  (default)",
            "[data] retention_days = (default: kept until deleted)  (default)",
            "[limits] session_tokens = (default: no ceiling)  (default)",
            "[limits] build_tokens = (default: no ceiling)  (default)",
            "[limits] build_usd = (default: no ceiling)  (default)",
            "[shell] container = (default: none; commands run on the host)  (default)",
            "[network] ca_bundle = (default: the system's trust store)  (default)",
            "[network] client_cert = (default: none; no client certificate is presented)  (default)",
            "[network] proxy = \"none\"  (default)",
            "[audit] required = true  (default)",
            "[audit] record = \"digest\"  (default)",
            "[audit] mirror_dir = (default: none; records stay in the session folder)  (default)",
        ] {
            assert!(
                text.contains(unset),
                "{unset} missing in:
{text}"
            );
        }
    }

    #[test]
    fn policy_show_covers_every_user_key_the_policy_constrains() {
        let dir = tempfile::tempdir().expect("dir");
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join(".anvil")).expect("mkdir");
        std::fs::write(
            home.join(".anvil").join("config.toml"),
            "[providers.corp]
protocol = \"openai\"
base_url = \"https://models.corp.example/v1\"
api_key_env = \"PATH\"
model = \"m\"
context_window = 1000
supports_tools = true
supports_reasoning = false
[roles]
coder = \"corp\"
summarizer = \"corp\"
[permissions]
allow = [\"read_file\"]
[limits]
session_tokens = 4000
",
        )
        .expect("write");
        let loaded = anvil_types::config::load_profile(Some(&home), None).expect("loads");
        let managed = policy(
            dir.path(),
            "[features]
hooks = false
[limits]
session_tokens = 5000
",
        );
        let text = show(Some(&managed), Some(&loaded), None)
            .replace(&dir.path().display().to_string(), "[dir]")
            .replace('\\', "/");
        for key in [
            "[network] allow_http_hosts = []  (default)",
            "[tools] extra_dirs = []  (default)",
            "[permissions] allow = [\"read_file\"]  (your configuration",
            "[permissions] ask = []  (default)",
            "[permissions] deny = []  (default)",
            "[permissions] default = (default: the tool's own; reads run, writes ask)  (default)",
            "[permissions] allow_tools = []  (default)",
            "[memory] always_summarize = false  (default)",
            "[limits] session_tokens = 4000  (your configuration",
            "roles planner = (default: the coder)  (default)",
        ] {
            assert!(
                text.contains(key),
                "{key} missing in:
{text}"
            );
        }
        insta::assert_snapshot!(text);
    }

    #[test]
    fn policy_check_reports_valid_and_invalid_files() {
        let dir = tempfile::tempdir().expect("dir");
        let good = dir.path().join("good.toml");
        std::fs::write(&good, MINIMAL).expect("write");
        let (text, code) = check_file(&good, false);
        assert_eq!(code, 0, "{text}");
        assert!(text.starts_with("ok:"), "{text}");
        let bad = dir.path().join("bad.toml");
        std::fs::write(&bad, format!("{MINIMAL}[extensions]\nhooks = true\n")).expect("write");
        let (text, code) = check_file(&bad, false);
        assert_eq!(code, 1);
        assert!(text.contains("extensions"), "{text}");
        let (_, code) = check_file(&dir.path().join("missing.toml"), false);
        assert_eq!(code, 1);
        if !cfg!(unix) {
            let (text, code) = check_file(&good, true);
            assert_eq!(
                code, 1,
                "a user-owned file fails the ownership check: {text}"
            );
        }
    }
}
