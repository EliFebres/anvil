//! The session audit log end to end: what a run writes, a required log
//! that fails, one that cannot be opened, and subagents.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anvil_audit::SessionOutcome;
use anvil_audit::audit::{AUDIT_FILE, AuditEvent, AuditLog, SealedRecord, sha256_hex, verify};
use anvil_core::agent::{AgentLoop, AutoApprove, CheckCommand, LoopConfig, LoopProviders, Status};
use anvil_core::provider::{FakeProvider, Provider, ToolCall};
use anvil_core::tools::{
    CallGate, Permissions, PromptAnswer, StaticPrompt, ToolContext, ToolRegistry,
};
use anvil_types::LimitsConfig;

use super::SessionUsage;
use crate::audit::{SessionAudit, identity};
use crate::console::Console;
use crate::wiring::{ProjectAccess, Wiring};

/// Appears in the task and the tool arguments; a digest-mode log must
/// never contain it.
const MARKER: &str = "MARKER_audit_9c1d";

const PLAN: &str = "Looked around.\n\n# Plan\n\n1. [ ] Write hello.txt\n";

const APPROVE: &str = "```json\n{\"verdict\": \"approve\"}\n```";

/// A wiring over the configuration template, with `dir/project` as the
/// project and `dir/home` as an empty home, so nothing of the machine's
/// own home is read.
fn wiring_in(dir: &Path) -> Wiring {
    let config: anvil_types::AnvilConfig =
        toml::from_str(anvil_types::config::CONFIG_TEMPLATE).expect("config parses");
    let project = dir.join("project");
    let home = dir.join("home");
    std::fs::create_dir_all(&project).expect("project");
    std::fs::create_dir_all(&home).expect("home");
    Wiring::new_in(project, Some(home), config, ProjectAccess::default()).expect("wiring")
}

fn usage_for(cwd: &Path, session: &str) -> SessionUsage {
    SessionUsage::new(cwd, None, session, "fast", None, "fast", BTreeMap::new())
}

fn write_call(id: &str, path: &str) -> ToolCall {
    ToolCall {
        id: id.to_owned(),
        name: "write_file".to_owned(),
        arguments_json: serde_json::json!({ "path": path, "content": format!("hi {MARKER}") })
            .to_string(),
    }
}

fn loop_config(cwd: &Path, session_dir: PathBuf) -> LoopConfig {
    LoopConfig {
        task: format!("write hello.txt {MARKER}"),
        cwd: cwd.to_path_buf(),
        session_dir,
        limits: LimitsConfig::default(),
        check: CheckCommand::Command("exit 0".to_owned()),
        context_window: 100_000,
        home_dir: None,
        trusted_project: true,
        extra_instructions: Vec::new(),
        sections: Vec::new(),
        review_sections: Vec::new(),
        delegate_guidance: false,
    }
}

fn read_chain(path: &Path) -> Vec<SealedRecord> {
    let text = std::fs::read_to_string(path).expect("audit file");
    verify(&text).expect("the chain verifies").records
}

fn position(records: &[SealedRecord], tag: &str) -> usize {
    records
        .iter()
        .position(|record| record.body.event.tag() == tag)
        .unwrap_or_else(|| panic!("no {tag} record"))
}

