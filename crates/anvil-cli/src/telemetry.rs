//! Telemetry wiring: loop events become content-free records, permission
//! answers are recorded as they happen, and a finished session is
//! classified into signals for `anvil ticket draft`.
//!
//! Everything here translates; the rules that keep content out of the
//! files live in `anvil_telemetry::record`.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anvil_core::agent::{EventSink, IncompleteReason, LoopEvent, ReviewVerdict, Status};
use anvil_core::provider::{
    CompletionRequest, Event, EventStream, Provider, ProviderError, StopReason,
};
use anvil_core::tools::{
    PermissionPrompt, PermissionRequest, PromptAnswer, StaticPrompt, ToolOutcome, ToolRegistry,
    permissions,
};
use anvil_telemetry::paths::signals_file;
use anvil_telemetry::{
    PermissionAnswer, RecordKind, Recorder, SessionMode, SessionOutcome, StopKind, Tokens,
    ToolOutcomeKind, WarningClass, append_signals, classify, load_records, session_records,
};
use anvil_types::TelemetryConfig;
use chrono::{Duration, Utc};
use futures_util::StreamExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::terminal::TerminalPrompt;
use crate::wiring::Wiring;

/// How far back the cross-session history reaches.
const HISTORY_DAYS: i64 = 90;

/// Which provider plays which role, so a turn can be attributed.
#[derive(Debug, Clone)]
struct Roles {
    coder: String,
    reviewer: String,
    summarizer: String,
}

/// The telemetry of one session.
///
/// Cloning shares the recorder and the role names, so the event tee, the
/// permission prompt and the chat loop all see the same state.
#[derive(Clone)]
pub struct SessionTelemetry {
    recorder: Arc<Recorder>,
    roles: Arc<Mutex<Roles>>,
    /// Provider name to model name, from config.
    models: Arc<BTreeMap<String, String>>,
    thresholds: TelemetryConfig,
}

impl SessionTelemetry {
    /// Opens the recorder for a session and records its start.
    ///
    /// # Arguments
    ///
    /// * `wiring` - Config and paths.
    /// * `session` - The session id.
    /// * `mode` - How the session was started.
    ///
    /// # Returns
    ///
    /// The telemetry handle.
    pub fn start(wiring: &Wiring, session: &str, mode: SessionMode) -> Self {
        let this = Self::new(
            &wiring.cwd,
            wiring.home.as_deref(),
            session,
            &wiring.config.roles.coder,
            wiring.config.roles.reviewer.as_deref(),
            &wiring.config.roles.summarizer,
            wiring
                .config
                .providers
                .iter()
                .map(|(name, provider)| (name.clone(), provider.model.clone()))
                .collect(),
            wiring.config.telemetry,
        );
        this.record(RecordKind::SessionStarted { mode });
        this
    }

    /// Builds the handle without touching a [`Wiring`], for tests.
    #[allow(clippy::too_many_arguments)]
    fn new(
        cwd: &Path,
        home: Option<&Path>,
        session: &str,
        coder: &str,
        reviewer: Option<&str>,
        summarizer: &str,
        models: BTreeMap<String, String>,
        thresholds: TelemetryConfig,
    ) -> Self {
        Self {
            recorder: Arc::new(Recorder::open(cwd, home, session)),
            roles: Arc::new(Mutex::new(Roles {
                coder: coder.to_owned(),
                reviewer: reviewer.unwrap_or(coder).to_owned(),
                summarizer: summarizer.to_owned(),
            })),
            models: Arc::new(models),
            thresholds,
        }
    }

    /// Records one event.
    ///
    /// # Arguments
    ///
    /// * `kind` - What happened.
    pub fn record(&self, kind: RecordKind) {
        self.recorder.record(kind);
    }

    /// Switches the coder, as chat's `/model` does.
    ///
    /// # Arguments
    ///
    /// * `name` - The provider name.
    pub fn set_coder(&self, name: &str) {
        if let Ok(mut roles) = self.roles.lock() {
            roles.coder = name.to_owned();
        }
    }

    /// Records every skill `requires` entry no registered tool satisfies.
    ///
    /// # Arguments
    ///
    /// * `wiring` - The skill set.
    /// * `registry` - The registered tools.
    pub fn record_skill_requires(&self, wiring: &Wiring, registry: &ToolRegistry) {
        for (skill, tool) in wiring.skills.missing_requires(&registry.names()) {
            self.record(RecordKind::SkillRequires { skill, tool });
        }
    }

