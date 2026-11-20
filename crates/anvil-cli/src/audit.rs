//! The session audit log: what the loop, the permission check, the plan
//! gate and subagents did, as chained records in the session folder's
//! `audit.jsonl` and the policy's mirror.
//!
//! Everything here translates and decides when to stop; the record, its
//! canonical form and the chain live in `anvil_audit::audit`.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::Path;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use anvil_audit::audit::{
    AUDIT_FILE, AuditEvent, AuditLog, AuditTarget, Content, Identity, ProviderEntry, RecordMode,
    mirror_file,
};
use anvil_core::agent::{LoopEvent, PlanDecision};
use anvil_core::guard::DataGuard;
use anvil_core::provider::{StopReason, Usage};
use anvil_core::tools::{
    CallGate, DecisionScope, DecisionSource, PermissionDecision, RuleOrigin, ToolOutcome,
};
use anvil_types::policy::{self, LoadedPolicy};
use anyhow::{Context as _, bail};
use sha2::Digest as _;
use tokio::sync::watch;

use crate::wiring::Wiring;

/// The agent name of records the session's own loop produces.
pub const MAIN_AGENT: &str = "main";

/// One session's audit log and what it needs to translate events.
pub struct SessionAudit {
    /// The log; `None` when it could not be opened and is not required.
    log: Option<AuditLog>,
    /// Whether the policy requires the log.
    required: bool,
    /// How prompts and arguments are kept.
    mode: RecordMode,
    /// Redacts kept text, and counts what was redacted.
    guard: DataGuard,
    /// Provider name to where it sends requests.
    providers: BTreeMap<String, ProviderEntry>,
    /// The guard's count at the previous model request.
    redactions_seen: AtomicU64,
    /// Set to the reason once a required log fails; the session stops.
    stop: watch::Sender<Option<String>>,
}

impl SessionAudit {
    /// Opens the session's log before anything else happens in it.
    ///
    /// # Arguments
    ///
    /// * `wiring` - The session's wiring: policy, guard and providers.
    /// * `session` - The session id.
    /// * `session_dir` - The session folder; the log is `audit.jsonl` in it.
    ///
    /// # Returns
    ///
    /// The audit. When the log cannot be opened and the policy does not
    /// require it, a warning is printed and records are dropped.
    ///
    /// # Errors
    ///
    /// When the policy's `[audit] required = true` and the log or its
    /// mirror cannot be opened.
    pub fn open(wiring: &Wiring, session: &str, session_dir: &Path) -> anyhow::Result<Self> {
        let settings = policy::audit_settings(wiring.policy.as_deref());
        let target = AuditTarget {
            primary: session_dir.join(AUDIT_FILE),
            mirror: settings
                .mirror_dir
                .as_deref()
                .map(|dir| mirror_file(dir, session)),
            required: settings.required,
        };
        let log = match AuditLog::open(session, identity(wiring.policy.as_deref()), &target) {
            Ok(log) => Some(log),
            Err(error) if settings.required => bail!(
                "the session does not start: the managed policy's [audit] required = true and the audit log cannot be opened: {error}"
            ),
            Err(error) => {
                tracing::warn!(%error, "the audit log cannot be opened; this session is not audited");
                eprintln!("warning: this session is not audited: {error}");
                None
            }
        };
        let providers = wiring
            .config
            .providers
            .iter()
            .map(|(name, provider)| {
                (
                    name.clone(),
                    ProviderEntry {
                        name: name.clone(),
                        host: host_of(&provider.base_url),
                        model: provider.model.clone(),
                    },
                )
            })
            .collect();
        Ok(Self {
            log,
            required: settings.required,
            mode: settings.record,
            guard: wiring.guard.clone(),
            providers,
            redactions_seen: AtomicU64::new(wiring.guard.redaction_count()),
            stop: watch::channel(None).0,
        })
    }

