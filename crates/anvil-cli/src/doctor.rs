//! `anvil doctor`: one row per check, each failure with its fix.
//!
//! Rows: the `.env` files loaded, every API key, every provider, the tools
//! on `PATH` and the key scan over tracked files. The skill, command, agent,
//! hook and MCP reports follow unchanged.

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anvil_core::context;
use anvil_core::provider::{CompletionRequest, Message};
use anvil_types::dotenv::is_placeholder;
use anvil_types::{AnvilConfig, Protocol};
use tokio::process::Command;

use crate::commands;
use crate::secrets;
use crate::wiring::Wiring;

/// Wall clock allowed for one provider ping.
const PING_TIMEOUT: Duration = Duration::from_secs(20);

/// Wall clock allowed for one tool invocation.
const SHORT_TIMEOUT: Duration = Duration::from_secs(15);

/// What a run of doctor needs besides the wiring.
pub struct Options<'a> {
    /// Every `.env` file in loading order, present or not.
    pub env_files: &'a [PathBuf],
    /// The `.env` files that were loaded.
    pub loaded_env: &'a [PathBuf],
}

/// How a check went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// The check passed.
    Pass,
    /// The check failed; the row carries the fix.
    Fail,
    /// The check did not apply here; the detail says why.
    Skip,
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(match self {
            Self::Pass => "PASS",
            Self::Fail => "FAIL",
            Self::Skip => "SKIP",
        })
    }
}

/// One row of the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    /// What was checked, such as `key:ANTHROPIC_API_KEY`.
    pub name: String,
    /// How it went.
    pub status: Status,
    /// What was found.
    pub detail: String,
    /// What to do about a failure.
    pub fix: Option<String>,
}

/// The rows, in order.
#[derive(Debug, Default)]
pub struct Report {
    /// The rows.
    pub checks: Vec<Check>,
}

impl Report {
    /// Adds a passing row.
    fn pass(&mut self, name: impl Into<String>, detail: impl Into<String>) {
        self.push(name, Status::Pass, detail, None);
    }

    /// Adds a failing row with its fix.
    fn fail(&mut self, name: impl Into<String>, detail: impl Into<String>, fix: impl Into<String>) {
        self.push(name, Status::Fail, detail, Some(fix.into()));
    }

    /// Adds a skipped row.
    fn skip(&mut self, name: impl Into<String>, detail: impl Into<String>) {
        self.push(name, Status::Skip, detail, None);
    }

    /// Adds a row.
    fn push(
        &mut self,
        name: impl Into<String>,
        status: Status,
        detail: impl Into<String>,
        fix: Option<String>,
    ) {
        self.checks.push(Check {
            name: name.into(),
            status,
            detail: detail.into(),
            fix,
        });
    }

    /// How many rows failed.
    pub fn failures(&self) -> usize {
        self.checks
            .iter()
            .filter(|check| check.status == Status::Fail)
            .count()
    }

    /// The rows as aligned text, each failure followed by its fix.
    ///
    /// # Returns
    ///
    /// The text, with a trailing newline.
    pub fn render(&self) -> String {
        let width = self
            .checks
            .iter()
            .map(|check| check.name.len())
            .max()
            .unwrap_or(0);
        let mut out = String::new();
        for check in &self.checks {
            out.push_str(&format!(
                "{:<4}  {:<width$}  {}\n",
                check.status, check.name, check.detail
            ));
            if let Some(fix) = &check.fix {
                out.push_str(&format!("      fix: {fix}\n"));
            }
        }
        out
    }
}

/// Runs every check, prints the report and the compat reports.
///
/// # Arguments
///
/// * `wiring` - Providers, paths and the loaded compat sets.
/// * `options` - The `.env` files and flags.
///
/// # Returns
///
/// Zero when every row passed and every MCP server answered, one otherwise.
///
/// # Errors
///
/// When the permissions file cannot be read.
pub async fn run(wiring: &Wiring, options: Options<'_>) -> anyhow::Result<i32> {
    let mut report = Report::default();
    let config = &wiring.config;
    let root = secrets::git_toplevel(&wiring.cwd);

    for line in crate::policy::report(wiring.policy.as_deref()) {
        println!("{line}");
    }
    println!(
        "repository: {}",
        if wiring.project.trusted {
            "trusted; what it ships is loaded"
        } else {
            "not trusted; nothing it ships is loaded"
        }
    );
    report_policy(&mut report, wiring.policy.as_deref());
    report_env(&mut report, options.env_files, options.loaded_env);
    let bad_keys = report_keys(&mut report, config);
    report_providers(&mut report, wiring, &bad_keys).await;
    report_tools(&mut report).await;
    report_secrets(&mut report, root.as_deref(), options.env_files);

    print!("{}", report.render());
    let roles = &config.roles;
    println!(
        "roles: coder={} summarizer={} reviewer={}{}{}",
        roles.coder,
        roles.summarizer,
        roles.reviewer.as_deref().unwrap_or("(coder)"),
        roles
            .planner
            .as_deref()
            .map(|planner| format!(" planner={planner}"))
            .unwrap_or_default(),
        roles
            .worker
            .as_deref()
            .map(|worker| format!(" worker={worker}"))
            .unwrap_or_default()
    );
    commands::report_skills(wiring)?;
    commands::report_commands(wiring);
    commands::report_agents(wiring);
    commands::report_hooks(wiring);
    let failures = report.failures() + commands::report_mcp(wiring).await;
    if failures > 0 {
        println!("doctor: {failures} check(s) failed");
        return Ok(1);
    }
    println!("doctor: all checks passed");
    Ok(0)
}