    /// Records a provider failure.
    ///
    /// # Arguments
    ///
    /// * `provider` - The provider name.
    /// * `error` - The failure; only its kind and HTTP status are kept.
    pub fn record_provider_error(&self, provider: &str, error: &ProviderError) {
        let (kind, http_status) = error_kind(error);
        self.record(RecordKind::ProviderError {
            provider: provider.to_owned(),
            kind,
            http_status,
        });
    }

    /// The permission prompt, with every answer recorded.
    ///
    /// # Arguments
    ///
    /// * `yes` - Whether the run is unattended; then every call is approved
    ///   without asking (the built-in block list still applies).
    ///
    /// # Returns
    ///
    /// The prompt.
    pub fn prompt(&self, yes: bool) -> Box<dyn PermissionPrompt> {
        let inner: Box<dyn PermissionPrompt> = if yes {
            Box::new(StaticPrompt(PromptAnswer::Yes))
        } else {
            Box::new(TerminalPrompt)
        };
        self.recording(inner)
    }

    /// Wraps a prompt so every answer is recorded.
    ///
    /// # Arguments
    ///
    /// * `inner` - Who actually answers.
    ///
    /// # Returns
    ///
    /// The recording prompt.
    pub fn recording(&self, inner: Box<dyn PermissionPrompt>) -> Box<dyn PermissionPrompt> {
        Box::new(RecordingPrompt {
            inner,
            telemetry: self.clone(),
        })
    }

    /// A sink that records every event and forwards it downstream.
    ///
    /// The returned task ends once the sink is dropped and every event has
    /// been recorded and forwarded; await it before classifying.
    ///
    /// # Arguments
    ///
    /// * `downstream` - Where to forward events, such as a renderer.
    ///
    /// # Returns
    ///
    /// The sink and the task.
    pub fn sink(
        &self,
        downstream: Option<mpsc::UnboundedSender<LoopEvent>>,
    ) -> (EventSink, JoinHandle<()>) {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let this = self.clone();
        let task = tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                if let Some(kind) = this.translate(&event) {
                    this.record(kind);
                }
                if let Some(downstream) = &downstream {
                    let _ = downstream.send(event);
                }
            }
        });
        (EventSink::new(tx), task)
    }

    /// Wraps a provider so its completions are recorded as turns of `state`.
    ///
    /// # Arguments
    ///
    /// * `name` - The provider name.
    /// * `provider` - The provider.
    /// * `state` - The label for the turn records, such as `memory`.
    ///
    /// # Returns
    ///
    /// The wrapped provider.
    pub fn recorded(
        &self,
        name: &str,
        provider: Arc<dyn Provider>,
        state: &'static str,
    ) -> Arc<dyn Provider> {
        Arc::new(RecordedProvider {
            inner: provider,
            telemetry: self.clone(),
            provider: name.to_owned(),
            state,
        })
    }

    /// Records how a run ended, then classifies the session.
    ///
    /// # Arguments
    ///
    /// * `status` - The loop's final status.
    pub fn finish_run(&self, status: &Status) {
        if let Status::ProviderError { error } = status {
            let coder = self
                .roles
                .lock()
                .map(|roles| roles.coder.clone())
                .unwrap_or_default();
            self.record_provider_error(&coder, error);
        }
        self.finish(session_outcome(status));
    }

    /// Records how a session ended, then classifies it. Problems are
    /// reported as warnings and never fail the command.
    ///
    /// # Arguments
    ///
    /// * `outcome` - How the session ended.
    pub fn finish(&self, outcome: SessionOutcome) {
        self.record(RecordKind::SessionEnded { outcome });
        match self.classify_session() {
            Ok(0) => {}
            Ok(count) => eprintln!(
                "telemetry: {count} signal(s) recorded; `anvil ticket draft` turns them into tickets"
            ),
            Err(error) => eprintln!("warning: telemetry classification skipped: {error}"),
        }
    }

    /// Runs the classifier and appends its signals to the project.
    fn classify_session(&self) -> Result<usize, anvil_telemetry::TelemetryError> {
        let since = Some(Utc::now() - Duration::days(HISTORY_DAYS));
        let project = load_records(self.recorder.project_dir(), since)?;
        let session = session_records(&project, self.recorder.session());
        let history = match self.recorder.home_dir() {
            Some(dir) => load_records(dir, since)?,
            None => project,
        };
        let signals = classify(&session, &history, &self.thresholds);
        append_signals(&signals_file(self.recorder.project_dir()), &signals)?;
        Ok(signals.len())
    }

    /// The project-local telemetry folder.
    #[cfg(test)]
    fn project_dir(&self) -> std::path::PathBuf {
        self.recorder.project_dir().to_path_buf()
    }

    /// The provider and model that serve a state.
    fn provider_for(&self, state: &str) -> (String, String) {
        let name = match self.roles.lock() {
            Ok(roles) => match state {
                "review" => roles.reviewer.clone(),
                "compact" => roles.summarizer.clone(),
                _ => roles.coder.clone(),
            },
            Err(_) => String::new(),
        };
        let model = self.models.get(&name).cloned().unwrap_or_default();
        (name, model)
    }

    /// The record for a loop event, or `None` for events that carry only
    /// text or paths.
    fn translate(&self, event: &LoopEvent) -> Option<RecordKind> {
        Some(match event {
            LoopEvent::StateEntered { state, visit } => RecordKind::StateEntered {
                state: state.to_string(),
                visit: *visit,
            },
            LoopEvent::TurnFinished {
                state,
                stop_reason,
                usage,
                latency_ms,
                retries,
            } => {
                let (provider, model) = self.provider_for(&state.to_string());
                RecordKind::Turn {
                    state: state.to_string(),
                    provider,
                    model,
                    tokens: usage.map(tokens).unwrap_or_default(),
                    latency_ms: *latency_ms,
                    retries: *retries,
                    stop_reason: stop_kind(stop_reason),
                }
            }
            LoopEvent::ToolStarted { call } => {
                RecordKind::tool_call(&call.name, &call.arguments_json)
            }
            LoopEvent::ToolFinished {
                name,
                outcome,
                preview,
                stats,
                ..
            } => RecordKind::tool_result(
                name,
                outcome_kind(*outcome),
                preview,
                stats.bytes,
                stats.truncated,
                stats.digest,
            ),
            LoopEvent::ReviewVerdict { verdict } => match verdict {
                ReviewVerdict::Approve => RecordKind::review(true, &[]),
                ReviewVerdict::Changes { items } => RecordKind::review(false, items),
            },
            LoopEvent::CheckFinished {
                command, passed, ..
            } => RecordKind::check(command, *passed),
            LoopEvent::Compacted {
                dropped_messages,
                usage,
                ..
            } => {
                let (provider, model) = self.provider_for("compact");
                RecordKind::Compacted {
                    dropped: *dropped_messages,
                    provider,
                    model,
                    tokens: usage.map(tokens),
                }
            }
            LoopEvent::Warning(message) => RecordKind::Warning {
                class: WarningClass::of(message),
            },
            LoopEvent::Transition { .. }
            | LoopEvent::TextDelta(_)
            | LoopEvent::ReasoningDelta(_)
            | LoopEvent::PlanWritten { .. }
            | LoopEvent::CheckStarted { .. }
            | LoopEvent::Finished { .. } => return None,
        })
    }
}