    /// Records the session's start: providers, roles, the switches in
    /// effect and what the policy refused while loading.
    ///
    /// # Arguments
    ///
    /// * `wiring` - The session's wiring.
    /// * `mode` - `run`, `chat` or `resume`.
    /// * `yes` - Whether `--yes` is in effect.
    pub fn start(&self, wiring: &Wiring, mode: &str, yes: bool) {
        let roles = &wiring.config.roles;
        let mut role_map = BTreeMap::from([
            ("coder".to_owned(), roles.coder.clone()),
            ("summarizer".to_owned(), roles.summarizer.clone()),
        ]);
        for (role, name) in [
            ("planner", &roles.planner),
            ("worker", &roles.worker),
            ("reviewer", &roles.reviewer),
        ] {
            if let Some(name) = name {
                role_map.insert(role.to_owned(), name.clone());
            }
        }
        let settings = policy::audit_settings(wiring.policy.as_deref());
        let flags = BTreeMap::from([
            ("yes".to_owned(), yes.to_string()),
            (
                "plan_approval_required".to_owned(),
                wiring.plans_need_a_person().to_string(),
            ),
            (
                "transcript".to_owned(),
                wiring.transcript.as_str().to_owned(),
            ),
            ("redaction".to_owned(), wiring.guard.redacts().to_string()),
            (
                "shell_mode".to_owned(),
                format!("{:?}", wiring.shell.mode).to_lowercase(),
            ),
            (
                "shell_executor".to_owned(),
                format!("{:?}", wiring.shell.executor).to_lowercase(),
            ),
            (
                "project_trusted".to_owned(),
                wiring.project.trusted.to_string(),
            ),
            ("audit_required".to_owned(), settings.required.to_string()),
            (
                "audit_record".to_owned(),
                settings.record.as_str().to_owned(),
            ),
            (
                "audit_mirror".to_owned(),
                settings.mirror_dir.is_some().to_string(),
            ),
        ]);
        self.record(
            MAIN_AGENT,
            AuditEvent::SessionStart {
                mode: mode.to_owned(),
                providers: self.providers.values().cloned().collect(),
                roles: role_map,
                flags,
            },
        );
        for refusal in &wiring.policy_refusals {
            self.record(
                MAIN_AGENT,
                AuditEvent::PolicyRefusal {
                    what: refusal.what.clone(),
                    key: refusal.key.clone(),
                },
            );
        }
    }

    /// Records a prompt the user submitted.
    ///
    /// # Arguments
    ///
    /// * `text` - The prompt as typed; it is redacted before it is kept.
    pub fn prompt(&self, text: &str) {
        self.record(
            MAIN_AGENT,
            AuditEvent::PromptSubmitted {
                content: Content::of(&self.guard.scrub(text), self.mode),
            },
        );
    }

    /// Records a loop event, if it is one the audit keeps.
    ///
    /// # Arguments
    ///
    /// * `agent` - `main` or the subagent that emitted it.
    /// * `event` - The event.
    /// * `provider` - The provider that serves the event's role, for
    ///   model requests.
    pub fn event(&self, agent: &str, event: &LoopEvent, provider: &str) {
        let record = match event {
            LoopEvent::TurnFinished {
                role,
                stop_reason,
                usage,
                ..
            } => self.model_request(role.name(), provider, *usage, stop_reason),
            LoopEvent::Compacted { usage, .. } => {
                self.model_request("summarizer", provider, *usage, &StopReason::EndTurn)
            }
            LoopEvent::ToolStarted { call } => AuditEvent::ToolCall {
                call_id: call.id.clone(),
                tool: call.name.clone(),
                arguments: Content::of(&self.guard.scrub(&call.arguments_json), self.mode),
            },
            LoopEvent::ToolFinished {
                call_id,
                name,
                outcome,
                stats,
                ..
            } => AuditEvent::ToolResult {
                call_id: call_id.clone(),
                tool: name.clone(),
                outcome: outcome_label(*outcome).to_owned(),
                error: !matches!(outcome, ToolOutcome::Completed),
                bytes: u64::try_from(stats.bytes).unwrap_or(u64::MAX),
                sha256: stats.sha256.iter().map(|b| format!("{b:02x}")).collect(),
                truncated: stats.truncated,
                exit_code: stats.exit_code,
            },
            LoopEvent::FileChanged {
                call_id,
                path,
                before,
                after,
            } => AuditEvent::FileChange {
                call_id: call_id.clone(),
                path: path.clone(),
                before: before.clone(),
                after: after.clone(),
            },
            _ => return,
        };
        self.record(agent, record);
    }

    /// A model request record, counting the secrets redacted since the
    /// previous one.
    ///
    /// # Arguments
    ///
    /// * `role` - The role the request served.
    /// * `provider` - The provider's name.
    /// * `usage` - Tokens, when reported.
    /// * `stop_reason` - Why the model stopped.
    ///
    /// # Returns
    ///
    /// The record, not yet appended.
    pub fn model_request(
        &self,
        role: &str,
        provider: &str,
        usage: Option<Usage>,
        stop_reason: &StopReason,
    ) -> AuditEvent {
        let now = self.guard.redaction_count();
        let before = self.redactions_seen.swap(now, Ordering::Relaxed);
        let entry = self.providers.get(provider);
        let usage = usage.unwrap_or_default();
        AuditEvent::ModelRequest {
            role: role.to_owned(),
            provider: provider.to_owned(),
            host: entry.map(|e| e.host.clone()).unwrap_or_default(),
            model: entry.map(|e| e.model.clone()).unwrap_or_default(),
            input_tokens: usage.input,
            output_tokens: usage.output,
            cached_tokens: usage.cached,
            stop_reason: stop_label(stop_reason).to_owned(),
            redactions: now.saturating_sub(before),
        }
    }

