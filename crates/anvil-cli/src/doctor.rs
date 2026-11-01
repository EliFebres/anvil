//! `anvil doctor`: one row per check, each failure with its fix.
//!
//! Rows: the `.env` files loaded, every API key, every provider, the
//! reviewers in `factory.toml`, GitHub access for both tokens, branch
//! protection on `main`, the `factory/state` branch and its ruleset, that
//! `main` takes pull requests only, the tools on `PATH` and the key scan over
//! tracked files. The skill, command, agent, hook and MCP reports follow
//! unchanged.

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anvil_core::context;
use anvil_core::provider::{CompletionRequest, Message};
use anvil_types::dotenv::is_placeholder;
use anvil_types::factory_config::FACTORY_FILE;
use anvil_types::{AnvilConfig, FactoryConfig, Protocol};
use anyhow::{Context, bail};
use serde_json::{Value, json};
use tokio::process::Command;

use crate::commands;
use crate::legacy;
use crate::secrets;
use crate::wiring::Wiring;

/// Wall clock allowed for one provider ping.
const PING_TIMEOUT: Duration = Duration::from_secs(20);

/// Wall clock allowed for one GitHub request or tool invocation.
const SHORT_TIMEOUT: Duration = Duration::from_secs(15);

/// Environment variable that overrides the GitHub API base, for tests.
const GITHUB_API_ENV: &str = "ANVIL_GITHUB_API_URL";

/// The status check branch protection must require.
const REQUIRED_CHECK: &str = "ci";

/// The branch that carries ticket state; the factory pushes to it directly.
const STATE_BRANCH: &str = anvil_types::ticket::dirs::STATE_BRANCH;

/// The ruleset `--fix-github` creates for the state branch.
const STATE_RULESET: &str = "factory-state";

/// What a run of doctor needs besides the wiring.
pub struct Options<'a> {
    /// Every `.env` file in loading order, present or not.
    pub env_files: &'a [PathBuf],
    /// The `.env` files that were loaded.
    pub loaded_env: &'a [PathBuf],
    /// Apply the branch protection settings through `gh`.
    pub fix_github: bool,
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

    report_env(&mut report, options.env_files, options.loaded_env);
    report_legacy(&mut report, wiring.home.as_deref(), &wiring.cwd);
    let bad_keys = report_keys(&mut report, config);
    report_providers(&mut report, wiring, &bad_keys).await;
    let factory = report_reviewers(&mut report, config, root.as_deref());
    let repo = factory
        .as_ref()
        .map(|factory| factory.github.repo.clone())
        .or_else(|| root.as_deref().and_then(github_remote));
    report_github(&mut report, repo.as_deref(), options.fix_github).await;
    report_tools(&mut report).await;
    report_secrets(&mut report, root.as_deref(), options.env_files);

    print!("{}", report.render());
    let roles = &config.roles;
    println!(
        "roles: coder={} summarizer={} reviewer={}",
        roles.coder,
        roles.summarizer,
        roles.reviewer.as_deref().unwrap_or("(coder)")
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

/// The `legacy` row: directories and variables still carrying the old name.
fn report_legacy(report: &mut Report, home: Option<&Path>, cwd: &Path) {
    let left = legacy::leftovers(home, cwd);
    if left.is_empty() {
        report.pass("legacy", "nothing left from the old name");
        return;
    }
    report.fail(
        "legacy",
        format!("still present: {}", left.join(", ")),
        "delete or merge the old directories into .anvil and rename the variables to ANVIL_*",
    );
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

/// The `reviewers` row; returns the parsed `factory.toml` when there is one.
fn report_reviewers(
    report: &mut Report,
    config: &AnvilConfig,
    root: Option<&Path>,
) -> Option<FactoryConfig> {
    let path = root?.join(FACTORY_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(_) => {
            report.skip(
                "reviewers",
                format!("no {FACTORY_FILE} at the repository root (`anvil setup` writes it)"),
            );
            return None;
        }
    };
    let factory: FactoryConfig = match toml::from_str(&text) {
        Ok(factory) => factory,
        Err(error) => {
            report.fail(
                "reviewers",
                head(&error.to_string(), 200),
                format!("fix {}", path.display()),
            );
            return None;
        }
    };
    if let Err(error) = factory.validate() {
        report.fail(
            "reviewers",
            error.to_string(),
            format!("fix {}", path.display()),
        );
        return Some(factory);
    }
    match factory.check_independence(config) {
        Err(problem) => report.fail(
            "reviewers",
            problem.to_string(),
            format!(
                "give each reviewer in {FACTORY_FILE} a provider whose base_url differs from the coder's and from the other reviewers'"
            ),
        ),
        Ok(()) => {
            let names: BTreeSet<&str> = factory
                .reviewers
                .iter()
                .map(|reviewer| reviewer.provider.as_str())
                .collect();
            report.pass(
                "reviewers",
                format!(
                    "coder={} reviewers={}",
                    factory.roles.coder,
                    names.into_iter().collect::<Vec<_>>().join(",")
                ),
            );
        }
    }
    Some(factory)
}

/// The `owner/repo` of the `origin` remote, when it is on GitHub.
fn github_remote(root: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let url = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    let rest = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))
        .or_else(|| url.strip_prefix("git@github.com:"))?;
    let rest = rest.trim_end_matches('/');
    Some(rest.strip_suffix(".git").unwrap_or(rest).to_owned())
}