/// Records each answer before handing it back to the registry.
struct RecordingPrompt {
    inner: Box<dyn PermissionPrompt>,
    telemetry: SessionTelemetry,
}

impl PermissionPrompt for RecordingPrompt {
    fn ask(&self, request: &PermissionRequest) -> PromptAnswer {
        let answer = self.inner.ask(request);
        let rule = permissions::always_rule(&request.tool, request.target.as_deref());
        let recorded = match answer {
            PromptAnswer::Yes => PermissionAnswer::Yes,
            PromptAnswer::No => PermissionAnswer::No,
            PromptAnswer::Always => PermissionAnswer::Always,
        };
        self.telemetry
            .record(RecordKind::permission(&request.tool, &rule, recorded));
        answer
    }
}

/// A provider whose completions outside the loop are recorded as turns.
struct RecordedProvider {
    inner: Arc<dyn Provider>,
    telemetry: SessionTelemetry,
    provider: String,
    state: &'static str,
}

impl Provider for RecordedProvider {
    fn complete(&self, request: CompletionRequest) -> EventStream {
        let telemetry = self.telemetry.clone();
        let provider = self.provider.clone();
        let model = telemetry.models.get(&provider).cloned().unwrap_or_default();
        let state = self.state;
        let started = Instant::now();
        let mut usage = Tokens::default();
        let mut retries = 0;
        Box::pin(
            self.inner
                .complete(request)
                .inspect(move |item| match item {
                    Ok(Event::Usage(reported)) => usage = tokens(*reported),
                    Ok(Event::Retried { retries: count }) => retries = *count,
                    Ok(Event::Done { stop_reason }) => telemetry.record(RecordKind::Turn {
                        state: state.to_owned(),
                        provider: provider.clone(),
                        model: model.clone(),
                        tokens: usage,
                        latency_ms: u64::try_from(started.elapsed().as_millis())
                            .unwrap_or(u64::MAX),
                        retries,
                        stop_reason: stop_kind(stop_reason),
                    }),
                    Err(error) => telemetry.record_provider_error(&provider, error),
                    Ok(_) => {}
                }),
        )
    }
}