#[tokio::test]
async fn a_run_is_audited_in_order_and_its_log_verifies() {
    let cwd = tempfile::tempdir().expect("cwd");
    let session = ulid::Ulid::generate().to_string();
    let session_dir = cwd.path().join(".anvil").join("sessions").join(&session);
    let wiring = wiring_in(cwd.path());
    let audit = SessionAudit::open(&wiring, &session, &session_dir).expect("audit opens");
    audit.start(&wiring, "run", true);
    let usage = usage_for(cwd.path(), &session).with_audit(audit);
    usage.prompt(&format!("write hello.txt {MARKER}"));

    let coder = Arc::new(FakeProvider::new(vec![
        FakeProvider::text_turn(PLAN),
        FakeProvider::tool_turn(vec![write_call("w1", "hello.txt")]),
        FakeProvider::text_turn("built"),
    ]));
    let reviewer = Arc::new(FakeProvider::new(vec![FakeProvider::text_turn(APPROVE)]));
    let mut registry = ToolRegistry::with_builtins(
        Permissions::from_toml("allow = [\"write_file\"]", None).expect("rules"),
        Box::new(StaticPrompt(PromptAnswer::No)),
    );
    registry.set_observer(Arc::new(usage.clone()));
    let (sink, tee) = usage.sink(None);
    let agent = AgentLoop::new(
        loop_config(cwd.path(), session_dir.clone()),
        LoopProviders {
            coder: coder as Arc<dyn Provider>,
            planner: None,
            reviewer: reviewer as Arc<dyn Provider>,
            summarizer: Arc::new(FakeProvider::new(vec![])) as Arc<dyn Provider>,
        },
        registry,
        ToolContext::new(cwd.path()),
        usage.gate("yes_flag", Box::new(AutoApprove)),
        sink,
    );
    let result = agent.run().await;
    tee.await.expect("tee ends");
    assert_eq!(result.status, Status::Done);
    usage.finish(SessionOutcome::Done);

    let path = session_dir.join(AUDIT_FILE);
    let records = read_chain(&path);
    assert_eq!(records[0].body.event.tag(), "session_start");
    assert_eq!(
        records.last().map(|record| record.body.event.tag()),
        Some("session_end")
    );
    assert!(records.iter().all(|record| record.body.session == session));
    assert!(records.iter().all(|record| record.body.agent == "main"));
    let prompt = position(&records, "prompt_submitted");
    let plan = position(&records, "plan_approval");
    let call = position(&records, "tool_call");
    let decision = position(&records, "permission_decision");
    let result = position(&records, "tool_result");
    let change = position(&records, "file_change");
    assert!(prompt < plan && plan < call, "{records:#?}");
    assert!(
        call < decision && decision < result && result < change,
        "{records:#?}"
    );
    let models = records
        .iter()
        .filter(|record| record.body.event.tag() == "model_request")
        .count();
    assert!(models >= 3, "plan, build and review requests: {models}");
    match &records[decision].body.event {
        AuditEvent::PermissionDecision {
            tool,
            decision,
            source,
            ..
        } => {
            assert_eq!(tool, "write_file");
            assert_eq!(decision, "allow");
            assert_eq!(source, "user_rule");
        }
        other => panic!("{other:?}"),
    }
    match &records[change].body.event {
        AuditEvent::FileChange {
            path,
            before,
            after,
            ..
        } => {
            assert_eq!(path, "hello.txt");
            assert_eq!(before, &None);
            assert_eq!(after, &sha256_hex(format!("hi {MARKER}").as_bytes()));
        }
        other => panic!("{other:?}"),
    }
    match &records[plan].body.event {
        AuditEvent::PlanApproval {
            decision,
            answered_by,
        } => {
            assert_eq!(decision, "approved");
            assert_eq!(answered_by, "yes_flag");
        }
        other => panic!("{other:?}"),
    }
    let text = std::fs::read_to_string(&path).expect("read");
    assert!(
        !text.contains(MARKER),
        "digest mode keeps no prompt or argument text"
    );
}

/// A writer that accepts `left` writes and then fails every one.
struct FailAfter(AtomicUsize);

