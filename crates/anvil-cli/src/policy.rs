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
use anvil_types::config::{LoadedConfig, PermissionRules, Unmatched};
use anvil_types::policy::{self, LoadedPolicy, Policy};

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

/// Deletes session folders older than the policy's `retention_days`.
///
/// Only folders whose name is a session id (26 letters and digits) are
/// touched, so nothing else in the folder can be removed by mistake.
///
/// # Arguments
///
/// * `root` - The sessions folder.
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
        let old = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > keep);
        if is_session && old && entry.path().is_dir() {
            match std::fs::remove_dir_all(entry.path()) {
                Ok(()) => removed += 1,
                Err(error) => tracing::warn!(%error, session = name, "cannot prune session"),
            }
        }
    }
    removed
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

/// Every effective policy setting, one `key = value  (layer)` line each.
///
/// The layer is `policy` when the file sets the key and `default` when it
/// takes the default.
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
    let mut lines = Vec::new();
    for (table, value) in &effective {
        let Some(keys) = value.as_table() else {
            lines.push(format!("{table} = {value}  (policy)"));
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
            lines.push(format!("[{table}] {key} = {value}  ({layer})"));
        }
    }
    lines
}

/// What `anvil policy show` prints: the policy's settings with their layer,
/// then the user's settings the policy bounds, with theirs.
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
    if let Some(rules) = &config.permissions {
        let _ = writeln!(
            out,
            "[permissions] allow = {:?}, ask = {:?}, deny = {:?}  ({layer})",
            rules.allow, rules.ask, rules.deny
        );
    }
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