/// The session outcome for a loop status.
///
/// # Arguments
///
/// * `status` - The loop's final status.
///
/// # Returns
///
/// The outcome, with reasons reduced to their tags.
pub fn session_outcome(status: &Status) -> SessionOutcome {
    match status {
        Status::Done => SessionOutcome::Done,
        Status::Incomplete { reason } => SessionOutcome::Incomplete {
            reason: reason_tag(reason),
        },
        Status::Denied { reason } => SessionOutcome::Denied {
            reason: reason_tag(reason),
        },
        Status::ProviderError { error } => SessionOutcome::ProviderError {
            kind: error_kind(error).0,
        },
    }
}

/// The serde tag of an incomplete reason, such as `review_rounds_exhausted`.
fn reason_tag(reason: &IncompleteReason) -> String {
    serde_json::to_value(reason)
        .ok()
        .and_then(|value| value.get("reason")?.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_owned())
}

/// The serde tag of a provider error and its HTTP status, looking through
/// exhausted retries to the error that caused them.
fn error_kind(error: &ProviderError) -> (String, Option<u16>) {
    match error {
        ProviderError::RetriesExhausted { last, .. } => error_kind(last),
        ProviderError::Http { status, .. } => ("http".to_owned(), Some(*status)),
        other => (
            serde_json::to_value(other)
                .ok()
                .and_then(|value| value.get("error")?.as_str().map(str::to_owned))
                .unwrap_or_else(|| "unknown".to_owned()),
            None,
        ),
    }
}

/// Token counts from a provider's usage report.
fn tokens(usage: anvil_core::provider::Usage) -> Tokens {
    Tokens {
        input: usage.input,
        output: usage.output,
        cached: usage.cached,
    }
}

/// The stop kind for a stop reason.
fn stop_kind(reason: &StopReason) -> StopKind {
    match reason {
        StopReason::EndTurn => StopKind::EndTurn,
        StopReason::ToolUse => StopKind::ToolUse,
        StopReason::MaxTokens => StopKind::MaxTokens,
        StopReason::Other(_) => StopKind::Other,
    }
}