impl Write for FailAfter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.0.load(Ordering::SeqCst) == 0 {
            return Err(std::io::Error::other("the audit disk is full"));
        }
        self.0.fetch_sub(1, Ordering::SeqCst);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn a_required_audit_sink_that_fails_stops_the_session() {
    let cwd = tempfile::tempdir().expect("cwd");
    let session = ulid::Ulid::generate().to_string();
    let session_dir = cwd.path().join(".anvil").join("sessions").join(&session);
    let wiring = wiring_in(cwd.path());
    // Enough for the first plan turn and its approval, not for the build.
    let log = AuditLog::with_sinks(
        &session,
        identity(None),
        true,
        vec![(
            PathBuf::from("mirror"),
            Box::new(FailAfter(AtomicUsize::new(2))),
        )],
    );
    let audit = Arc::new(SessionAudit::with_log(&wiring, log));
    let usage = usage_for(cwd.path(), &session).with_shared_audit(Arc::clone(&audit));
    usage.observe_permissions(&wiring);

    let calls: Vec<_> = (0..4)
        .map(|n| FakeProvider::tool_turn(vec![write_call(&format!("w{n}"), &format!("f{n}.txt"))]))
        .collect();
    let mut scripts = vec![FakeProvider::text_turn(PLAN)];
    scripts.extend(calls);
    scripts.push(FakeProvider::text_turn("built"));
    let coder = Arc::new(FakeProvider::new(scripts));
    let gate = Arc::clone(&audit) as Arc<dyn CallGate>;
    let gated = crate::wiring::gated(Arc::clone(&coder) as Arc<dyn Provider>, Arc::clone(&gate));
    let mut registry = ToolRegistry::with_builtins(
        Permissions::from_toml("allow = [\"write_file\"]", None).expect("rules"),
        Box::new(StaticPrompt(PromptAnswer::No)),
    );
    registry.set_observer(Arc::new(usage.clone()));
    registry.set_gate(Arc::clone(&gate));
    let (sink, tee) = usage.sink(None);
    let agent = AgentLoop::new(
        loop_config(cwd.path(), session_dir.clone()),
        LoopProviders {
            coder: Arc::clone(&gated),
            planner: None,
            reviewer: Arc::clone(&gated),
            summarizer: gated,
        },
        registry,
        ToolContext::new(cwd.path()),
        usage.gate("yes_flag", Box::new(AutoApprove)),
        sink,
    );
    let result = crate::commands::drive(agent, None, &session_dir, &Console::Stderr, &usage).await;
    let _ = tee.await;

    assert_ne!(result.status, Status::Done, "{:?}", result.status);
    assert_ne!(result.status.exit_code(), 0);
    let failure = usage.audit_failure().expect("the failure is kept");
    assert!(failure.contains("the audit disk is full"), "{failure}");
    assert!(crate::commands::unaudited(&usage).is_err());
    let written: Vec<String> = (0..4)
        .map(|n| format!("f{n}.txt"))
        .filter(|name| cwd.path().join(name).exists())
        .collect();
    assert!(
        written.is_empty(),
        "no tool runs after the failed write: {written:?}"
    );
    assert_eq!(
        coder.requests().len(),
        2,
        "the plan and the build turn whose record failed; nothing after"
    );
    assert!(gate.refusal().is_some());
}

#[test]
fn a_required_audit_log_that_cannot_be_opened_stops_the_session_before_it_starts() {
    let dir = tempfile::tempdir().expect("dir");
    let blocker = dir.path().join("not-a-folder");
    std::fs::write(&blocker, "x").expect("write");
    let policy_path = dir.path().join("policy.toml");
    std::fs::write(
        &policy_path,
        format!(
            "version = 1\n[network]\nallowed_hosts = [\"models.internal.example:443\"]\n[audit]\nrequired = true\nmirror_dir = '{}'\n",
            blocker.join("audit").display()
        ),
    )
    .expect("write policy");
    let policy = anvil_types::policy::load(&policy_path)
        .expect("valid")
        .expect("present");
    let wiring = wiring_in(dir.path())
        .with_policy(Arc::new(policy))
        .expect("policy applies");
    let session = ulid::Ulid::generate().to_string();
    let session_dir = dir.path().join("sessions").join(&session);
    let bound = wiring.with_session(&session, &session_dir);
    let error = SessionUsage::start(&bound, &session, anvil_audit::SessionMode::Run, true)
        .err()
        .expect("the session must not start");
    let text = format!("{error:#}");
    assert!(text.contains("[audit] required = true"), "{text}");
    let written = std::fs::read_to_string(session_dir.join(AUDIT_FILE)).unwrap_or_default();
    assert!(
        written.is_empty(),
        "no record before the refusal: {written}"
    );

    let lenient = dir.path().join("lenient.toml");
    std::fs::write(
        &lenient,
        format!(
            "version = 1\n[network]\nallowed_hosts = [\"models.internal.example:443\"]\n[audit]\nrequired = false\nmirror_dir = '{}'\n",
            blocker.join("audit").display()
        ),
    )
    .expect("write policy");
    let policy = anvil_types::policy::load(&lenient)
        .expect("valid")
        .expect("present");
    let wiring = wiring_in(dir.path())
        .with_policy(Arc::new(policy))
        .expect("policy applies");
    let bound = wiring.with_session(&session, &session_dir);
    SessionUsage::start(&bound, &session, anvil_audit::SessionMode::Run, true)
        .expect("not required: the session starts unaudited");
}