/// A minimal GitHub REST client bound to one token.
struct GitHub {
    /// The HTTP client.
    client: reqwest::Client,
    /// The API base, without a trailing slash.
    base: String,
    /// The token.
    token: String,
}

impl GitHub {
    /// Builds a client for a token.
    fn new(token: &str) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(SHORT_TIMEOUT)
            .user_agent(format!("anvil/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .context("building the HTTP client")?;
        let base = std::env::var(GITHUB_API_ENV)
            .unwrap_or_else(|_| "https://api.github.com".to_owned())
            .trim_end_matches('/')
            .to_owned();
        Ok(Self {
            client,
            base,
            token: token.to_owned(),
        })
    }

    /// Sends one request and returns the status and the JSON body.
    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> anyhow::Result<(u16, Value)> {
        let mut request = self
            .client
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request
            .send()
            .await
            .with_context(|| format!("requesting {path}"))?;
        let status = response.status().as_u16();
        let text = response.text().await.unwrap_or_default();
        let value = serde_json::from_str(&text).unwrap_or(Value::Null);
        Ok((status, value))
    }

    /// `GET path`.
    async fn get(&self, path: &str) -> anyhow::Result<(u16, Value)> {
        self.send(reqwest::Method::GET, path, None).await
    }
}

/// A GitHub error message, from the body when present.
fn github_message(status: u16, body: &Value) -> String {
    let message = body
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("no message");
    format!("HTTP {status}: {message}")
}

/// The three GitHub rows.
async fn report_github(report: &mut Report, repo: Option<&str>, fix_github: bool) {
    let Some(repo) = repo else {
        for row in ["github:token", "github:factory-token", "github:protection"] {
            report.skip(
                row,
                "no GitHub repository: set [github] repo in factory.toml or add an origin remote",
            );
        }
        return;
    };
    let token_path = "github.com -> Settings -> Developer settings -> Fine-grained tokens";
    let personal = match usable_token("GITHUB_TOKEN") {
        Ok(token) => token,
        Err(detail) => {
            report.fail(
                "github:token",
                detail,
                format!("set GITHUB_TOKEN in ~/.anvil/.env to a token from {token_path} scoped to {repo}: Contents read, Pull requests read and write"),
            );
            String::new()
        }
    };
    let mut personal_client = None;
    if !personal.is_empty() {
        match GitHub::new(&personal) {
            Ok(client) => {
                report_personal_token(report, &client, repo).await;
                personal_client = Some(client);
            }
            Err(error) => report.fail("github:token", format!("{error:#}"), "retry"),
        }
    }

    let mut factory_client = None;
    match std::env::var("FACTORY_GITHUB_TOKEN") {
        Err(_) => report.skip(
            "github:factory-token",
            "FACTORY_GITHUB_TOKEN not set (only the NUC needs it)",
        ),
        Ok(token) if is_placeholder(&token) => report.fail(
            "github:factory-token",
            "still the template placeholder",
            format!("set FACTORY_GITHUB_TOKEN in ~/.factory/env to a token from {token_path} scoped to {repo}: Contents read and write, Pull requests read and write, Workflows read"),
        ),
        Ok(token) => match GitHub::new(&token) {
            Ok(client) => {
                report_factory_token(report, &client, repo).await;
                factory_client = Some(client);
            }
            Err(error) => report.fail("github:factory-token", format!("{error:#}"), "retry"),
        },
    }

    report_protection(report, personal_client.as_ref(), repo, fix_github).await;
    match factory_client.as_ref().or(personal_client.as_ref()) {
        Some(client) => {
            report_state_branch(report, client, repo).await;
            report_main_locked(report, client, repo).await;
        }
        None => {
            for row in ["github:state-branch", "github:main-locked"] {
                report.skip(row, "no usable token");
            }
        }
    }
}

/// The `github:state-branch` row: `factory/state` exists and a ruleset
/// forbids deleting or force-pushing it without requiring pull requests, so
/// the factory can commit ticket moves to it directly.
async fn report_state_branch(report: &mut Report, client: &GitHub, repo: &str) {
    let row = "github:state-branch";
    let fix = format!(
        "run `anvil doctor --fix-github`, or create branch `{STATE_BRANCH}` from main and a ruleset for it with the rules `Restrict deletions` and `Block force pushes` only"
    );
    let branch = match client
        .get(&format!(
            "/repos/{repo}/branches/{}",
            encode_branch(STATE_BRANCH)
        ))
        .await
    {
        Ok(answer) => answer,
        Err(error) => {
            report.fail(row, format!("{error:#}"), "check the network and the token");
            return;
        }
    };
    let rules = match client
        .get(&format!(
            "/repos/{repo}/rules/branches/{}",
            encode_branch(STATE_BRANCH)
        ))
        .await
    {
        Ok((200, rules)) => rules,
        Ok((status, body)) => {
            report.fail(
                row,
                format!(
                    "cannot read the rules of {STATE_BRANCH}: {}",
                    github_message(status, &body)
                ),
                fix,
            );
            return;
        }
        Err(error) => {
            report.fail(row, format!("{error:#}"), "check the network and the token");
            return;
        }
    };
    let gaps = state_branch_gaps(branch.0, &rules);
    if gaps.is_empty() {
        report.pass(
            row,
            format!(
                "{STATE_BRANCH} exists; deletion and force pushes blocked, no pull request required"
            ),
        );
    } else {
        report.fail(row, gaps.join("; "), fix);
    }
}

/// What is wrong with the state branch, given the branch lookup status and
/// the rules in effect for it.
fn state_branch_gaps(branch_status: u16, rules: &Value) -> Vec<String> {
    let mut gaps = Vec::new();
    if branch_status == 404 {
        gaps.push(format!("branch `{STATE_BRANCH}` does not exist"));
        return gaps;
    }
    if branch_status != 200 {
        gaps.push(format!(
            "cannot read branch `{STATE_BRANCH}` (HTTP {branch_status})"
        ));
        return gaps;
    }
    let types = rule_types(rules);
    if !types.iter().any(|t| t == "deletion") {
        gaps.push("deletion of the state branch is not blocked".to_owned());
    }
    if !types.iter().any(|t| t == "non_fast_forward") {
        gaps.push("force pushes to the state branch are not blocked".to_owned());
    }
    if types.iter().any(|t| t == "pull_request") {
        gaps.push(
            "the state branch requires pull requests, so the factory cannot commit ticket moves to it"
                .to_owned(),
        );
    }
    gaps
}

/// The `github:main-locked` row: `main` takes pull requests only, through a
/// ruleset with no bypass actors or through classic protection that binds
/// admins too, so no token can push code to it directly.
async fn report_main_locked(report: &mut Report, client: &GitHub, repo: &str) {
    let row = "github:main-locked";
    let fix = format!(
        "open https://github.com/{repo}/settings/rules and give `main` a ruleset with `Require a pull request before merging` and no bypass actors"
    );
    let rules = match client
        .get(&format!("/repos/{repo}/rules/branches/main"))
        .await
    {
        Ok((200, rules)) => rules,
        Ok((status, body)) => {
            report.fail(
                row,
                format!(
                    "cannot read the rules of main: {}",
                    github_message(status, &body)
                ),
                fix,
            );
            return;
        }
        Err(error) => {
            report.fail(row, format!("{error:#}"), "check the network and the token");
            return;
        }
    };
    let mut rulesets = Vec::new();
    for id in ruleset_ids(&rules) {
        match client.get(&format!("/repos/{repo}/rulesets/{id}")).await {
            Ok((200, ruleset)) => rulesets.push(ruleset),
            Ok((status, body)) => {
                report.fail(
                    row,
                    format!(
                        "cannot read ruleset {id}: {}",
                        github_message(status, &body)
                    ),
                    fix,
                );
                return;
            }
            Err(error) => {
                report.fail(row, format!("{error:#}"), "check the network and the token");
                return;
            }
        }
    }
    let classic = match client
        .get(&format!("/repos/{repo}/branches/main/protection"))
        .await
    {
        Ok((200, protection)) => Some(protection),
        _ => None,
    };
    let gaps = main_lock_gaps(&rules, &rulesets, classic.as_ref());
    if gaps.is_empty() {
        report.pass(
            row,
            "main takes pull requests only; no token can push to it directly",
        );
    } else {
        report.fail(row, gaps.join("; "), fix);
    }
}

/// What lets a token push to `main` directly, given the rules in effect, the
/// rulesets behind them and the classic protection, if any.
fn main_lock_gaps(rules: &Value, rulesets: &[Value], classic: Option<&Value>) -> Vec<String> {
    let types = rule_types(rules);
    if types.iter().any(|t| t == "pull_request") {
        let mut gaps = Vec::new();
        for ruleset in rulesets {
            let actors = ruleset
                .get("bypass_actors")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0);
            if actors > 0 {
                let name = ruleset.get("name").and_then(Value::as_str).unwrap_or("?");
                gaps.push(format!(
                    "ruleset `{name}` has {actors} bypass actor(s) who may push to main directly"
                ));
            }
        }
        return gaps;
    }
    let classic_locks = classic.is_some_and(|protection| {
        protection
            .pointer("/enforce_admins/enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            && protection
                .get("required_pull_request_reviews")
                .is_some_and(|v| !v.is_null())
    });
    if classic_locks {
        return Vec::new();
    }
    vec!["main does not require pull requests through a ruleset, and classic protection does not bind admins to pull requests either".to_owned()]
}

/// The `type` of every rule in a `GET /rules/branches/<name>` answer.
fn rule_types(rules: &Value) -> Vec<String> {
    rules
        .as_array()
        .map(|rules| {
            rules
                .iter()
                .filter_map(|rule| rule.get("type").and_then(Value::as_str))
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// The distinct `ruleset_id`s behind the rules, in order of appearance.
fn ruleset_ids(rules: &Value) -> Vec<u64> {
    let mut ids = Vec::new();
    for rule in rules.as_array().into_iter().flatten() {
        if let Some(id) = rule.get("ruleset_id").and_then(Value::as_u64)
            && !ids.contains(&id)
        {
            ids.push(id);
        }
    }
    ids
}

/// A branch name as a URL path segment: `factory/state` becomes `factory%2Fstate`.
fn encode_branch(branch: &str) -> String {
    branch.replace('/', "%2F")
}

/// A token from the environment, or why it is unusable.
fn usable_token(name: &str) -> Result<String, String> {
    match std::env::var(name) {
        Ok(token) if is_placeholder(&token) => Err("still the template placeholder".to_owned()),
        Ok(token) => Ok(token),
        Err(_) => Err("not set".to_owned()),
    }
}

/// The `github:token` row: the repository and its pull requests are readable.
async fn report_personal_token(report: &mut Report, client: &GitHub, repo: &str) {
    let fix =
        format!("the token must be scoped to {repo} with Contents read and Pull requests read");
    let repo_result = client.get(&format!("/repos/{repo}")).await;
    let (status, body) = match repo_result {
        Ok(answer) => answer,
        Err(error) => {
            report.fail(
                "github:token",
                format!("{error:#}"),
                "check the network and GITHUB_TOKEN",
            );
            return;
        }
    };
    if status != 200 {
        report.fail(
            "github:token",
            format!("cannot read {repo}: {}", github_message(status, &body)),
            fix,
        );
        return;
    }
    let default_branch = body
        .get("default_branch")
        .and_then(Value::as_str)
        .unwrap_or("?")
        .to_owned();
    match client.get(&format!("/repos/{repo}/pulls?per_page=1")).await {
        Ok((200, _)) => report.pass(
            "github:token",
            format!("reads {repo} (default branch {default_branch}) and lists pull requests"),
        ),
        Ok((status, body)) => report.fail(
            "github:token",
            format!(
                "cannot list pull requests: {}",
                github_message(status, &body)
            ),
            fix,
        ),
        Err(error) => report.fail(
            "github:token",
            format!("{error:#}"),
            "check the network and GITHUB_TOKEN",
        ),
    }
}

/// The `github:factory-token` row: a throwaway branch can be pushed and a
/// pull request opened from it.
///
/// Opening the pull request is answered with 422 (no commits between the
/// branches) when the token may open pull requests, and 403 when it may not;
/// this relies on GitHub checking permission before content. The branch is
/// deleted on every path.
async fn report_factory_token(report: &mut Report, client: &GitHub, repo: &str) {
    let row = "github:factory-token";
    let fix = format!(
        "the token must be scoped to {repo} with Contents read and write, Pull requests read and write, Workflows read"
    );
    let (status, body) = match client
        .get(&format!("/repos/{repo}/git/ref/heads/main"))
        .await
    {
        Ok(answer) => answer,
        Err(error) => {
            report.fail(
                row,
                format!("{error:#}"),
                "check the network and FACTORY_GITHUB_TOKEN",
            );
            return;
        }
    };
    let Some(sha) = body.pointer("/object/sha").and_then(Value::as_str) else {
        report.fail(
            row,
            format!("cannot read main: {}", github_message(status, &body)),
            fix,
        );
        return;
    };
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let branch = format!("doctor/{stamp}");
    let created = client
        .send(
            reqwest::Method::POST,
            &format!("/repos/{repo}/git/refs"),
            Some(json!({ "ref": format!("refs/heads/{branch}"), "sha": sha })),
        )
        .await;
    match created {
        Ok((201, _)) => {}
        Ok((status, body)) => {
            report.fail(
                row,
                format!(
                    "cannot push branch {branch}: {}",
                    github_message(status, &body)
                ),
                fix,
            );
            return;
        }
        Err(error) => {
            report.fail(
                row,
                format!("{error:#}"),
                "check the network and FACTORY_GITHUB_TOKEN",
            );
            return;
        }
    }
    let pull = client
        .send(
            reqwest::Method::POST,
            &format!("/repos/{repo}/pulls"),
            Some(json!({
                "title": "anvil doctor probe (never merged)",
                "head": branch,
                "base": "main",
                "body": "Opened by `anvil doctor` to test FACTORY_GITHUB_TOKEN; it is closed and its branch deleted immediately.",
            })),
        )
        .await;
    let deleted = client
        .send(
            reqwest::Method::DELETE,
            &format!("/repos/{repo}/git/refs/heads/{branch}"),
            None,
        )
        .await;
    let cleanup = match deleted {
        Ok((204, _)) => String::new(),
        Ok((status, body)) => format!(
            "; branch {branch} was NOT deleted ({}), delete it by hand",
            github_message(status, &body)
        ),
        Err(error) => format!("; branch {branch} may remain ({error:#}), check by hand"),
    };
    match pull {
        Ok((422, _)) => report.pass(
            row,
            format!("pushed and deleted branch {branch}; may open pull requests{cleanup}"),
        ),
        Ok((201, body)) => {
            let number = body.get("number").and_then(Value::as_u64).unwrap_or(0);
            report.pass(
                row,
                format!("pushed and deleted branch {branch}; opened pull request #{number}, close it by hand{cleanup}"),
            );
        }
        Ok((status, body)) => report.fail(
            row,
            format!(
                "pushed branch {branch} but cannot open a pull request: {}{cleanup}",
                github_message(status, &body)
            ),
            fix,
        ),
        Err(error) => report.fail(
            row,
            format!("{error:#}{cleanup}"),
            "check the network and FACTORY_GITHUB_TOKEN",
        ),
    }
}

/// What branch protection on `main` is missing.
#[derive(Debug, Default, PartialEq, Eq)]
struct ProtectionGaps {
    /// Problems, in the words of the settings page.
    problems: Vec<String>,
}

/// Reads the protection and repository settings and lists what is wrong.
fn protection_gaps(protection: &Value, repo_settings: &Value) -> ProtectionGaps {
    let mut gaps = ProtectionGaps::default();
    let checks = protection.pointer("/required_status_checks/checks");
    let contexts = protection.pointer("/required_status_checks/contexts");
    let has_ci = checks.and_then(Value::as_array).is_some_and(|checks| {
        checks
            .iter()
            .any(|check| check.get("context").and_then(Value::as_str) == Some(REQUIRED_CHECK))
    }) || contexts
        .and_then(Value::as_array)
        .is_some_and(|contexts| contexts.iter().any(|c| c.as_str() == Some(REQUIRED_CHECK)));
    if !has_ci {
        gaps.problems.push(format!(
            "required status check `{REQUIRED_CHECK}` is not set"
        ));
    }
    let force_pushes = protection
        .pointer("/allow_force_pushes/enabled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if force_pushes {
        gaps.problems.push("force pushes are allowed".to_owned());
    }
    let squash = repo_settings
        .get("allow_squash_merge")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    if !squash {
        gaps.problems.push("squash merging is disabled".to_owned());
    }
    gaps
}

/// The manual steps for the settings pages.
fn manual_steps(repo: &str) -> String {
    format!(
        "open https://github.com/{repo}/settings/branches, add or edit the rule for `main`: tick `Require status checks to pass before merging` and add `{REQUIRED_CHECK}`, leave `Allow force pushes` unticked; then open https://github.com/{repo}/settings and tick `Allow squash merging`; then create branch `{STATE_BRANCH}` from main with a ruleset that blocks deletions and force pushes only"
    )
}

/// Whether `gh` is installed and logged in.
async fn gh_authenticated() -> bool {
    let mut command = Command::new("gh");
    command.args(["auth", "status"]);
    // Same as gh_api: judge the login, not a token loaded from ~/.anvil/.env.
    command.env_remove("GITHUB_TOKEN").env_remove("GH_TOKEN");
    matches!(
        tokio::time::timeout(SHORT_TIMEOUT, command.output()).await,
        Ok(Ok(out)) if out.status.success()
    )
}

/// Runs `gh api` with an optional JSON body on stdin and parses the answer.
async fn gh_api(method: &str, path: &str, body: Option<&Value>) -> anyhow::Result<Value> {
    use tokio::io::AsyncWriteExt;

    let mut command = Command::new("gh");
    command.args(["api", "-X", method, path]);
    // gh prefers a token in the environment over its own login, and the
    // fine-grained GITHUB_TOKEN loaded from ~/.anvil/.env cannot administer
    // branches: its answers were 404s.
    command.env_remove("GITHUB_TOKEN").env_remove("GH_TOKEN");
    if body.is_some() {
        command.args(["--input", "-"]);
        command.stdin(std::process::Stdio::piped());
    }
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());
    let mut child = command.spawn().context("running gh")?;
    if let (Some(body), Some(mut stdin)) = (body, child.stdin.take()) {
        stdin.write_all(body.to_string().as_bytes()).await?;
        drop(stdin);
    }
    let out = tokio::time::timeout(SHORT_TIMEOUT, child.wait_with_output())
        .await
        .context("gh did not answer in time")??;
    if !out.status.success() {
        bail!(
            "gh api {method} {path}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(serde_json::from_slice(&out.stdout).unwrap_or(Value::Null))
}

/// The `github:protection` row, through `gh` when it is logged in, else the token.
async fn report_protection(
    report: &mut Report,
    personal: Option<&GitHub>,
    repo: &str,
    fix_github: bool,
) {
    let row = "github:protection";
    let gh = gh_authenticated().await;
    if fix_github {
        if !gh {
            report.fail(
                row,
                "--fix-github needs `gh` installed and logged in (`gh auth login`)",
                manual_steps(repo),
            );
            return;
        }
        if let Err(error) = apply_protection(repo).await {
            report.fail(
                row,
                format!("--fix-github failed: {error:#}"),
                manual_steps(repo),
            );
            return;
        }
    }
    let settings = if gh {
        read_protection_with_gh(repo).await
    } else if let Some(client) = personal {
        read_protection_with_token(client, repo).await
    } else {
        Err(anyhow::anyhow!("no GITHUB_TOKEN and `gh` is not logged in"))
    };
    match settings {
        Ok((protection, repo_settings)) => {
            let gaps = protection_gaps(&protection, &repo_settings);
            if gaps.problems.is_empty() {
                report.pass(
                    row,
                    format!(
                        "main requires `{REQUIRED_CHECK}`, blocks force pushes, squash merging on"
                    ),
                );
            } else {
                let fix = if gh {
                    format!("run `anvil doctor --fix-github`, or {}", manual_steps(repo))
                } else {
                    manual_steps(repo)
                };
                report.fail(row, gaps.problems.join("; "), fix);
            }
        }
        Err(error) => report.fail(row, format!("{error:#}"), manual_steps(repo)),
    }
}

/// Reads the protection of `main` and the repository settings through `gh`.
async fn read_protection_with_gh(repo: &str) -> anyhow::Result<(Value, Value)> {
    let protection = gh_api(
        "GET",
        &format!("repos/{repo}/branches/main/protection"),
        None,
    )
    .await
    .context("main is not protected, or the account lacks admin access")?;
    let settings = gh_api("GET", &format!("repos/{repo}"), None).await?;
    Ok((protection, settings))
}

/// Reads the protection of `main` and the repository settings with the token.
async fn read_protection_with_token(client: &GitHub, repo: &str) -> anyhow::Result<(Value, Value)> {
    let (status, protection) = client
        .get(&format!("/repos/{repo}/branches/main/protection"))
        .await?;
    match status {
        200 => {}
        404 => bail!(
            "main is not protected, or GITHUB_TOKEN lacks Administration read (needed to see protection); install `gh` to check with your login"
        ),
        403 => bail!(
            "GITHUB_TOKEN may not read branch protection ({}); install `gh` to check with your login",
            github_message(status, &protection)
        ),
        _ => bail!("{}", github_message(status, &protection)),
    }
    let (status, settings) = client.get(&format!("/repos/{repo}")).await?;
    if status != 200 {
        bail!("{}", github_message(status, &settings));
    }
    Ok((protection, settings))
}

/// Sets the required protection on `main` and enables squash merging through `gh`.
async fn apply_protection(repo: &str) -> anyhow::Result<()> {
    let protection = json!({
        "required_status_checks": { "strict": false, "checks": [{ "context": REQUIRED_CHECK }] },
        "enforce_admins": null,
        "required_pull_request_reviews": null,
        "restrictions": null,
        "allow_force_pushes": false,
        "allow_deletions": false,
    });
    gh_api(
        "PUT",
        &format!("repos/{repo}/branches/main/protection"),
        Some(&protection),
    )
    .await?;
    gh_api(
        "PATCH",
        &format!("repos/{repo}"),
        Some(&json!({ "allow_squash_merge": true })),
    )
    .await?;
    ensure_state_branch(repo).await?;
    Ok(())
}

/// Creates `factory/state` from `main` when it is missing, and its ruleset
/// (deletion and force pushes blocked, nothing else) when none has the name.
async fn ensure_state_branch(repo: &str) -> anyhow::Result<()> {
    let branch_path = format!("repos/{repo}/branches/{}", encode_branch(STATE_BRANCH));
    if gh_api("GET", &branch_path, None).await.is_err() {
        let main = gh_api("GET", &format!("repos/{repo}/git/ref/heads/main"), None).await?;
        let sha = main
            .pointer("/object/sha")
            .and_then(Value::as_str)
            .context("main has no head commit")?;
        gh_api(
            "POST",
            &format!("repos/{repo}/git/refs"),
            Some(&json!({ "ref": format!("refs/heads/{STATE_BRANCH}"), "sha": sha })),
        )
        .await
        .context("creating the state branch")?;
    }
    let rulesets = gh_api("GET", &format!("repos/{repo}/rulesets"), None).await?;
    let exists = rulesets.as_array().is_some_and(|list| {
        list.iter()
            .any(|r| r.get("name").and_then(Value::as_str) == Some(STATE_RULESET))
    });
    if !exists {
        gh_api(
            "POST",
            &format!("repos/{repo}/rulesets"),
            Some(&json!({
                "name": STATE_RULESET,
                "target": "branch",
                "enforcement": "active",
                "conditions": { "ref_name": { "include": [format!("refs/heads/{STATE_BRANCH}")], "exclude": [] } },
                "rules": [{ "type": "deletion" }, { "type": "non_fast_forward" }],
            })),
        )
        .await
        .context("creating the state-branch ruleset")?;
    }
    Ok(())
}

/// One `tool:<name>` row per required tool, with its version.
async fn report_tools(report: &mut Report) {
    let tools: Vec<(&str, Vec<&str>, &str)> = vec![
        (
            "git",
            vec!["--version"],
            "install git (https://git-scm.com)",
        ),
        (
            "cargo",
            vec!["--version"],
            "install Rust with rustup (https://rustup.rs)",
        ),
        (
            "just",
            vec!["--version"],
            "install just: cargo install just, or the distro package",
        ),
        (
            "python",
            vec!["--version"],
            "install Python 3 (python.org, or winget install Python.Python.3)",
        ),
    ];
    for (name, args, fix) in tools {
        match tool_output(name, &args).await {
            Some(version) => report.pass(format!("tool:{name}"), version),
            None => report.fail(format!("tool:{name}"), "not on PATH", fix),
        }
    }
    let libs = tool_output(
        "python",
        &["-c", "import pytest, pandas; print('pytest', pytest.__version__, 'pandas', pandas.__version__)"],
    )
    .await;
    match libs {
        Some(versions) => report.pass("tool:python-libs", versions),
        None => report.fail(
            "tool:python-libs",
            "pytest or pandas missing",
            "run: python -m pip install pytest pandas",
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
    if cfg!(target_os = "linux") {
        let podman = tool_output("podman", &["--version"]).await;
        let docker = tool_output("docker", &["--version"]).await;
        match podman.or(docker) {
            Some(version) => report.pass("tool:container", version),
            None => report.fail(
                "tool:container",
                "neither podman nor docker on PATH",
                "install podman with the distro package manager",
            ),
        }
    }
}

/// The first line a tool prints, or `None` when it is missing or fails.
async fn tool_output(program: &str, args: &[&str]) -> Option<String> {
    let out = tokio::time::timeout(SHORT_TIMEOUT, Command::new(program).args(args).output())
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
        if tool_output(&candidate, &["-c", "echo ok"]).await.as_deref() == Some("ok") {
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
    fn state_branch_gaps_want_deletion_and_force_push_rules_only() {
        let good = json!([
            { "type": "deletion", "ruleset_id": 7 },
            { "type": "non_fast_forward", "ruleset_id": 7 }
        ]);
        assert!(state_branch_gaps(200, &good).is_empty());
        assert_eq!(
            state_branch_gaps(404, &good),
            vec!["branch `factory/state` does not exist".to_owned()]
        );
        let gaps = state_branch_gaps(200, &json!([{ "type": "pull_request", "ruleset_id": 1 }]));
        assert_eq!(gaps.len(), 3, "{gaps:?}");
        assert!(gaps[0].contains("deletion"));
        assert!(gaps[1].contains("force"));
        assert!(gaps[2].contains("pull requests"));
        assert_eq!(encode_branch("factory/state"), "factory%2Fstate");
    }

    #[test]
    fn main_is_locked_by_a_ruleset_without_bypass_or_by_classic_admin_protection() {
        let pr_rule = json!([{ "type": "pull_request", "ruleset_id": 3 }]);
        let clean = json!({ "name": "main", "bypass_actors": [] });
        assert!(main_lock_gaps(&pr_rule, &[clean], None).is_empty());
        let leaky = json!({ "name": "main", "bypass_actors": [{ "actor_id": 5, "actor_type": "RepositoryRole" }] });
        let gaps = main_lock_gaps(&pr_rule, &[leaky], None);
        assert_eq!(gaps.len(), 1);
        assert!(gaps[0].contains("`main` has 1 bypass actor"), "{gaps:?}");

        let none = json!([]);
        let classic = json!({ "enforce_admins": { "enabled": true }, "required_pull_request_reviews": { "required_approving_review_count": 0 } });
        assert!(main_lock_gaps(&none, &[], Some(&classic)).is_empty());
        let weak = json!({ "enforce_admins": { "enabled": false }, "required_pull_request_reviews": null });
        assert_eq!(main_lock_gaps(&none, &[], Some(&weak)).len(), 1);
        assert_eq!(main_lock_gaps(&none, &[], None).len(), 1);
        assert_eq!(
            ruleset_ids(&json!([{ "ruleset_id": 3 }, { "ruleset_id": 3 }, { "ruleset_id": 9 }])),
            vec![3, 9]
        );
    }

    #[test]
    fn protection_gaps_are_named() {
        let good = json!({
            "required_status_checks": { "checks": [{ "context": "ci" }] },
            "allow_force_pushes": { "enabled": false }
        });
        let settings = json!({ "allow_squash_merge": true });
        assert!(protection_gaps(&good, &settings).problems.is_empty());

        let bad = json!({
            "required_status_checks": { "contexts": ["other"] },
            "allow_force_pushes": { "enabled": true }
        });
        let gaps = protection_gaps(&bad, &json!({ "allow_squash_merge": false }));
        assert_eq!(gaps.problems.len(), 3);
        assert!(gaps.problems[0].contains("`ci`"));
        assert!(gaps.problems[1].contains("force"));
        assert!(gaps.problems[2].contains("squash"));

        let legacy = json!({ "required_status_checks": { "contexts": ["ci"] } });
        assert!(protection_gaps(&legacy, &settings).problems.is_empty());
    }

    #[test]
    fn head_truncates_on_one_line() {
        assert_eq!(head("a\n  b", 10), "a b");
        assert_eq!(head("abcdef", 3), "abc…");
    }
}