/// The record outcome for a tool outcome.
fn outcome_kind(outcome: ToolOutcome) -> ToolOutcomeKind {
    match outcome {
        ToolOutcome::Completed => ToolOutcomeKind::Completed,
        ToolOutcome::Failed => ToolOutcomeKind::Failed,
        ToolOutcome::DeniedByRule => ToolOutcomeKind::DeniedByRule,
        ToolOutcome::DeclinedByUser => ToolOutcomeKind::DeclinedByUser,
        ToolOutcome::Blocked => ToolOutcomeKind::Blocked,
        ToolOutcome::UnknownTool => ToolOutcomeKind::UnknownTool,
        ToolOutcome::InvalidArguments => ToolOutcomeKind::InvalidArguments,
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::Pin;

    use anvil_core::agent::{
        AgentLoop, AutoApprove, CheckCommand, LoopConfig, LoopProviders, State,
    };
    use anvil_core::provider::{FakeProvider, ToolCall};
    use anvil_core::tools::{Permissions, StaticPrompt, ToolContext};
    use anvil_telemetry::{DraftFuture, Drafter, Signal, SignalKind, TicketStore, draft_all};
    use anvil_types::LimitsConfig;

    use super::*;

    /// Appears in tool arguments and tool results and must never be written
    /// anywhere under the telemetry or ticket folders.
    const MARKER: &str = "MARKER_7f3e_project_secret";

    /// Returns one canned draft and remembers every prompt it saw.
    struct CapturingDrafter {
        prompts: Mutex<Vec<String>>,
    }

    impl Drafter for CapturingDrafter {
        fn complete<'a>(&'a self, system: &'a str, user: &'a str) -> DraftFuture<'a> {
            self.prompts
                .lock()
                .expect("lock")
                .push(format!("{system}\n{user}"));
            let reply: Pin<Box<dyn Future<Output = Result<String, String>> + Send>> = Box::pin(
                async {
                    Ok("```json\n{\"symptom\": \"anvil keeps failing a tool\", \"spec\": \"make it not fail\", \"acceptance\": [\"it passes\"]}\n```".to_owned())
                },
            );
            reply
        }
    }

    fn tool_call(id: &str, name: &str, arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            id: id.to_owned(),
            name: name.to_owned(),
            arguments_json: arguments.to_string(),
        }
    }

    /// Every file under `dir`, recursively.
    fn files_under(dir: &Path) -> Vec<std::path::PathBuf> {
        let mut found = Vec::new();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    found.extend(files_under(&path));
                } else {
                    found.push(path);
                }
            }
        }
        found
    }

    #[test]
    fn unattended_prompt_approves_and_records_every_call() {
        let home = tempfile::tempdir().expect("home");
        let cwd = tempfile::tempdir().expect("cwd");
        let session = ulid::Ulid::generate().to_string();
        let telemetry = SessionTelemetry::new(
            cwd.path(),
            Some(home.path()),
            &session,
            "fast",
            None,
            "fast",
            BTreeMap::new(),
            TelemetryConfig::default(),
        );
        let request = PermissionRequest {
            tool: "write_file".to_owned(),
            target: Some("notes.txt".to_owned()),
            arguments: serde_json::json!({"path": "notes.txt"}),
        };
        assert_eq!(telemetry.prompt(true).ask(&request), PromptAnswer::Yes);
        let records = load_records(&telemetry.project_dir(), None).expect("records");
        assert!(records.iter().any(|r| matches!(
            &r.kind,
            RecordKind::Permission { tool, answer: PermissionAnswer::Yes, .. } if tool == "write_file"
        )));
    }

    #[tokio::test]
    async fn marker_in_tool_results_never_reaches_telemetry_or_tickets() {
        let home = tempfile::tempdir().expect("home");
        let cwd = tempfile::tempdir().expect("cwd");
        let session = ulid::Ulid::generate().to_string();
        let telemetry = SessionTelemetry::new(
            cwd.path(),
            Some(home.path()),
            &session,
            "fast",
            None,
            "fast",
            BTreeMap::from([("fast".to_owned(), "m1".to_owned())]),
            TelemetryConfig::default(),
        );
        telemetry.record(RecordKind::SessionStarted {
            mode: SessionMode::Run,
        });

        let plan = "# Plan\n\n1. [ ] Write notes.txt\n";
        let write = tool_call(
            "c1",
            "write_file",
            serde_json::json!({"path": "notes.txt", "content": format!("line {MARKER}\n")}),
        );
        let read = tool_call("c2", "read_file", serde_json::json!({"path": "notes.txt"}));
        let unknown = tool_call("c3", "browser", serde_json::json!({"url": MARKER}));
        let coder = Arc::new(FakeProvider::new(vec![
            FakeProvider::text_turn(plan),
            FakeProvider::tool_turn(vec![write]),
            FakeProvider::tool_turn(vec![read, unknown]),
            FakeProvider::text_turn("built"),
            FakeProvider::text_turn("addressed the review"),
            FakeProvider::text_turn("nothing to simplify"),
        ]));
        let reviewer = Arc::new(FakeProvider::new(vec![
            FakeProvider::text_turn(&format!(
                "```json\n{{\"verdict\": \"changes\", \"items\": [\"rename {MARKER} in notes.txt\", \"add a test\"]}}\n```"
            )),
            FakeProvider::text_turn("```json\n{\"verdict\": \"approve\"}\n```"),
        ]));
        let session_dir = cwd.path().join(".anvil").join("sessions").join(&session);
        let config = LoopConfig {
            task: "write a note".to_owned(),
            cwd: cwd.path().to_path_buf(),
            session_dir,
            limits: LimitsConfig::default(),
            check: CheckCommand::Command("exit 0".to_owned()),
            context_window: 100_000,
            home_dir: None,
            extra_instructions: Vec::new(),
            sections: Vec::new(),
            review_sections: Vec::new(),
        };
        let prompt = RecordingPrompt {
            inner: Box::new(StaticPrompt(PromptAnswer::Yes)),
            telemetry: telemetry.clone(),
        };
        let (sink, task) = telemetry.sink(None);
        let agent = AgentLoop::new(
            config,
            LoopProviders {
                coder: coder as Arc<dyn Provider>,
                reviewer: reviewer as Arc<dyn Provider>,
                summarizer: Arc::new(FakeProvider::new(vec![])) as Arc<dyn Provider>,
            },
            ToolRegistry::with_builtins(Permissions::empty(), Box::new(prompt)),
            ToolContext::new(cwd.path()),
            Box::new(AutoApprove),
            sink,
        );
        let result = agent.run().await;
        task.await.expect("tee ends");
        telemetry.finish_run(&result.status);
        assert!(
            std::fs::read_to_string(cwd.path().join("notes.txt"))
                .expect("the tool wrote the file")
                .contains(MARKER)
        );

        let project = load_records(&telemetry.project_dir(), None).expect("records");
        assert!(project.iter().any(|r| matches!(&r.kind, RecordKind::ToolResult { name, outcome: ToolOutcomeKind::UnknownTool, .. } if name == "browser")));
        assert!(project.iter().any(|r| matches!(&r.kind, RecordKind::Permission { tool, answer: PermissionAnswer::Yes, .. } if tool == "write_file")));
        assert!(project.iter().any(|r| matches!(&r.kind, RecordKind::Turn { provider, model, .. } if provider == "fast" && model == "m1")));
        assert!(project.iter().any(|r| matches!(
            &r.kind,
            RecordKind::Review {
                approved: false,
                items: 2,
                ..
            }
        )));
        assert!(
            project
                .iter()
                .any(|r| matches!(&r.kind, RecordKind::SessionEnded { .. }))
        );

        let signals_path = signals_file(&telemetry.project_dir());
        let mut signals = anvil_telemetry::load_signals(&signals_path).expect("signals");
        assert!(
            signals.iter().any(
                |s| matches!(&s.kind, SignalKind::ToolMissing { tool, .. } if tool == "browser")
            ),
            "{signals:?}"
        );
        signals.push(Signal::new(
            &session,
            SignalKind::ReviewLoopExhausted { items: 2 },
        ));
        let drafter = CapturingDrafter {
            prompts: Mutex::new(Vec::new()),
        };
        let mut store = TicketStore::load(cwd.path()).expect("store");
        let report = draft_all(&mut signals, &project, &mut store, &drafter, Utc::now())
            .await
            .expect("drafts");
        assert_eq!(report.created.len(), signals.len(), "{report:?}");

        let mut checked = 0;
        for root in [
            cwd.path().join(".anvil").join("telemetry"),
            home.path().join(".anvil").join("telemetry"),
            cwd.path().join("tickets"),
        ] {
            for file in files_under(&root) {
                let text = std::fs::read_to_string(&file).expect("text file");
                assert!(
                    !text.contains(MARKER),
                    "{} leaks the marker",
                    file.display()
                );
                assert!(
                    !text.contains("notes.txt"),
                    "{} leaks a file name",
                    file.display()
                );
                checked += 1;
            }
        }
        assert!(checked >= 4, "{checked} files checked");
        for prompt in drafter.prompts.lock().expect("lock").iter() {
            assert!(!prompt.contains(MARKER));
            assert!(!prompt.contains("notes.txt"));
        }
    }

    #[test]
    fn status_and_error_kinds_reduce_to_tags() {
        assert_eq!(session_outcome(&Status::Done), SessionOutcome::Done);
        assert_eq!(
            session_outcome(&Status::Incomplete {
                reason: IncompleteReason::ReviewRoundsExhausted
            }),
            SessionOutcome::Incomplete {
                reason: "review_rounds_exhausted".to_owned()
            }
        );
        assert_eq!(
            session_outcome(&Status::Denied {
                reason: IncompleteReason::TurnBudget {
                    state: State::Build
                }
            }),
            SessionOutcome::Denied {
                reason: "turn_budget".to_owned()
            }
        );
        let error = ProviderError::RetriesExhausted {
            attempts: 3,
            last: Box::new(ProviderError::Http {
                status: 503,
                body: "secret body".to_owned(),
            }),
        };
        assert_eq!(error_kind(&error), ("http".to_owned(), Some(503)));
        assert_eq!(
            session_outcome(&Status::ProviderError { error }),
            SessionOutcome::ProviderError {
                kind: "http".to_owned()
            }
        );
        assert_eq!(
            error_kind(&ProviderError::Timeout { after_s: 5 }),
            ("timeout".to_owned(), None)
        );
    }
}