    /// Records a permission decision and, when the managed policy refused
    /// the call, a policy refusal naming the key.
    ///
    /// # Arguments
    ///
    /// * `decision` - What the permission check decided.
    pub fn decision(&self, decision: &PermissionDecision) {
        let allowed = matches!(
            decision.scope,
            DecisionScope::Once
                | DecisionScope::Session
                | DecisionScope::Saved
                | DecisionScope::Rule
        );
        self.record(
            MAIN_AGENT,
            AuditEvent::PermissionDecision {
                tool: decision.tool.clone(),
                decision: if allowed { "allow" } else { "deny" }.to_owned(),
                scope: scope_label(decision.scope).to_owned(),
                source: decision.source.as_str().to_owned(),
            },
        );
        let key = match decision.source {
            DecisionSource::Rule(RuleOrigin::Policy) if !allowed => "permissions.deny",
            DecisionSource::ShellAllowlist(RuleOrigin::Policy) => "shell.allow",
            _ => return,
        };
        self.record(
            MAIN_AGENT,
            AuditEvent::PolicyRefusal {
                what: format!("a `{}` call", decision.tool),
                key: key.to_owned(),
            },
        );
    }

    /// Records a plan answer.
    ///
    /// # Arguments
    ///
    /// * `decision` - The answer.
    /// * `answered_by` - `user`, `yes_flag` or `build`.
    pub fn plan(&self, decision: &PlanDecision, answered_by: &str) {
        let decision = match decision {
            PlanDecision::Approve => "approved",
            PlanDecision::Revise(_) => "revise",
            PlanDecision::Abort => "rejected",
        };
        self.record(
            MAIN_AGENT,
            AuditEvent::PlanApproval {
                decision: decision.to_owned(),
                answered_by: answered_by.to_owned(),
            },
        );
    }

    /// Records how the session ended.
    ///
    /// # Arguments
    ///
    /// * `outcome` - Such as `done` or `incomplete: review_rounds_exhausted`.
    pub fn end(&self, outcome: &str) {
        self.record(
            MAIN_AGENT,
            AuditEvent::SessionEnd {
                outcome: outcome.to_owned(),
            },
        );
    }

    /// Whether the policy requires the log.
    ///
    /// # Returns
    ///
    /// `[audit] required`.
    pub fn required(&self) -> bool {
        self.required
    }

    /// Why a required log stopped the session, once it has.
    ///
    /// # Returns
    ///
    /// The first failed write, or `None`.
    pub fn failure(&self) -> Option<String> {
        self.stop.borrow().clone()
    }

    /// Resolves once a required log fails; never otherwise.
    ///
    /// # Returns
    ///
    /// The reason.
    pub async fn stopped(&self) -> String {
        let mut watch = self.stop.subscribe();
        match watch.wait_for(Option::is_some).await {
            Ok(reason) => reason.clone().unwrap_or_default(),
            Err(_) => std::future::pending().await,
        }
    }

    /// Appends one record built elsewhere, such as a model request made
    /// outside the loop.
    ///
    /// # Arguments
    ///
    /// * `agent` - `main` or a subagent.
    /// * `event` - The record.
    pub fn record_event(&self, agent: &str, event: AuditEvent) {
        self.record(agent, event);
    }

    /// Appends one record. A failure stops a required log and the session
    /// with it; otherwise it is logged and the record is lost.
    fn record(&self, agent: &str, event: AuditEvent) {
        let Some(log) = &self.log else {
            return;
        };
        if let Err(error) = log.append(agent, event) {
            if self.required {
                tracing::error!(%error, "the audit log cannot be written; stopping the session");
                self.stop.send_if_modified(|reason| {
                    let first = reason.is_none();
                    if first {
                        *reason = Some(error.to_string());
                    }
                    first
                });
            } else {
                tracing::warn!(%error, "an audit record was not written");
            }
        }
    }

    /// An audit over a log the caller built, for tests that make writes fail.
    #[cfg(test)]
    pub fn with_log(wiring: &Wiring, log: AuditLog) -> Self {
        Self {
            required: log.required(),
            log: Some(log),
            mode: RecordMode::Digest,
            guard: wiring.guard.clone(),
            providers: BTreeMap::new(),
            redactions_seen: AtomicU64::new(0),
            stop: watch::channel(None).0,
        }
    }
}