/// The policy rows: whether one is installed, whether only an
/// administrator can change it, and its hash.
fn report_policy(report: &mut Report, policy: Option<&anvil_types::policy::LoadedPolicy>) {
    let Some(loaded) = policy else {
        report.skip(
            "policy",
            format!(
                "unmanaged: no file at {}",
                anvil_types::policy::system_path().display()
            ),
        );
        return;
    };
    report.pass("policy", format!("managed: {}", loaded.path.display()));
    match anvil_types::policy::ownership::check(&loaded.path) {
        Ok(()) => report.pass(
            "policy:ownership",
            "only an administrator can change the file and its folder",
        ),
        Err(detail) => report.fail(
            "policy:ownership",
            detail,
            "give the file and its folder to an administrator (docs/ADMIN_GUIDE.md)",
        ),
    }
    report.pass("policy:sha256", loaded.sha256.clone());
}

/// The `env` row: which `.env` files were loaded.
fn report_env(report: &mut Report, env_files: &[PathBuf], loaded: &[PathBuf]) {
    if loaded.is_empty() {
        let first = env_files
            .first()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "~/.anvil/.env".to_owned());
        report.skip(
            "env",
            format!("no .env file found; `anvil setup` writes {first}"),
        );
        return;
    }
    let names: Vec<String> = loaded
        .iter()
        .map(|path| path.display().to_string())
        .collect();
    report.pass("env", format!("loaded {}", names.join(", ")));
}

/// One `key:<ENV>` row per distinct variable; returns the unusable ones.
fn report_keys(report: &mut Report, config: &AnvilConfig) -> BTreeSet<String> {
    let names: BTreeSet<&str> = config
        .providers
        .values()
        .filter(|provider| provider.protocol != Protocol::Fake)
        .map(|provider| provider.api_key_env.as_str())
        .collect();
    let mut bad = BTreeSet::new();
    for name in names {
        let row = format!("key:{name}");
        match std::env::var(name) {
            Ok(value) if is_placeholder(&value) => {
                bad.insert(name.to_owned());
                report.fail(
                    row,
                    "still the template placeholder",
                    format!("replace the value of {name} in ~/.anvil/.env with the real key"),
                );
            }
            Ok(_) => report.pass(row, "set"),
            Err(_) => {
                bad.insert(name.to_owned());
                report.fail(
                    row,
                    "not set",
                    format!(
                        "add {name}=<key> to ~/.anvil/.env (`anvil setup` writes the template)"
                    ),
                );
            }
        }
    }
    bad
}

/// One `provider:<name>` row per provider, pinged concurrently.
async fn report_providers(report: &mut Report, wiring: &Wiring, bad_keys: &BTreeSet<String>) {
    let mut pings = Vec::new();
    for (name, entry) in &wiring.config.providers {
        if let Some(script) = entry
            .script
            .as_deref()
            .filter(|_| entry.protocol == Protocol::Fake)
        {
            report.pass(
                format!("provider:{name}"),
                format!("fake: replays {script}"),
            );
            continue;
        }
        if bad_keys.contains(&entry.api_key_env) {
            report.skip(
                format!("provider:{name}"),
                format!("key {} is not usable", entry.api_key_env),
            );
            continue;
        }
        pings.push(async move {
            let result = tokio::time::timeout(PING_TIMEOUT, ping(wiring, name)).await;
            (name, result)
        });
    }
    for (name, result) in futures_util::future::join_all(pings).await {
        let entry = &wiring.config.providers[name];
        let row = format!("provider:{name}");
        let endpoint = match entry.protocol {
            Protocol::Openai => {
                format!("{}/chat/completions", entry.base_url.trim_end_matches('/'))
            }
            Protocol::Anthropic => format!("{}/messages", entry.base_url.trim_end_matches('/')),
            Protocol::Fake => continue,
        };
        let fix = format!(
            "check {} and base_url {}: POST {endpoint} must accept model `{}`",
            entry.api_key_env, entry.base_url, entry.model
        );
        match result {
            Ok(Ok(millis)) => report.pass(row, format!("{} answered in {millis} ms", entry.model)),
            Ok(Err(error)) => report.fail(row, head(&format!("{error:#}"), 200), fix),
            Err(_) => report.fail(
                row,
                format!("no answer within {} s", PING_TIMEOUT.as_secs()),
                fix,
            ),
        }
    }
}

/// Sends a one-token request and returns the latency in milliseconds.
async fn ping(wiring: &Wiring, name: &str) -> anyhow::Result<u128> {
    let provider = wiring.provider(name)?;
    let request = CompletionRequest {
        system: "Reply with the single word OK.".to_owned(),
        messages: vec![Message::user("ping")],
        tools: Vec::new(),
        max_output_tokens: Some(1),
    };
    let started = Instant::now();
    context::collect_text(&*provider, request).await?;
    Ok(started.elapsed().as_millis())
}