#[tokio::test]
async fn subagents_are_confined_and_audited() {
    let dir = tempfile::tempdir().expect("dir");
    let script = dir.path().join("script.json");
    std::fs::write(
        &script,
        serde_json::json!({ "turns": [
            { "tool_calls": [
                { "name": "read_file", "arguments": { "path": "a.txt" } },
                { "name": "read_file", "arguments": { "path": "../outside.txt" } }
            ] },
            { "text": "found it" }
        ] })
        .to_string(),
    )
    .expect("script");
    let config: anvil_types::AnvilConfig = toml::from_str(&format!(
        "[providers.scripted]\nprotocol = \"fake\"\nscript = '{}'\nmodel = \"m\"\ncontext_window = 100000\nsupports_tools = true\nsupports_reasoning = false\n[roles]\ncoder = \"scripted\"\nsummarizer = \"scripted\"\n",
        script.display()
    ))
    .expect("config parses");
    let project = dir.path().join("project");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&project).expect("project");
    std::fs::create_dir_all(&home).expect("home");
    std::fs::write(project.join("a.txt"), "inside\n").expect("write");
    std::fs::write(dir.path().join("outside.txt"), "outside\n").expect("write");
    let wiring = Wiring::new_in(
        project.clone(),
        Some(home),
        config,
        ProjectAccess::default(),
    )
    .expect("wiring");
    let session = ulid::Ulid::generate().to_string();
    let session_dir = project.join(".anvil").join("sessions").join(&session);
    let audit = SessionAudit::open(&wiring, &session, &session_dir).expect("audit opens");
    let usage = SessionUsage::new(
        &project,
        None,
        &session,
        "scripted",
        None,
        "scripted",
        BTreeMap::new(),
    )
    .with_audit(audit);
    usage.observe_permissions(&wiring);

    let explore = wiring
        .agents
        .get("explore")
        .expect("built-in agent")
        .clone();
    let run = anvil_compat::delegate::run_subagent(&explore, "find it", &wiring).await;
    assert_eq!(run.result, Ok("found it".to_owned()));

    let records = read_chain(&session_dir.join(AUDIT_FILE));
    let by_explore: Vec<&SealedRecord> = records
        .iter()
        .filter(|record| record.body.agent == "explore")
        .collect();
    let tags: Vec<&str> = by_explore.iter().map(|r| r.body.event.tag()).collect();
    assert_eq!(
        tags,
        [
            "model_request",
            "tool_call",
            "tool_result",
            "tool_call",
            "tool_result",
            "model_request"
        ],
        "{records:#?}"
    );
    let results: Vec<(String, bool)> = by_explore
        .iter()
        .filter_map(|record| match &record.body.event {
            AuditEvent::ToolResult { tool, error, .. } => Some((tool.clone(), *error)),
            _ => None,
        })
        .collect();
    assert_eq!(
        results,
        [
            ("read_file".to_owned(), false),
            ("read_file".to_owned(), true)
        ],
        "the read outside the workspace is refused, and that is audited too"
    );
    assert!(by_explore.iter().any(|record| matches!(
        &record.body.event,
        AuditEvent::ModelRequest { role, provider, .. } if role == "worker" && provider == "scripted"
    )));
}