/// The audit file a command names: a path that exists, or a session id
/// of this repository.
fn audit_file(sessions_root: &Path, target: &str) -> std::path::PathBuf {
    let path = Path::new(target);
    if path.is_file() {
        return path.to_path_buf();
    }
    sessions_root.join(target).join(AUDIT_FILE)
}

/// `anvil audit verify`: walks the chain, reports the first break, and
/// compares the file with its mirror when there is one.
///
/// # Arguments
///
/// * `sessions_root` - This repository's sessions folder.
/// * `target` - A session id or an audit file.
/// * `mirror` - A mirrored copy given on the command line.
/// * `mirror_dir` - The policy's `[audit] mirror_dir`, where the copy of a
///   session named by id is looked for when `mirror` is not given; that
///   copy must exist.
///
/// # Returns
///
/// `0` when the chain is intact and the mirror, if any, holds the same
/// bytes; `1` otherwise.
///
/// # Errors
///
/// When a file cannot be read.
pub fn verify_command(
    sessions_root: &Path,
    target: &str,
    mirror: Option<std::path::PathBuf>,
    mirror_dir: Option<&Path>,
) -> anyhow::Result<i32> {
    let file = audit_file(sessions_root, target);
    let mirror = mirror.or_else(|| {
        (!Path::new(target).is_file())
            .then_some(mirror_dir)
            .flatten()
            .map(|dir| mirror_file(dir, target))
    });
    let text = std::fs::read_to_string(&file)
        .with_context(|| format!("reading the audit log {}", file.display()))?;
    let verified = match anvil_audit::audit::verify(&text) {
        Ok(verified) => verified,
        Err(error) => {
            println!("{}: chain broken at {error}", file.display());
            return Ok(1);
        }
    };
    println!(
        "{}: chain intact, {} records, last hash {}",
        file.display(),
        verified.records.len(),
        verified.last_hash()
    );
    let Some(mirror) = mirror else {
        println!(
            "An intact chain shows no record was edited, removed or moved. It cannot show that records were cut from the end or that the whole file was rewritten; pass the mirrored copy with --mirror to compare."
        );
        return Ok(0);
    };
    let copy = match std::fs::read_to_string(&mirror) {
        Ok(copy) => copy,
        Err(error) => {
            println!("{}: the mirror cannot be read: {error}", mirror.display());
            return Ok(1);
        }
    };
    match first_difference(&text, &copy) {
        None => {
            println!(
                "{}: the mirror holds the same {} records",
                mirror.display(),
                verified.records.len()
            );
            Ok(0)
        }
        Some(line) => {
            println!(
                "{}: the mirror differs from the session file at line {line}",
                mirror.display()
            );
            Ok(1)
        }
    }
}

/// The first line, from 1, where two audit files differ; `None` when they
/// are the same bytes.
fn first_difference(primary: &str, mirror: &str) -> Option<usize> {
    if primary == mirror {
        return None;
    }
    let mut left = primary.split('\n');
    let mut right = mirror.split('\n');
    let mut line = 1;
    while left.next() == right.next() {
        line += 1;
    }
    Some(line)
}

/// `anvil audit show`: prints the records readably.
///
/// # Arguments
///
/// * `sessions_root` - This repository's sessions folder.
/// * `target` - A session id or an audit file.
///
/// # Returns
///
/// `0` when the chain is intact, `1` when it is broken.
///
/// # Errors
///
/// When the file cannot be read.
pub fn show_command(sessions_root: &Path, target: &str) -> anyhow::Result<i32> {
    let file = audit_file(sessions_root, target);
    let text = std::fs::read_to_string(&file)
        .with_context(|| format!("reading the audit log {}", file.display()))?;
    print!("{}", anvil_audit::audit::show::render(&text));
    Ok(i32::from(anvil_audit::audit::verify(&text).is_err()))
}

impl anvil_core::tools::PermissionObserver for SessionAudit {
    fn decided(&self, decision: &PermissionDecision) {
        self.decision(decision);
    }
}

impl CallGate for SessionAudit {
    fn refusal(&self) -> Option<String> {
        self.failure()
            .map(|detail| format!("refused: the audit log cannot be written ({detail})"))
    }
}