/// One `tool:<name>` row per tool anvil needs, with its version.
async fn report_tools(report: &mut Report) {
    let mut git = Command::from(anvil_core::git::standalone());
    git.arg("--version");
    match tool_output(git).await {
        Some(version) => report.pass("tool:git", version),
        None => report.fail(
            "tool:git",
            "not on PATH",
            "install git (https://git-scm.com)",
        ),
    }
    match shell_candidate().await {
        Some(detail) => report.pass("tool:sh", detail),
        None if cfg!(windows) => report.fail(
            "tool:sh",
            "no sh found (Git Bash)",
            "install Git for Windows (https://git-scm.com) and add its usr\\bin to PATH",
        ),
        None => report.fail("tool:sh", "no sh on PATH", "install a POSIX shell"),
    }
}

/// The first line a tool prints, or `None` when it is missing or fails.
async fn tool_output(mut command: Command) -> Option<String> {
    let out = tokio::time::timeout(SHORT_TIMEOUT, command.kill_on_drop(true).output())
        .await
        .ok()?
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().next().map(str::trim).unwrap_or("");
    let line = if line.is_empty() {
        String::from_utf8_lossy(&out.stderr)
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .to_owned()
    } else {
        line.to_owned()
    };
    Some(line)
}

/// A working `sh`, on PATH or in the Git for Windows install.
async fn shell_candidate() -> Option<String> {
    let mut candidates = vec!["sh".to_owned()];
    if cfg!(windows) {
        let program_files =
            std::env::var("ProgramFiles").unwrap_or_else(|_| "C:\\Program Files".to_owned());
        candidates.push(format!("{program_files}\\Git\\usr\\bin\\sh.exe"));
        candidates.push(format!("{program_files}\\Git\\bin\\sh.exe"));
    }
    for candidate in candidates {
        let mut sh = Command::new(&candidate);
        sh.args(["-c", "echo ok"])
            .env_clear()
            .envs(anvil_types::child_env::vars());
        if tool_output(sh).await.as_deref() == Some("ok") {
            return Some(candidate);
        }
    }
    None
}

/// The `secrets` row: the same scan as `anvil scan-keys`.
fn report_secrets(report: &mut Report, root: Option<&Path>, env_files: &[PathBuf]) {
    let Some(root) = root else {
        report.skip("secrets", "not inside a git repository");
        return;
    };
    let scan = secrets::env_values(env_files).and_then(|values| secrets::scan_tree(root, &values));
    match scan {
        Ok(hits) if hits.is_empty() => report.pass("secrets", "no API keys in tracked files"),
        Ok(hits) => {
            let listed: Vec<String> = hits.iter().take(5).map(ToString::to_string).collect();
            report.fail(
                "secrets",
                format!("{} hit(s): {}", hits.len(), listed.join("; ")),
                "remove the values from those files, rotate the keys and commit again (`anvil scan-keys` lists every hit)",
            );
        }
        Err(error) => report.fail(
            "secrets",
            format!("{error:#}"),
            "run `anvil scan-keys` for details",
        ),
    }
}

/// The first `max` characters of a message, on one line.
fn head(text: &str, max: usize) -> String {
    let one_line: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= max {
        one_line
    } else {
        let cut: String = one_line.chars().take(max).collect();
        format!("{cut}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_renders_aligned_rows_with_fixes_under_failures() {
        let mut report = Report::default();
        report.pass("env", "loaded a");
        report.fail("key:LONG_NAME", "not set", "add it");
        report.skip("x", "n/a");
        let text = report.render();
        assert_eq!(
            text,
            "PASS  env            loaded a\nFAIL  key:LONG_NAME  not set\n      fix: add it\nSKIP  x              n/a\n"
        );
        assert_eq!(report.failures(), 1);
    }

    #[test]
    fn head_truncates_on_one_line() {
        assert_eq!(head("a\n  b", 10), "a b");
        assert_eq!(head("abcdef", 3), "abc…");
    }

    #[test]
    fn policy_rows_show_presence_ownership_and_hash() {
        let mut report = Report::default();
        report_policy(&mut report, None);
        assert_eq!(report.checks[0].status, Status::Skip);
        assert!(report.checks[0].detail.starts_with("unmanaged"));

        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("policy.toml");
        std::fs::write(
            &path,
            "version = 1\n[network]\nallowed_hosts = [\"m.example:443\"]\n",
        )
        .expect("write");
        let loaded = anvil_types::policy::load(&path)
            .expect("valid")
            .expect("present");
        let mut report = Report::default();
        report_policy(&mut report, Some(&loaded));
        let names: Vec<&str> = report.checks.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["policy", "policy:ownership", "policy:sha256"]);
        assert_eq!(report.checks[2].detail, loaded.sha256);
        #[cfg(windows)]
        assert_eq!(
            report.checks[1].status,
            Status::Fail,
            "a file the user created is not administrator-owned"
        );
    }
}