#[tokio::test]
async fn permission_records_name_their_source_and_policy_refusals_their_key() {
    let cwd = tempfile::tempdir().expect("cwd");
    let session = ulid::Ulid::generate().to_string();
    let session_dir = cwd.path().join("session");
    let wiring = wiring_in(cwd.path());
    let audit = SessionAudit::open(&wiring, &session, &session_dir).expect("audit opens");
    let usage = usage_for(cwd.path(), &session).with_audit(audit);
    let ctx = ToolContext::new(cwd.path());
    let call = |id: &str, name: &str, args: serde_json::Value| ToolCall {
        id: id.to_owned(),
        name: name.to_owned(),
        arguments_json: args.to_string(),
    };

    let mut by_policy = Permissions::empty();
    by_policy
        .tighten_as(
            anvil_core::tools::RuleOrigin::Policy,
            &["shell".to_owned()],
            &[],
            &[],
        )
        .expect("rules");
    by_policy
        .tighten(&["edit_file".to_owned()], &[], &[])
        .expect("rules");
    let mut registry =
        ToolRegistry::with_builtins(by_policy, Box::new(StaticPrompt(PromptAnswer::Yes)));
    registry.set_observer(Arc::new(usage.clone()));
    registry
        .execute(
            &call("s1", "shell", serde_json::json!({"command": "echo hi"})),
            &ctx,
        )
        .await;
    registry
        .execute(
            &call(
                "e1",
                "edit_file",
                serde_json::json!({"path": "a", "old": "x", "new": "y"}),
            ),
            &ctx,
        )
        .await;
    registry
        .execute(
            &call(
                "w1",
                "write_file",
                serde_json::json!({"path": "b.txt", "content": "z"}),
            ),
            &ctx,
        )
        .await;

    let records = read_chain(&session_dir.join(AUDIT_FILE));
    let decisions: Vec<(String, String, String)> = records
        .iter()
        .filter_map(|record| match &record.body.event {
            AuditEvent::PermissionDecision {
                tool,
                decision,
                source,
                ..
            } => Some((tool.clone(), decision.clone(), source.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        decisions,
        [
            (
                "shell".to_owned(),
                "deny".to_owned(),
                "policy_rule".to_owned()
            ),
            (
                "edit_file".to_owned(),
                "deny".to_owned(),
                "project_rule".to_owned()
            ),
            (
                "write_file".to_owned(),
                "allow".to_owned(),
                "yes_flag".to_owned()
            ),
        ]
    );
    let refusals: Vec<&AuditEvent> = records
        .iter()
        .map(|record| &record.body.event)
        .filter(|event| event.tag() == "policy_refusal")
        .collect();
    assert_eq!(
        refusals,
        [&AuditEvent::PolicyRefusal {
            what: "a `shell` call".to_owned(),
            key: "permissions.deny".to_owned(),
        }],
        "only the policy's own refusal names a policy key"
    );
}

#[tokio::test]
async fn the_wiring_attaches_a_failed_required_logs_gate_to_tools_providers_and_hooks() {
    use futures_util::StreamExt;

    let dir = tempfile::tempdir().expect("dir");
    let project = dir.path().join("project");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&project).expect("project");
    let script = dir.path().join("script.json");
    std::fs::write(&script, r#"{"turns": [{"text": "hello"}]}"#).expect("script");
    let hook = home.join(".claude").join("settings.json");
    std::fs::create_dir_all(hook.parent().expect("parent")).expect("mkdir");
    std::fs::write(
        &hook,
        r#"{"hooks": {"SessionStart": [{"hooks": [{"type": "command", "command": "git init -q hook-ran"}]}]}}"#,
    )
    .expect("hook");
    let config: anvil_types::AnvilConfig = toml::from_str(&format!(
        "{}\n[providers.scripted]\nprotocol = \"fake\"\nmodel = \"m\"\ncontext_window = 1000\nsupports_tools = true\nsupports_reasoning = false\nscript = '{}'\n",
        anvil_types::config::CONFIG_TEMPLATE,
        script.display()
    ))
    .expect("config parses");
    let wiring = Wiring::new_in(
        project.clone(),
        Some(home),
        config,
        ProjectAccess::default(),
    )
    .expect("wiring");
    let session = ulid::Ulid::generate().to_string();
    let log = AuditLog::with_sinks(
        &session,
        identity(None),
        true,
        vec![(
            PathBuf::from("mirror"),
            Box::new(FailAfter(AtomicUsize::new(0))),
        )],
    );
    let audit = Arc::new(SessionAudit::with_log(&wiring, log));
    let usage = usage_for(&project, &session).with_shared_audit(Arc::clone(&audit));
    // The production path: the session's usage hands the required log to
    // the wiring, and everything below is built by the wiring itself.
    usage.observe_permissions(&wiring);
    // The session's first record cannot be written, which closes the gate.
    audit.start(&wiring, "run", false);
    assert!(audit.failure().is_some(), "the start record failed");

    let registry = wiring
        .registry(Box::new(StaticPrompt(PromptAnswer::Yes)))
        .expect("registry");
    let call = ToolCall {
        id: "r1".to_owned(),
        name: "list_dir".to_owned(),
        arguments_json: r#"{"path": "."}"#.to_owned(),
    };
    let run = registry.execute(&call, &wiring.tool_context()).await;
    assert!(run.result.is_error, "{}", run.result.content);
    assert!(
        run.result.content.contains("audit"),
        "{}",
        run.result.content
    );

    let provider = wiring.provider("scripted").expect("provider");
    let first = provider
        .complete(anvil_core::provider::CompletionRequest {
            system: String::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            max_output_tokens: None,
        })
        .next()
        .await;
    assert!(
        matches!(
            first,
            Some(Err(anvil_core::provider::ProviderError::Refused { .. }))
        ),
        "the provider answered after the log failed: {first:?}"
    );

    let runner = wiring.hook_runner().expect("the user's hook loads");
    let _ = runner.session_start().await;
    assert!(
        !project.join("hook-ran").exists(),
        "a hook ran after the log failed"
    );
}