/// Who and what produces this process's records.
///
/// The user and host come from the environment (`USERNAME` or `USER`,
/// `COMPUTERNAME` or `HOSTNAME`, then `/etc/hostname`), as the operating
/// system reports them to a process; a user who controls the environment
/// can change them.
///
/// # Arguments
///
/// * `policy` - The policy in force, if any.
///
/// # Returns
///
/// The identity.
pub fn identity(policy: Option<&LoadedPolicy>) -> Identity {
    Identity {
        user: first_env(&["USERNAME", "USER", "LOGNAME"]).unwrap_or_else(|| "unknown".to_owned()),
        host: host_name(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        commit: crate::cli::BUILD_COMMIT.unwrap_or("unknown").to_owned(),
        binary_sha256: binary_sha256().to_owned(),
        policy: policy::label(policy),
    }
}

/// The first of `names` that is set to something.
fn first_env(names: &[&str]) -> Option<String> {
    names
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .map(|value| value.trim().to_owned())
        .find(|value| !value.is_empty())
}

/// The host name from the environment or `/etc/hostname`.
fn host_name() -> String {
    first_env(&["COMPUTERNAME", "HOSTNAME"])
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|text| text.trim().to_owned())
                .filter(|text| !text.is_empty())
        })
        .unwrap_or_else(|| "unknown".to_owned())
}

/// SHA-256 of the running binary, computed once per process.
fn binary_sha256() -> &'static str {
    static HASH: OnceLock<String> = OnceLock::new();
    HASH.get_or_init(|| {
        let digest = std::env::current_exe().and_then(|path| {
            let mut file = std::fs::File::open(path)?;
            let mut hasher = sha2::Sha256::new();
            let mut buffer = vec![0u8; 64 * 1024];
            loop {
                let read = file.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                hasher.update(&buffer[..read]);
            }
            Ok(hasher.finalize())
        });
        match digest {
            Ok(digest) => digest.iter().map(|b| format!("{b:02x}")).collect(),
            Err(error) => {
                tracing::warn!(%error, "cannot hash the running binary");
                "unknown".to_owned()
            }
        }
    })
}

/// The `host:port` of a base URL, with the scheme's default port.
fn host_of(base_url: &str) -> String {
    let (scheme, rest) = base_url.split_once("://").unwrap_or(("https", base_url));
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let has_port = authority
        .rsplit_once(':')
        .is_some_and(|(_, port)| port.parse::<u16>().is_ok())
        && !authority.ends_with(']');
    if has_port || authority.is_empty() {
        authority.to_ascii_lowercase()
    } else {
        let port = if scheme.eq_ignore_ascii_case("http") {
            80
        } else {
            443
        };
        format!("{}:{port}", authority.to_ascii_lowercase())
    }
}

/// The record name of a scope.
fn scope_label(scope: DecisionScope) -> &'static str {
    match scope {
        DecisionScope::Once => "once",
        DecisionScope::Session => "session",
        DecisionScope::Saved => "saved",
        DecisionScope::Rule => "rule",
        DecisionScope::Deny => "deny",
        DecisionScope::Blocked => "blocked",
    }
}

/// The record name of a stop reason.
fn stop_label(reason: &StopReason) -> &'static str {
    match reason {
        StopReason::EndTurn => "end_turn",
        StopReason::ToolUse => "tool_use",
        StopReason::MaxTokens => "max_tokens",
        StopReason::Other(_) => "other",
    }
}

/// The record name of a tool outcome.
fn outcome_label(outcome: ToolOutcome) -> &'static str {
    match outcome {
        ToolOutcome::Completed => "completed",
        ToolOutcome::Failed => "failed",
        ToolOutcome::DeniedByRule => "denied_by_rule",
        ToolOutcome::DeclinedByUser => "declined_by_user",
        ToolOutcome::Blocked => "blocked",
        ToolOutcome::UnknownTool => "unknown_tool",
        ToolOutcome::InvalidArguments => "invalid_arguments",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_of_adds_the_default_port_and_keeps_an_explicit_one() {
        assert_eq!(host_of("https://Models.Example/v1"), "models.example:443");
        assert_eq!(host_of("http://localhost:8080/v1"), "localhost:8080");
        assert_eq!(host_of("http://10.0.0.1/v1"), "10.0.0.1:80");
        assert_eq!(host_of("https://[::1]:9000/v1"), "[::1]:9000");
    }

    #[test]
    fn the_identity_names_this_build_and_the_policy_label() {
        let identity = identity(None);
        assert_eq!(identity.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(identity.policy, "unmanaged");
        assert!(
            identity.binary_sha256 == "unknown"
                || (identity.binary_sha256.len() == 64
                    && identity
                        .binary_sha256
                        .bytes()
                        .all(|b| b.is_ascii_hexdigit())),
            "{}",
            identity.binary_sha256
        );
        assert!(!identity.user.is_empty() && !identity.host.is_empty());
    }
}
