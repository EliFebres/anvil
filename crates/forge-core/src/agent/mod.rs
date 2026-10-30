//! The agent loop: an explicit state machine from task to verified change.
//!
//! [`AgentLoop::run`] drives [`state::transition`]. Each state runs model
//! turns against the coder provider (or one review completion against the
//! reviewer), the pure transition table picks the next state, and every
//! move is reported on the [`EventSink`]. Nothing here prints.

pub mod checks;
pub mod diff;
pub mod events;
pub mod gate;
pub mod prompts;
pub mod state;
pub mod turn;
pub mod verdict;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use forge_types::LimitsConfig;
use serde::{Deserialize, Serialize};

pub use checks::{ChangeCheckFailure, ChangeChecks, CheckCommand};
pub use events::{EventSink, LoopEvent};
pub use gate::{AutoApprove, PlanDecision, PlanGate, ScriptedGate};
pub use prompts::{Instruction, PromptSection};
pub use state::{IncompleteReason, Rounds, State, Status};
pub use verdict::{ReviewVerdict, VerifyVerdict};

use self::prompts::{PromptInputs, system_prompt};
use self::state::{StateOutcome, Step, transition};
use self::turn::{Turn, complete_turn};
use self::verdict::parse_verdict;
use crate::context::{
    Conversation, SessionLog, SessionMeta, SessionRecord, SessionSnapshot, discover_instructions,
};
use crate::provider::{
    CompletionRequest, Message, Provider, ProviderError, ToolCall, ToolResult, ToolSpec,
};
use crate::tools::{Access, ToolContext, ToolRegistry, shell};

/// File name of the plan inside the session folder.
pub const PLAN_FILE: &str = "plan.md";

/// File name of the summary inside the session folder.
pub const SUMMARY_FILE: &str = "summary.md";

/// How long a project check command may run.
const CHECK_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Lines of check output shown to the model after a failure.
const CHECK_TAIL_LINES: usize = 80;

/// Settings for one run.
#[derive(Debug, Clone)]
pub struct LoopConfig {
    /// What the user asked for.
    pub task: String,
    /// The project root.
    pub cwd: PathBuf,
    /// Where the plan, summary and tool outputs go.
    pub session_dir: PathBuf,
    /// Budgets.
    pub limits: LimitsConfig,
    /// How Verify runs the project's checks.
    pub check: CheckCommand,
    /// The coder's context window in tokens; compaction runs at 80% of it.
    pub context_window: u32,
    /// The user's home directory, for `~/.forge/FORGE.md`.
    pub home_dir: Option<PathBuf>,
    /// Prompt sections supplied from outside the loop, such as a skill list,
    /// appended after the instruction files found on disk.
    pub extra_instructions: Vec<Instruction>,
    /// Sections under their own headings for every coder-side state, such
    /// as the user's standing rules, after the instruction files.
    pub sections: Vec<PromptSection>,
    /// Sections appended to the reviewer's prompt.
    pub review_sections: Vec<PromptSection>,
}

/// The models a run talks to.
#[derive(Clone)]
pub struct LoopProviders {
    /// Plans, builds, optimizes and verifies.
    pub coder: Arc<dyn Provider>,
    /// Reviews the diff. May be the same provider as `coder`.
    pub reviewer: Arc<dyn Provider>,
    /// Writes compaction summaries. May be the same provider as `coder`.
    pub summarizer: Arc<dyn Provider>,
}

/// Turns and tokens spent in one state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Spend {
    /// Completions made.
    pub turns: u32,
    /// Tokens used, as the providers reported them.
    pub tokens: u64,
}

/// What [`AgentLoop::run`] returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopResult {
    /// How the run ended.
    pub status: Status,
    /// The summary text, also written to [`SUMMARY_FILE`].
    pub summary: String,
    /// Where the summary was written.
    pub summary_path: PathBuf,
    /// Retry edges taken.
    pub rounds: Rounds,
    /// Spend per state.
    pub spend: BTreeMap<State, Spend>,
}

/// How a run of model turns in one state ended.
enum ModelEnd {
    /// The model ended its turn without tool calls; here is its text.
    Finished(String),
    /// Something stopped the state before the model finished.
    Interrupted(StateOutcome),
}

/// One run of the loop.
pub struct AgentLoop {
    config: LoopConfig,
    providers: LoopProviders,
    registry: ToolRegistry,
    ctx: ToolContext,
    gate: Box<dyn PlanGate>,
    events: EventSink,
    /// Checks run on the changed files in Verify, before the project's own.
    change_checks: Option<Arc<dyn ChangeChecks>>,
    /// The coder-side conversation, shared by every state except Review.
    conversation: Conversation,
    /// The session log, when the folder is writable.
    log: Option<SessionLog>,
    /// Where the run starts: Plan, or the restored state when resuming.
    start_state: State,
    spend: BTreeMap<State, Spend>,
    visits: BTreeMap<State, u32>,
    rounds: Rounds,
    /// Whether any tool call was denied, declined or blocked.
    denial_seen: bool,
    /// Paths given to `write_file` and `edit_file`, for the review fallback.
    written: BTreeSet<String>,
    /// Paths read or written, for instruction file discovery.
    touched: BTreeSet<PathBuf>,
    plan: Option<String>,
    /// Git tree of the working tree at session start.
    baseline: Option<String>,
    /// What to tell the model when Build is entered again.
    pending_feedback: Option<String>,
    last_text: String,
}

impl AgentLoop {
    /// Prepares a run. Nothing happens until [`AgentLoop::run`].
    ///
    /// The session folder is granted write access for `write_file` and
    /// `edit_file` without prompting, so the model can tick plan checkboxes.
    ///
    /// # Arguments
    ///
    /// * `config` - Task, paths and budgets.
    /// * `providers` - The coder and reviewer.
    /// * `registry` - Tools and permissions.
    /// * `ctx` - Where tools run; its `session_dir` is set from `config`.
    /// * `gate` - Plan approval.
    /// * `events` - Where events go.
    ///
    /// # Returns
    ///
    /// The loop.
    pub fn new(
        config: LoopConfig,
        providers: LoopProviders,
        registry: ToolRegistry,
        mut ctx: ToolContext,
        gate: Box<dyn PlanGate>,
        events: EventSink,
    ) -> Self {
        ctx.session_dir = Some(config.session_dir.clone());
        let session_glob = format!("{}/**", ctx.display_path(&config.session_dir));
        for tool in ["write_file", "edit_file"] {
            let rule = format!("{tool}:{session_glob}");
            if let Err(error) = registry.permissions().allow_unsaved(&rule) {
                tracing::warn!(%error, rule, "could not allow session folder writes");
            }
        }
        let log = match SessionLog::open(&config.session_dir) {
            Ok(mut log) => {
                if log.is_empty() {
                    let meta = SessionMeta {
                        id: config
                            .session_dir
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_default(),
                        created_at: chrono::Utc::now(),
                        task: config.task.clone(),
                        cwd: config.cwd.clone(),
                    };
                    if let Err(error) = log.append(&SessionRecord::Meta(meta)) {
                        tracing::warn!(%error, "cannot write the session meta record");
                    }
                }
                Some(log)
            }
            Err(error) => {
                tracing::warn!(%error, "session will not be persisted");
                None
            }
        };
        Self {
            config,
            providers,
            registry,
            ctx,
            gate,
            events,
            change_checks: None,
            conversation: Conversation::new(),
            log,
            start_state: State::Plan,
            spend: BTreeMap::new(),
            visits: BTreeMap::new(),
            rounds: Rounds::default(),
            denial_seen: false,
            written: BTreeSet::new(),
            touched: BTreeSet::new(),
            plan: None,
            baseline: None,
            pending_feedback: None,
            last_text: String::new(),
        }
    }

    /// Prepares a run that continues a saved session.
    ///
    /// The conversation and git baseline are restored from the snapshot. The
    /// run re-enters the last recorded state; a session that had already
    /// finished restarts at Build with its outcome as feedback.
    ///
    /// # Arguments
    ///
    /// * `snapshot` - The saved session, from `load_session`.
    /// * `config` - Task, paths and budgets; `task` and `session_dir` should
    ///   come from the snapshot.
    /// * `providers`, `registry`, `ctx`, `gate`, `events` - As for [`AgentLoop::new`].
    ///
    /// # Returns
    ///
    /// The loop.
    pub fn resume(
        snapshot: SessionSnapshot,
        config: LoopConfig,
        providers: LoopProviders,
        registry: ToolRegistry,
        ctx: ToolContext,
        gate: Box<dyn PlanGate>,
        events: EventSink,
    ) -> Self {
        let mut agent = Self::new(config, providers, registry, ctx, gate, events);
        agent.conversation = Conversation::from_messages(snapshot.messages);
        agent.baseline = snapshot.baseline;
        agent.plan = std::fs::read_to_string(snapshot.dir.join(PLAN_FILE)).ok();
        let (start, note) = match (snapshot.last_state, snapshot.status) {
            (_, Some(status)) => (
                State::Build,
                format!(
                    "This session was resumed. It had ended with: {status}. Continue the task from where it stopped."
                ),
            ),
            (Some(State::Plan) | None, None) => (State::Plan, String::new()),
            (Some(State::Done), None) => (
                State::Build,
                "This session was resumed. Continue the task from where it stopped.".to_owned(),
            ),
            (Some(state), None) => (
                state,
                "This session was resumed after an interruption. Continue from where it stopped."
                    .to_owned(),
            ),
        };
        agent.start_state = start;
        if !note.is_empty() {
            agent.pending_feedback = Some(note);
        }
        agent
    }

    /// Adds checks that Verify runs on the changed files before the
    /// project's check command. A failure sends the loop back to Build.
    ///
    /// # Arguments
    ///
    /// * `checks` - The checks.
    ///
    /// # Returns
    ///
    /// The loop.
    pub fn with_change_checks(mut self, checks: Arc<dyn ChangeChecks>) -> Self {
        self.change_checks = Some(checks);
        self
    }

    /// Runs the state machine to completion.
    ///
    /// # Returns
    ///
    /// The result; the summary is also written to the session folder.
    pub async fn run(mut self) -> LoopResult {
        if let Err(error) = std::fs::create_dir_all(&self.config.session_dir) {
            self.warn(format!("cannot create the session folder: {error}"));
        }
        if self.baseline.is_none() {
            self.baseline = diff::snapshot_tree(&self.config.cwd, &self.config.session_dir).await;
            match &self.baseline {
                Some(tree) => self.log(SessionRecord::Baseline { tree: tree.clone() }),
                None => self.warn(
                    "not a git repository; the reviewer will see touched files instead of a diff"
                        .to_owned(),
                ),
            }
        }
        if self.conversation.is_empty() {
            self.push_message(Message::user(self.config.task.clone()));
        }

        let mut state = self.start_state;
        self.enter(state);
        loop {
            let outcome = self.run_state(state).await;
            match transition(state, outcome, &mut self.rounds) {
                Step::Next(next) => {
                    self.events.emit(LoopEvent::Transition {
                        from: state,
                        to: next,
                    });
                    state = next;
                    self.enter(state);
                }
                Step::Finish(status) => {
                    self.events.emit(LoopEvent::Transition {
                        from: state,
                        to: State::Done,
                    });
                    return self.finish(status);
                }
            }
        }
    }

    /// Records a state entry.
    fn enter(&mut self, state: State) {
        let visit = self.visits.entry(state).or_insert(0);
        *visit += 1;
        let visit = *visit;
        self.events.emit(LoopEvent::StateEntered { state, visit });
        self.log(SessionRecord::State { state, visit });
    }

    /// Appends to the conversation and the session log.
    fn push_message(&mut self, message: Message) {
        self.log(SessionRecord::Message {
            message: message.clone(),
        });
        self.conversation.push(message);
    }

    /// Appends a record to the session log, if there is one.
    fn log(&mut self, record: SessionRecord) {
        if let Some(log) = &mut self.log
            && let Err(error) = log.append(&record)
        {
            tracing::warn!(%error, "cannot append to the session log");
        }
    }

    /// Builds the system prompt for a state, with instruction files found so far.
    fn system_prompt_for(&self, state: State, tools: &[ToolSpec]) -> String {
        let mut instructions = discover_instructions(
            self.config.home_dir.as_deref(),
            &self.config.cwd,
            &self.touched,
        );
        instructions.extend(self.config.extra_instructions.iter().cloned());
        system_prompt(&PromptInputs {
            state,
            instructions: &instructions,
            sections: &self.config.sections,
            tools,
            task: &self.config.task,
        })
    }

    /// Compacts the conversation when the next request would be too large.
    async fn compact_if_needed(&mut self, system: &str) {
        if !self
            .conversation
            .needs_compaction(system, self.config.context_window)
        {
            return;
        }
        let max_chars = usize::try_from(self.config.context_window)
            .unwrap_or(usize::MAX)
            .saturating_mul(2);
        let result = self
            .conversation
            .compact(
                &*self.providers.summarizer,
                &self.config.task,
                &self.config.session_dir,
                max_chars,
            )
            .await;
        match result {
            Ok(Some(compaction)) => {
                self.log(SessionRecord::Compacted {
                    messages: self.conversation.messages().to_vec(),
                    summary_path: compaction.summary_path.clone(),
                });
                self.events.emit(LoopEvent::Compacted {
                    dropped_messages: compaction.dropped_messages,
                    summary_path: compaction.summary_path,
                });
            }
            Ok(None) => tracing::debug!("context is large but there are too few turns to compact"),
            Err(error) => self.warn(format!(
                "compaction failed; continuing uncompacted: {error}"
            )),
        }
    }

    /// Dispatches to the state's handler.
    async fn run_state(&mut self, state: State) -> StateOutcome {
        match state {
            State::Plan => self.run_plan().await,
            State::Build => self.run_build().await,
            State::Review => self.run_review().await,
            State::Optimize => {
                let intro = "The change was approved by review. Make the simplification pass now.";
                match self
                    .run_model_state(State::Optimize, Some(intro.to_owned()), self.all_tools())
                    .await
                {
                    ModelEnd::Finished(_) => StateOutcome::Finished,
                    ModelEnd::Interrupted(outcome) => outcome,
                }
            }
            State::Verify => self.run_verify().await,
            State::Done => StateOutcome::Stop(IncompleteReason::Unexpected {
                detail: "the Done state was run".to_owned(),
            }),
        }
    }

    /// Plan: explore read-only, write the plan, get it approved.
    async fn run_plan(&mut self) -> StateOutcome {
        let tools = self.read_only_tools();
        let mut intro = None;
        loop {
            let text = match self
                .run_model_state(State::Plan, intro.take(), tools.clone())
                .await
            {
                ModelEnd::Finished(text) => text,
                ModelEnd::Interrupted(outcome) => return outcome,
            };
            let plan = extract_plan(&text);
            let path = self.config.session_dir.join(PLAN_FILE);
            if let Err(error) = std::fs::write(&path, &plan) {
                self.warn(format!("cannot write {}: {error}", path.display()));
            }
            self.events.emit(LoopEvent::PlanWritten { path });
            self.plan = Some(plan.clone());
            match self.gate.review(&plan) {
                PlanDecision::Approve => return StateOutcome::Finished,
                PlanDecision::Revise(feedback) => {
                    intro = Some(format!(
                        "The user wants the plan revised:\n{feedback}\n\nEnd your turn with the full revised plan."
                    ));
                }
                PlanDecision::Abort => return StateOutcome::Stop(IncompleteReason::PlanRejected),
            }
        }
    }

    /// Build: implement the plan, or address feedback from Review or Verify.
    async fn run_build(&mut self) -> StateOutcome {
        let intro = self.pending_feedback.take().unwrap_or_else(|| {
            format!(
                "The plan was approved. Implement it now. The plan file is `{}`; tick each step's checkbox there as you finish it.",
                self.plan_path()
            )
        });
        match self
            .run_model_state(State::Build, Some(intro), self.all_tools())
            .await
        {
            ModelEnd::Finished(_) => StateOutcome::Finished,
            ModelEnd::Interrupted(outcome) => outcome,
        }
    }

    /// Review: one separate completion against the reviewer with the diff.
    async fn run_review(&mut self) -> StateOutcome {
        if let Some(reason) = self.budget_exceeded(State::Review) {
            return StateOutcome::Stop(reason);
        }
        let diff = self.session_diff().await;
        let brief = format!(
            "# Task\n\n{}\n\n# Plan\n\n{}\n\n# Diff\n\n```diff\n{diff}\n```",
            self.config.task,
            self.plan.as_deref().unwrap_or("(no plan)")
        );
        let mut messages = vec![Message::user(brief)];
        let mut verdict = None;
        for attempt in 0..2 {
            let request = CompletionRequest {
                system: prompts::review_prompt(&self.config.review_sections),
                messages: messages.clone(),
                tools: Vec::new(),
                max_output_tokens: None,
            };
            let turn = match complete_turn(&*self.providers.reviewer, request, &self.events).await {
                Ok(turn) => turn,
                Err(error) => return self.provider_failure(error),
            };
            self.record_turn(State::Review, &turn);
            verdict = parse_verdict::<ReviewVerdict>(&turn.text);
            if verdict.is_some() {
                break;
            }
            if attempt == 0 {
                self.warn("the reviewer gave no JSON verdict; asking once more".to_owned());
                messages.push(turn.message());
                messages.push(Message::user(
                    "Reply with only the fenced JSON verdict block.",
                ));
            }
        }
        let verdict = verdict.unwrap_or_else(|| {
            self.warn(
                "the reviewer never gave a verdict; treating the review as approved".to_owned(),
            );
            ReviewVerdict::Approve
        });
        self.events.emit(LoopEvent::ReviewVerdict {
            verdict: verdict.clone(),
        });
        match verdict {
            ReviewVerdict::Approve => StateOutcome::ReviewApproved,
            ReviewVerdict::Changes { items } => {
                let list = items
                    .iter()
                    .map(|item| format!("- {item}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                self.pending_feedback = Some(format!(
                    "The reviewer requested changes:\n{list}\n\nAddress each one, then end your turn with what you changed."
                ));
                StateOutcome::ReviewChanges(items)
            }
        }
    }

    /// Verify: the change checks first, then the project's checks, or
    /// have the model write a test.
    async fn run_verify(&mut self) -> StateOutcome {
        if let Some(outcome) = self.run_change_checks().await {
            return outcome;
        }
        let Some(command) = self.config.check.resolve(&self.config.cwd) else {
            return self.run_model_verify().await;
        };
        self.events.emit(LoopEvent::CheckStarted {
            command: command.clone(),
        });
        let (passed, tail) = match shell::run_command(&self.ctx, &command, CHECK_TIMEOUT).await {
            Ok(run) => (
                run.succeeded(),
                tail_lines(&run.into_output().content, CHECK_TAIL_LINES),
            ),
            Err(error) => (false, format!("could not run `{command}`: {error}")),
        };
        self.events.emit(LoopEvent::CheckFinished {
            command: command.clone(),
            passed,
            output_tail: tail.clone(),
        });
        if passed {
            return StateOutcome::VerifyPassed;
        }
        self.pending_feedback = Some(format!(
            "Verification failed. `{command}` exited with an error:\n```\n{tail}\n```\nFix the cause, then end your turn with what you changed."
        ));
        StateOutcome::VerifyFailed(tail)
    }

    /// Runs the checks supplied from outside the loop on the changed files.
    ///
    /// Returns the failing outcome, or `None` when there are no checks or
    /// they all passed.
    async fn run_change_checks(&mut self) -> Option<StateOutcome> {
        let checks = Arc::clone(self.change_checks.as_ref()?);
        let changed = self.changed_files().await;
        let label = "standing rule checks".to_owned();
        self.events.emit(LoopEvent::CheckStarted {
            command: label.clone(),
        });
        let cwd = self.config.cwd.clone();
        let failures = tokio::task::spawn_blocking(move || checks.run(&cwd, &changed))
            .await
            .unwrap_or_else(|error| {
                vec![ChangeCheckFailure {
                    name: "checks".to_owned(),
                    output: format!("the checks did not finish: {error}"),
                }]
            });
        let passed = failures.is_empty();
        let report = failures
            .iter()
            .map(|failure| {
                format!(
                    "Standing rule `{}` check failed on the changed files:\n```\n{}\n```",
                    failure.name,
                    failure.output.trim_end()
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        self.events.emit(LoopEvent::CheckFinished {
            command: label,
            passed,
            output_tail: report.clone(),
        });
        if passed {
            return None;
        }
        self.pending_feedback = Some(format!(
            "Verification failed. {report}\nFix the cause, then end your turn with what you changed."
        ));
        Some(StateOutcome::VerifyFailed(report))
    }

    /// The files the session changed: from git, or the paths tools wrote.
    async fn changed_files(&self) -> Vec<PathBuf> {
        if let Some(baseline) = &self.baseline
            && let Some(files) =
                diff::changed_files(&self.config.cwd, &self.config.session_dir, baseline).await
        {
            return files;
        }
        self.written
            .iter()
            .map(|path| self.ctx.resolve(path))
            .collect()
    }

    /// Verify without a runner: the model writes and runs one test.
    async fn run_model_verify(&mut self) -> StateOutcome {
        let intro = "No test runner was detected. Verify the change as described, then end with the JSON verdict.";
        let text = match self
            .run_model_state(State::Verify, Some(intro.to_owned()), self.all_tools())
            .await
        {
            ModelEnd::Finished(text) => text,
            ModelEnd::Interrupted(outcome) => return outcome,
        };
        match parse_verdict::<VerifyVerdict>(&text) {
            Some(VerifyVerdict::Pass) => StateOutcome::VerifyPassed,
            Some(VerifyVerdict::Fail { details }) => {
                self.pending_feedback = Some(format!(
                    "Verification failed:\n{details}\n\nFix the cause, then end your turn with what you changed."
                ));
                StateOutcome::VerifyFailed(details)
            }
            None => {
                self.warn("the verify turn ended without a JSON verdict".to_owned());
                let detail = "the verify turn ended without a JSON verdict".to_owned();
                self.pending_feedback = Some(format!(
                    "Verification was inconclusive: {detail}. Run the test again and end with the JSON verdict."
                ));
                StateOutcome::VerifyFailed(detail)
            }
        }
    }

    /// Runs coder turns in `state` until the model stops calling tools.
    async fn run_model_state(
        &mut self,
        state: State,
        intro: Option<String>,
        tools: Vec<ToolSpec>,
    ) -> ModelEnd {
        if let Some(intro) = intro {
            self.push_message(Message::user(intro));
        }
        loop {
            if let Some(reason) = self.budget_exceeded(state) {
                return ModelEnd::Interrupted(StateOutcome::Stop(reason));
            }
            let system = self.system_prompt_for(state, &tools);
            self.compact_if_needed(&system).await;
            let request_len = self.conversation.messages().len();
            let request = CompletionRequest {
                system,
                messages: self.conversation.messages().to_vec(),
                tools: tools.clone(),
                max_output_tokens: None,
            };
            let turn = match complete_turn(&*self.providers.coder, request, &self.events).await {
                Ok(turn) => turn,
                Err(error) => return ModelEnd::Interrupted(self.provider_failure(error)),
            };
            self.record_turn(state, &turn);
            if let Some(usage) = turn.usage {
                self.conversation.record_usage(request_len, usage);
            }
            self.push_message(turn.message());
            self.last_text = turn.text.clone();
            if turn.tool_calls.is_empty() {
                return ModelEnd::Finished(turn.text);
            }
            let results = self.run_tools(&turn.tool_calls).await;
            self.push_message(Message::ToolResults { results });
        }
    }

    /// Executes each call through the registry and reports it.
    async fn run_tools(&mut self, calls: &[ToolCall]) -> Vec<ToolResult> {
        let mut results = Vec::with_capacity(calls.len());
        for call in calls {
            self.events
                .emit(LoopEvent::ToolStarted { call: call.clone() });
            let execution = self.registry.execute(call, &self.ctx).await;
            if execution.outcome.is_denial() {
                self.denial_seen = true;
            }
            if matches!(call.name.as_str(), "read_file" | "write_file" | "edit_file")
                && let Some(path) = serde_json::from_str::<serde_json::Value>(&call.arguments_json)
                    .ok()
                    .and_then(|args| args.get("path").and_then(|p| p.as_str()).map(str::to_owned))
            {
                self.touched.insert(self.ctx.resolve(&path));
                if call.name != "read_file" {
                    self.written.insert(path);
                }
            }
            self.events.emit(LoopEvent::ToolFinished {
                call_id: call.id.clone(),
                name: call.name.clone(),
                outcome: execution.outcome,
                preview: preview(&execution.result.content),
            });
            results.push(execution.result);
        }
        results
    }

    /// Adds a turn to the state's spend and reports it.
    fn record_turn(&mut self, state: State, turn: &Turn) {
        let spend = self.spend.entry(state).or_default();
        spend.turns += 1;
        spend.tokens = spend
            .tokens
            .saturating_add(turn.usage.map(|usage| usage.total()).unwrap_or(0));
        self.events.emit(LoopEvent::TurnFinished {
            state,
            stop_reason: turn.stop_reason.clone(),
            usage: turn.usage,
        });
    }

    /// Whether the state has used its turn or token budget.
    fn budget_exceeded(&self, state: State) -> Option<IncompleteReason> {
        let spend = self.spend.get(&state).copied().unwrap_or_default();
        let budget = state.budget(&self.config.limits);
        if spend.turns >= budget.max_turns {
            return Some(IncompleteReason::TurnBudget { state });
        }
        if spend.tokens >= budget.max_tokens {
            return Some(IncompleteReason::TokenBudget { state });
        }
        None
    }

    /// Maps a provider error to the outcome that ends the run.
    fn provider_failure(&self, error: ProviderError) -> StateOutcome {
        match error {
            ProviderError::BudgetExceeded { .. } => {
                StateOutcome::Stop(IncompleteReason::SessionBudget)
            }
            error => StateOutcome::ProviderFailed(error),
        }
    }

    /// The diff for the reviewer, or the touched-file list without git.
    async fn session_diff(&self) -> String {
        if let Some(baseline) = &self.baseline
            && let Some(diff) =
                diff::diff_since(&self.config.cwd, &self.config.session_dir, baseline).await
        {
            if diff.trim().is_empty() {
                return "(no changes in the working tree)".to_owned();
            }
            return diff;
        }
        if self.written.is_empty() {
            return "(no git diff available and no files were written)".to_owned();
        }
        let list = self
            .written
            .iter()
            .map(|path| format!("- {path}"))
            .collect::<Vec<_>>()
            .join("\n");
        format!("(no git diff available)\nFiles written by tools:\n{list}")
    }

    /// Every registered tool.
    fn all_tools(&self) -> Vec<ToolSpec> {
        self.registry.specs()
    }

    /// Tools whose access is read-only.
    fn read_only_tools(&self) -> Vec<ToolSpec> {
        self.registry
            .specs()
            .into_iter()
            .filter(|spec| {
                self.registry
                    .get(&spec.name)
                    .is_some_and(|tool| tool.access() == Access::Read)
            })
            .collect()
    }

    /// The plan file path as the model should write it.
    fn plan_path(&self) -> String {
        self.ctx
            .display_path(&self.config.session_dir.join(PLAN_FILE))
    }

    /// Emits a warning event.
    fn warn(&self, message: String) {
        tracing::warn!("{message}");
        self.events.emit(LoopEvent::Warning(message));
    }

    /// Writes the summary and builds the result.
    fn finish(mut self, status: Status) -> LoopResult {
        let status = match status {
            Status::Incomplete { reason } if self.denial_seen => Status::Denied { reason },
            other => other,
        };
        self.log(SessionRecord::Finished {
            status: status.clone(),
        });
        let summary = self.summary(&status);
        let summary_path = self.config.session_dir.join(SUMMARY_FILE);
        if let Err(error) = std::fs::write(&summary_path, &summary) {
            tracing::warn!(%error, path = %summary_path.display(), "cannot write the summary");
        }
        self.events.emit(LoopEvent::Finished {
            status: status.clone(),
            summary_path: summary_path.clone(),
        });
        LoopResult {
            status,
            summary,
            summary_path,
            rounds: self.rounds,
            spend: self.spend,
        }
    }

    /// Renders the summary.
    fn summary(&self, status: &Status) -> String {
        let mut text = format!(
            "# forge session summary\n\nStatus: {status}\n\nTask: {}\n\nRounds: review {}, verify {}\n\n## Spend\n\n",
            self.config.task, self.rounds.review, self.rounds.verify
        );
        for (state, spend) in &self.spend {
            text.push_str(&format!(
                "- {state}: {} turn(s), {} tokens\n",
                spend.turns, spend.tokens
            ));
        }
        if !self.written.is_empty() {
            text.push_str("\n## Files written\n\n");
            for path in &self.written {
                text.push_str(&format!("- {path}\n"));
            }
        }
        if !self.last_text.trim().is_empty() {
            text.push_str("\n## Last model message\n\n");
            text.push_str(&excerpt(&self.last_text, 2000));
            text.push('\n');
        }
        text
    }
}

/// Keeps the plan from the `# Plan` heading on, or the whole text.
fn extract_plan(text: &str) -> String {
    let start = text.find("# Plan").unwrap_or(0);
    let plan = text[start..].trim();
    if plan.is_empty() {
        "# Plan\n\n(the model produced no plan)".to_owned()
    } else {
        format!("{plan}\n")
    }
}

/// The last `count` lines of `text`.
fn tail_lines(text: &str, count: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(count);
    lines[start..].join("\n")
}

/// The first line, cut to 120 characters.
fn preview(text: &str) -> String {
    excerpt(text.lines().next().unwrap_or(""), 120)
}

/// Cuts text to `max` characters with an ellipsis.
fn excerpt(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let cut: String = text.chars().take(max).collect();
    format!("{cut}...")
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc;

    use super::*;
    use crate::provider::{Budgeted, Event, FakeProvider, Script, StopReason, TokenBudget};
    use crate::tools::{Permissions, PromptAnswer, StaticPrompt, ToolOutcome};

    const PLAN: &str = "Looked around.\n\n# Plan\n\n1. [ ] Write hello.txt\n2. [ ] Check it\n";

    fn approve() -> Script {
        FakeProvider::text_turn("Fine.\n```json\n{\"verdict\": \"approve\"}\n```")
    }

    fn changes() -> Script {
        FakeProvider::text_turn(
            "No.\n```json\n{\"verdict\": \"changes\", \"items\": [\"rename x\"]}\n```",
        )
    }

    fn write_call(path: &str) -> ToolCall {
        ToolCall {
            id: format!("w-{}", path.replace('/', "_")),
            name: "write_file".to_owned(),
            arguments_json: serde_json::json!({"path": path, "content": "hi"}).to_string(),
        }
    }

    struct Harness {
        dir: tempfile::TempDir,
        coder: Arc<FakeProvider>,
        reviewer: Arc<FakeProvider>,
        summarizer: Arc<FakeProvider>,
        events: mpsc::UnboundedReceiver<LoopEvent>,
    }

    impl Harness {
        fn new(coder: Vec<Script>, reviewer: Vec<Script>) -> Self {
            Self::with_summarizer(coder, reviewer, vec![])
        }

        fn with_summarizer(
            coder: Vec<Script>,
            reviewer: Vec<Script>,
            summarizer: Vec<Script>,
        ) -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let (tx, rx) = mpsc::unbounded_channel();
            drop(tx);
            Self {
                dir,
                coder: Arc::new(FakeProvider::new(coder)),
                reviewer: Arc::new(FakeProvider::new(reviewer)),
                summarizer: Arc::new(FakeProvider::new(summarizer)),
                events: rx,
            }
        }

        fn session_dir(&self) -> PathBuf {
            self.dir.path().join(".forge").join("sessions").join("t1")
        }

        fn config(&self, check: CheckCommand) -> LoopConfig {
            LoopConfig {
                task: "add a --dry-run flag".to_owned(),
                cwd: self.dir.path().to_path_buf(),
                session_dir: self.session_dir(),
                limits: LimitsConfig::default(),
                check,
                context_window: 100_000,
                home_dir: None,
                extra_instructions: Vec::new(),
                sections: Vec::new(),
                review_sections: Vec::new(),
            }
        }

        fn agent(
            &mut self,
            config: LoopConfig,
            coder: Arc<dyn Provider>,
            rules: &str,
            gate: Box<dyn PlanGate>,
        ) -> AgentLoop {
            let (tx, rx) = mpsc::unbounded_channel();
            self.events = rx;
            let registry = ToolRegistry::with_builtins(
                Permissions::from_toml(rules, None).expect("rules"),
                Box::new(StaticPrompt(PromptAnswer::No)),
            );
            AgentLoop::new(
                config,
                LoopProviders {
                    coder,
                    reviewer: Arc::clone(&self.reviewer) as Arc<dyn Provider>,
                    summarizer: Arc::clone(&self.summarizer) as Arc<dyn Provider>,
                },
                registry,
                ToolContext::new(self.dir.path()),
                gate,
                EventSink::new(tx),
            )
        }

        fn default_agent(&mut self, check: CheckCommand, rules: &str) -> AgentLoop {
            let config = self.config(check);
            let coder = Arc::clone(&self.coder) as Arc<dyn Provider>;
            self.agent(config, coder, rules, Box::new(AutoApprove))
        }

        fn drain(&mut self) -> Vec<LoopEvent> {
            let mut out = Vec::new();
            while let Ok(event) = self.events.try_recv() {
                out.push(event);
            }
            out
        }

        fn states(&mut self) -> Vec<State> {
            self.drain()
                .into_iter()
                .filter_map(|event| match event {
                    LoopEvent::StateEntered { state, .. } => Some(state),
                    _ => None,
                })
                .collect()
        }
    }

    #[tokio::test]
    async fn happy_path_visits_every_state_in_order() {
        let mut h = Harness::new(
            vec![
                FakeProvider::text_turn(PLAN),
                FakeProvider::text_turn("built"),
                FakeProvider::text_turn("nothing to simplify"),
            ],
            vec![approve()],
        );
        let agent = h.default_agent(CheckCommand::Command("exit 0".to_owned()), "");
        let result = agent.run().await;
        assert_eq!(result.status, Status::Done);
        assert_eq!(
            h.states(),
            vec![
                State::Plan,
                State::Build,
                State::Review,
                State::Optimize,
                State::Verify
            ]
        );
        let plan = std::fs::read_to_string(h.session_dir().join(PLAN_FILE)).expect("plan");
        assert!(plan.starts_with("# Plan\n"), "{plan}");
        assert!(!plan.contains("Looked around"));
        let summary = std::fs::read_to_string(&result.summary_path).expect("summary");
        assert!(summary.contains("Status: done"));
        assert_eq!(result.spend[&State::Review].turns, 1);
        assert_eq!(h.coder.calls(), 3);
        assert_eq!(h.reviewer.calls(), 1);
    }

    #[tokio::test]
    async fn build_runs_tools_and_reviewer_sees_touched_files() {
        let mut h = Harness::new(
            vec![
                FakeProvider::text_turn(PLAN),
                FakeProvider::tool_turn(vec![write_call("hello.txt")]),
                FakeProvider::text_turn("built"),
                FakeProvider::text_turn("nothing to simplify"),
            ],
            vec![approve()],
        );
        let agent = h.default_agent(
            CheckCommand::Command("exit 0".to_owned()),
            "allow = [\"write_file\"]",
        );
        let result = agent.run().await;
        assert_eq!(result.status, Status::Done);
        assert_eq!(
            std::fs::read_to_string(h.dir.path().join("hello.txt")).expect("written"),
            "hi"
        );
        let events = h.drain();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, LoopEvent::ToolStarted { call } if call.name == "write_file"))
        );
        assert!(events.iter().any(|e| matches!(
            e,
            LoopEvent::ToolFinished { outcome: ToolOutcome::Completed, name, .. } if name == "write_file"
        )));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, LoopEvent::CheckFinished { passed: true, .. }))
        );
        let review_request = &h.reviewer.requests()[0];
        let Message::User { text } = &review_request.messages[0] else {
            panic!("review brief");
        };
        assert!(text.contains("# Diff"), "{text}");
        assert!(text.contains("hello.txt"), "{text}");
        assert!(text.contains("1. [ ] Write hello.txt"));
        assert!(review_request.tools.is_empty());
        // The build intro names the plan file; the tool results reached the model.
        let build_request = &h.coder.requests()[1];
        assert!(
            matches!(&build_request.messages[2], Message::User { text } if text.contains("plan.md"))
        );
        assert!(matches!(
            &h.coder.requests()[2].messages[4],
            Message::ToolResults { .. }
        ));
    }

    #[tokio::test]
    async fn review_changes_loop_back_until_the_cap() {
        let mut h = Harness::new(
            vec![
                FakeProvider::text_turn(PLAN),
                FakeProvider::text_turn("b1"),
                FakeProvider::text_turn("b2"),
                FakeProvider::text_turn("b3"),
                FakeProvider::text_turn("b4"),
            ],
            vec![changes(), changes(), changes(), changes()],
        );
        let agent = h.default_agent(CheckCommand::Command("exit 0".to_owned()), "");
        let result = agent.run().await;
        assert_eq!(
            result.status,
            Status::Incomplete {
                reason: IncompleteReason::ReviewRoundsExhausted
            }
        );
        assert_eq!(result.rounds.review, 3);
        let states = h.states();
        assert_eq!(states.len(), 9);
        assert_eq!(states[1..], [State::Build, State::Review].repeat(4));
        let second_build = &h.coder.requests()[2];
        assert!(second_build.messages.iter().any(|m| matches!(
            m,
            Message::User { text } if text.contains("reviewer requested changes") && text.contains("rename x")
        )));
    }

    #[tokio::test]
    async fn verify_failures_loop_back_until_the_cap() {
        let mut h = Harness::new(
            vec![
                FakeProvider::text_turn(PLAN),
                FakeProvider::text_turn("b1"),
                FakeProvider::text_turn("o1"),
                FakeProvider::text_turn("b2"),
                FakeProvider::text_turn("o2"),
                FakeProvider::text_turn("b3"),
                FakeProvider::text_turn("o3"),
            ],
            vec![approve(), approve(), approve()],
        );
        let agent = h.default_agent(CheckCommand::Command("exit 1".to_owned()), "");
        let result = agent.run().await;
        assert_eq!(
            result.status,
            Status::Incomplete {
                reason: IncompleteReason::VerifyRoundsExhausted
            }
        );
        assert_eq!(result.rounds.verify, 2);
        let states = h.states();
        assert_eq!(states[0], State::Plan);
        assert_eq!(
            states[1..],
            [State::Build, State::Review, State::Optimize, State::Verify].repeat(3)
        );
        let rebuild = &h.coder.requests()[3];
        assert!(rebuild.messages.iter().any(|m| matches!(
            m,
            Message::User { text } if text.contains("Verification failed") && text.contains("exit 1")
        )));
    }

    #[tokio::test]
    async fn turn_budget_ends_the_run_with_a_summary() {
        let mut h = Harness::new(
            vec![
                FakeProvider::text_turn(PLAN),
                FakeProvider::tool_turn(vec![ToolCall {
                    id: "ls".to_owned(),
                    name: "list_dir".to_owned(),
                    arguments_json: "{}".to_owned(),
                }]),
                FakeProvider::text_turn("never reached"),
            ],
            vec![],
        );
        let mut config = h.config(CheckCommand::None);
        config.limits.build.max_turns = 1;
        let coder = Arc::clone(&h.coder) as Arc<dyn Provider>;
        let agent = h.agent(config, coder, "", Box::new(AutoApprove));
        let result = agent.run().await;
        assert_eq!(
            result.status,
            Status::Incomplete {
                reason: IncompleteReason::TurnBudget {
                    state: State::Build
                }
            }
        );
        assert_eq!(h.coder.remaining_scripts(), 1);
        assert!(result.summary.contains("used its turn budget"));
        assert!(result.summary.contains("build: 1 turn(s)"));
        assert!(h.drain().iter().any(|e| matches!(
            e,
            LoopEvent::Finished {
                status: Status::Incomplete { .. },
                ..
            }
        )));
    }

    #[tokio::test]
    async fn token_budget_is_checked_per_state() {
        let mut h = Harness::new(
            vec![
                FakeProvider::text_turn(PLAN),
                FakeProvider::text_turn("plan again"),
            ],
            vec![],
        );
        let mut config = h.config(CheckCommand::None);
        config.limits.plan.max_tokens = 10;
        let coder = Arc::clone(&h.coder) as Arc<dyn Provider>;
        let gate = ScriptedGate::new(vec![PlanDecision::Revise("shorter".to_owned())]);
        let agent = h.agent(config, coder, "", Box::new(gate));
        let result = agent.run().await;
        assert_eq!(
            result.status,
            Status::Incomplete {
                reason: IncompleteReason::TokenBudget { state: State::Plan }
            }
        );
    }

    #[tokio::test]
    async fn plan_gate_revise_then_abort() {
        let mut h = Harness::new(
            vec![
                FakeProvider::text_turn(PLAN),
                FakeProvider::text_turn("# Plan\n\n1. [ ] shorter\n"),
            ],
            vec![],
        );
        let gate = Arc::new(ScriptedGate::new(vec![
            PlanDecision::Revise("make it shorter".to_owned()),
            PlanDecision::Abort,
        ]));
        struct Shared(Arc<ScriptedGate>);
        impl PlanGate for Shared {
            fn review(&self, plan: &str) -> PlanDecision {
                self.0.review(plan)
            }
        }
        let config = h.config(CheckCommand::None);
        let coder = Arc::clone(&h.coder) as Arc<dyn Provider>;
        let agent = h.agent(config, coder, "", Box::new(Shared(Arc::clone(&gate))));
        let result = agent.run().await;
        assert_eq!(
            result.status,
            Status::Incomplete {
                reason: IncompleteReason::PlanRejected
            }
        );
        assert_eq!(gate.plans().len(), 2);
        assert!(gate.plans()[1].contains("shorter"));
        let second = &h.coder.requests()[1];
        assert!(second.messages.iter().any(|m| matches!(
            m,
            Message::User { text } if text.contains("make it shorter")
        )));
        assert_eq!(h.states(), vec![State::Plan]);
    }

    #[tokio::test]
    async fn provider_errors_end_the_run() {
        let mut h = Harness::new(
            vec![vec![Err(ProviderError::Timeout { after_s: 9 })]],
            vec![],
        );
        let agent = h.default_agent(CheckCommand::None, "");
        let result = agent.run().await;
        assert_eq!(
            result.status,
            Status::ProviderError {
                error: ProviderError::Timeout { after_s: 9 }
            }
        );
        assert_eq!(result.status.exit_code(), 4);
    }

    #[tokio::test]
    async fn session_budget_is_incomplete_not_a_provider_error() {
        let mut h = Harness::new(
            vec![
                FakeProvider::text_turn(PLAN),
                FakeProvider::text_turn("built"),
            ],
            vec![],
        );
        let coder: Arc<dyn Provider> =
            Arc::new(Budgeted::new(Arc::clone(&h.coder), TokenBudget::shared(1)));
        let config = h.config(CheckCommand::None);
        let agent = h.agent(config, coder, "", Box::new(AutoApprove));
        let result = agent.run().await;
        assert_eq!(
            result.status,
            Status::Incomplete {
                reason: IncompleteReason::SessionBudget
            }
        );
        assert_eq!(h.states(), vec![State::Plan, State::Build]);
    }

    #[tokio::test]
    async fn a_denied_tool_call_marks_an_incomplete_run_as_denied() {
        let mut h = Harness::new(
            vec![
                FakeProvider::text_turn(PLAN),
                FakeProvider::tool_turn(vec![write_call("secret.txt")]),
                FakeProvider::text_turn("b1"),
                FakeProvider::text_turn("b2"),
                FakeProvider::text_turn("b3"),
                FakeProvider::text_turn("b4"),
            ],
            vec![changes(), changes(), changes(), changes()],
        );
        let agent = h.default_agent(
            CheckCommand::Command("exit 0".to_owned()),
            "deny = [\"write_file\"]",
        );
        let result = agent.run().await;
        assert_eq!(
            result.status,
            Status::Denied {
                reason: IncompleteReason::ReviewRoundsExhausted
            }
        );
        assert_eq!(result.status.exit_code(), 3);
        assert!(!h.dir.path().join("secret.txt").exists());
        assert!(h.drain().iter().any(|e| matches!(
            e,
            LoopEvent::ToolFinished {
                outcome: ToolOutcome::DeniedByRule,
                ..
            }
        )));
    }

    #[tokio::test]
    async fn verify_without_a_runner_uses_the_model_verdict() {
        let mut h = Harness::new(
            vec![
                FakeProvider::text_turn(PLAN),
                FakeProvider::text_turn("built"),
                FakeProvider::text_turn("simplified"),
                FakeProvider::text_turn(
                    "ran it\n```json\n{\"verdict\": \"fail\", \"details\": \"assert broke\"}\n```",
                ),
                FakeProvider::text_turn("fixed"),
                FakeProvider::text_turn("simplified"),
                FakeProvider::text_turn("```json\n{\"verdict\": \"pass\"}\n```"),
            ],
            vec![approve(), approve()],
        );
        let agent = h.default_agent(CheckCommand::None, "");
        let result = agent.run().await;
        assert_eq!(result.status, Status::Done);
        assert_eq!(result.rounds.verify, 1);
        let rebuild = &h.coder.requests()[4];
        assert!(rebuild.messages.iter().any(|m| matches!(
            m,
            Message::User { text } if text.contains("assert broke")
        )));
        let verify_request = &h.coder.requests()[3];
        assert!(verify_request.system.contains("# State: VERIFY"));
    }

    #[tokio::test]
    async fn unparsable_review_verdict_is_reasked_then_approved() {
        let mut h = Harness::new(
            vec![
                FakeProvider::text_turn(PLAN),
                FakeProvider::text_turn("built"),
                FakeProvider::text_turn("simplified"),
            ],
            vec![
                FakeProvider::text_turn("hmm"),
                FakeProvider::text_turn("still no json"),
            ],
        );
        let agent = h.default_agent(CheckCommand::Command("exit 0".to_owned()), "");
        let result = agent.run().await;
        assert_eq!(result.status, Status::Done);
        assert_eq!(h.reviewer.calls(), 2);
        let retry = &h.reviewer.requests()[1];
        assert_eq!(retry.messages.len(), 3);
        let warnings = h
            .drain()
            .into_iter()
            .filter(|e| matches!(e, LoopEvent::Warning(_)))
            .count();
        assert!(warnings >= 2);
    }

    #[tokio::test]
    async fn plan_file_edits_are_allowed_without_prompting() {
        let mut h = Harness::new(
            vec![
                FakeProvider::text_turn(PLAN),
                FakeProvider::tool_turn(vec![ToolCall {
                    id: "tick".to_owned(),
                    name: "edit_file".to_owned(),
                    arguments_json: serde_json::json!({
                        "path": ".forge/sessions/t1/plan.md",
                        "old": "1. [ ]",
                        "new": "1. [x]"
                    })
                    .to_string(),
                }]),
                FakeProvider::text_turn("built"),
                FakeProvider::text_turn("simplified"),
            ],
            vec![approve()],
        );
        let agent = h.default_agent(CheckCommand::Command("exit 0".to_owned()), "");
        let result = agent.run().await;
        assert_eq!(result.status, Status::Done);
        let plan = std::fs::read_to_string(h.session_dir().join(PLAN_FILE)).expect("plan");
        assert!(plan.contains("1. [x] Write hello.txt"), "{plan}");
        assert!(h.drain().iter().any(|e| matches!(
            e,
            LoopEvent::ToolFinished { outcome: ToolOutcome::Completed, name, .. } if name == "edit_file"
        )));
    }

    /// A tool turn whose usage reports a large prompt, to drive compaction.
    fn heavy_tool_turn(id: &str, prompt_tokens: u64) -> Script {
        vec![
            Ok(Event::ToolCall(ToolCall {
                id: id.to_owned(),
                name: "list_dir".to_owned(),
                arguments_json: "{}".to_owned(),
            })),
            Ok(Event::Usage(crate::provider::Usage {
                input: prompt_tokens,
                output: 5,
                cached: 0,
            })),
            Ok(Event::Done {
                stop_reason: StopReason::ToolUse,
            }),
        ]
    }

    #[tokio::test]
    async fn compaction_runs_at_the_threshold_and_keeps_four_turns() {
        let mut h = Harness::with_summarizer(
            vec![
                FakeProvider::text_turn(PLAN),
                heavy_tool_turn("t1", 100),
                heavy_tool_turn("t2", 100),
                heavy_tool_turn("t3", 100),
                heavy_tool_turn("t4", 900),
                FakeProvider::text_turn("built"),
                FakeProvider::text_turn("simplified"),
            ],
            vec![approve()],
            vec![FakeProvider::text_turn("## Task\nsummary of earlier work")],
        );
        let mut config = h.config(CheckCommand::Command("exit 0".to_owned()));
        config.context_window = 1000;
        let coder = Arc::clone(&h.coder) as Arc<dyn Provider>;
        let agent = h.agent(config, coder, "", Box::new(AutoApprove));
        let result = agent.run().await;
        assert_eq!(result.status, Status::Done);
        assert_eq!(h.summarizer.calls(), 1, "one compaction");
        let compacted_request = &h.coder.requests()[5];
        assert!(matches!(
            &compacted_request.messages[0],
            Message::User { text } if text.contains("compacted") && text.contains("summary of earlier work")
        ));
        let assistant_turns = compacted_request
            .messages
            .iter()
            .filter(|m| matches!(m, Message::Assistant { .. }))
            .count();
        assert_eq!(assistant_turns, 4);
        assert!(h.session_dir().join("compaction-1.md").exists());
        let events = h.drain();
        assert!(events.iter().any(
            |e| matches!(e, LoopEvent::Compacted { dropped_messages, .. } if *dropped_messages > 0)
        ));
        let snapshot = crate::context::load_session(h.dir.path(), "t1").expect("log");
        assert!(
            matches!(&snapshot.messages[0], Message::User { text } if text.contains("compacted"))
        );
        assert_eq!(snapshot.status, Some(Status::Done));
    }

    #[tokio::test]
    async fn session_log_records_the_run_and_resume_continues_it() {
        let mut h = Harness::new(
            vec![
                FakeProvider::text_turn(PLAN),
                FakeProvider::tool_turn(vec![ToolCall {
                    id: "ls".to_owned(),
                    name: "list_dir".to_owned(),
                    arguments_json: "{}".to_owned(),
                }]),
            ],
            vec![],
        );
        let mut config = h.config(CheckCommand::Command("exit 0".to_owned()));
        config.limits.build.max_turns = 1;
        let coder = Arc::clone(&h.coder) as Arc<dyn Provider>;
        let agent = h.agent(config, coder, "", Box::new(AutoApprove));
        let first = agent.run().await;
        assert!(matches!(first.status, Status::Incomplete { .. }));

        let snapshot = crate::context::load_session(h.dir.path(), "t1").expect("log");
        assert_eq!(snapshot.meta.task, "add a --dry-run flag");
        assert_eq!(snapshot.last_state, Some(State::Build));
        assert_eq!(
            snapshot.messages.len(),
            5,
            "task, plan, intro, tool call, results"
        );
        let sessions = crate::context::list_sessions(h.dir.path()).expect("list");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].meta.id, "t1");

        // Resume with fresh providers: Build continues from the saved conversation.
        let resumed_coder = Arc::new(FakeProvider::new(vec![
            FakeProvider::text_turn("finished the build"),
            FakeProvider::text_turn("simplified"),
        ]));
        h.reviewer = Arc::new(FakeProvider::new(vec![approve()]));
        let config = h.config(CheckCommand::Command("exit 0".to_owned()));
        let (tx, rx) = mpsc::unbounded_channel();
        h.events = rx;
        let registry = ToolRegistry::with_builtins(
            Permissions::empty(),
            Box::new(StaticPrompt(PromptAnswer::No)),
        );
        let agent = AgentLoop::resume(
            snapshot,
            config,
            LoopProviders {
                coder: Arc::clone(&resumed_coder) as Arc<dyn Provider>,
                reviewer: Arc::clone(&h.reviewer) as Arc<dyn Provider>,
                summarizer: Arc::clone(&h.summarizer) as Arc<dyn Provider>,
            },
            registry,
            ToolContext::new(h.dir.path()),
            Box::new(AutoApprove),
            EventSink::new(tx),
        );
        let second = agent.run().await;
        assert_eq!(second.status, Status::Done);
        assert_eq!(
            h.states(),
            vec![State::Build, State::Review, State::Optimize, State::Verify]
        );
        let request = &resumed_coder.requests()[0];
        assert!(request.messages.len() > 5, "saved conversation restored");
        assert!(request.messages.iter().any(|m| matches!(
            m,
            Message::User { text } if text.contains("This session was resumed")
        )));
        let reloaded = crate::context::load_session(h.dir.path(), "t1").expect("log");
        assert_eq!(reloaded.status, Some(Status::Done));
        assert!(reloaded.messages.len() > request.messages.len());
    }

    #[tokio::test]
    async fn extra_instructions_follow_the_instruction_files() {
        let mut h = Harness::new(
            vec![
                FakeProvider::text_turn(PLAN),
                FakeProvider::text_turn("built"),
                FakeProvider::text_turn("nothing to simplify"),
            ],
            vec![approve()],
        );
        std::fs::write(h.dir.path().join("CLAUDE.md"), "repo rules").expect("write");
        let mut config = h.config(CheckCommand::Command("exit 0".to_owned()));
        config.extra_instructions.push(Instruction {
            source: "skills".to_owned(),
            text: "- caveman: terse voice".to_owned(),
        });
        let coder = Arc::clone(&h.coder) as Arc<dyn Provider>;
        let agent = h.agent(config, coder, "", Box::new(AutoApprove));
        assert_eq!(agent.run().await.status, Status::Done);
        let system = &h.coder.requests()[0].system;
        let repo = system.find("# Instructions from CLAUDE.md").expect("repo");
        let skills = system.find("# Instructions from skills").expect("skills");
        let tools = system.find("# Tools").expect("tools");
        assert!(repo < skills && skills < tools, "{system}");
        assert!(system.contains("- caveman: terse voice"));
    }

    #[tokio::test]
    async fn instruction_files_enter_the_prompt_as_files_are_touched() {
        let mut h = Harness::new(
            vec![
                FakeProvider::text_turn(PLAN),
                FakeProvider::tool_turn(vec![ToolCall {
                    id: "r".to_owned(),
                    name: "read_file".to_owned(),
                    arguments_json: serde_json::json!({"path": "crates/a/src/lib.rs"}).to_string(),
                }]),
                FakeProvider::text_turn("built"),
                FakeProvider::text_turn("simplified"),
            ],
            vec![approve()],
        );
        std::fs::write(h.dir.path().join("CLAUDE.md"), "repo rules").expect("write");
        let nested = h.dir.path().join("crates").join("a");
        std::fs::create_dir_all(nested.join("src")).expect("mkdir");
        std::fs::write(nested.join("CLAUDE.md"), "crate rules").expect("write");
        std::fs::write(nested.join("src").join("lib.rs"), "").expect("write");
        let agent = h.default_agent(CheckCommand::Command("exit 0".to_owned()), "");
        assert_eq!(agent.run().await.status, Status::Done);
        let requests = h.coder.requests();
        assert!(requests[0].system.contains("# Instructions from CLAUDE.md"));
        assert!(!requests[0].system.contains("crate rules"));
        assert!(
            requests[2]
                .system
                .contains("# Instructions from crates/a/CLAUDE.md")
        );
        let base = requests[2].system.find("You are forge").expect("base");
        let repo = requests[2].system.find("repo rules").expect("repo");
        let nested = requests[2].system.find("crate rules").expect("nested");
        let tools = requests[2].system.find("# Tools").expect("tools");
        assert!(base < repo && repo < nested && nested < tools);
    }

    /// Change checks that fail a scripted number of times, then pass.
    struct FlakyChecks {
        failures_left: std::sync::Mutex<u32>,
        seen: std::sync::Mutex<Vec<Vec<PathBuf>>>,
    }

    impl ChangeChecks for FlakyChecks {
        fn run(&self, _cwd: &std::path::Path, changed: &[PathBuf]) -> Vec<ChangeCheckFailure> {
            crate::provider::lock_or_recover(&self.seen).push(changed.to_vec());
            let mut left = crate::provider::lock_or_recover(&self.failures_left);
            if *left == 0 {
                return Vec::new();
            }
            *left -= 1;
            vec![ChangeCheckFailure {
                name: "google-docstrings".to_owned(),
                output: "hello.txt:1: public fn `x` has no doc comment".to_owned(),
            }]
        }
    }

    #[tokio::test]
    async fn change_check_failures_send_verify_back_to_build() {
        let mut h = Harness::new(
            vec![
                FakeProvider::text_turn(PLAN),
                FakeProvider::tool_turn(vec![write_call("hello.txt")]),
                FakeProvider::text_turn("b1"),
                FakeProvider::text_turn("o1"),
                FakeProvider::text_turn("b2"),
                FakeProvider::text_turn("o2"),
            ],
            vec![approve(), approve()],
        );
        let checks = Arc::new(FlakyChecks {
            failures_left: std::sync::Mutex::new(1),
            seen: std::sync::Mutex::new(Vec::new()),
        });
        let agent = h
            .default_agent(
                CheckCommand::Command("exit 0".to_owned()),
                "allow = [\"write_file\"]",
            )
            .with_change_checks(Arc::clone(&checks) as Arc<dyn ChangeChecks>);
        let result = agent.run().await;
        assert_eq!(result.status, Status::Done);
        assert_eq!(result.rounds.verify, 1);
        let seen = crate::provider::lock_or_recover(&checks.seen).clone();
        assert_eq!(seen.len(), 2);
        assert!(
            seen[0].iter().any(|path| path.ends_with("hello.txt")),
            "{seen:?}"
        );
        let rebuild = &h.coder.requests()[4];
        assert!(rebuild.messages.iter().any(|m| matches!(
            m,
            Message::User { text } if text.contains("Standing rule `google-docstrings` check failed")
                && text.contains("hello.txt:1")
        )));
        let events = h.drain();
        assert!(events.iter().any(|e| matches!(
            e,
            LoopEvent::CheckFinished { command, passed: false, .. } if command == "standing rule checks"
        )));
        let project_checks = events
            .iter()
            .filter(
                |e| matches!(e, LoopEvent::CheckFinished { command, .. } if command == "exit 0"),
            )
            .count();
        assert_eq!(
            project_checks, 1,
            "the project check runs only once the rule checks pass"
        );
    }

    #[tokio::test]
    async fn sections_reach_the_coder_and_the_reviewer() {
        let mut h = Harness::new(
            vec![
                FakeProvider::text_turn(PLAN),
                FakeProvider::text_turn("built"),
                FakeProvider::text_turn("nothing to simplify"),
            ],
            vec![approve()],
        );
        std::fs::write(h.dir.path().join("CLAUDE.md"), "repo rules").expect("write");
        let mut config = h.config(CheckCommand::Command("exit 0".to_owned()));
        config.sections.push(PromptSection {
            heading: "Standing rules".to_owned(),
            text: "1. [tidy] keep it tidy".to_owned(),
        });
        config.review_sections.push(PromptSection {
            heading: "Standing rules".to_owned(),
            text: "cite [tidy] on violations".to_owned(),
        });
        let coder = Arc::clone(&h.coder) as Arc<dyn Provider>;
        let agent = h.agent(config, coder, "", Box::new(AutoApprove));
        assert_eq!(agent.run().await.status, Status::Done);
        for request in h.coder.requests() {
            let repo = request.system.find("repo rules").expect("repo");
            let rules = request
                .system
                .find("# Standing rules\n\n1. [tidy]")
                .expect("rules");
            let tools = request.system.find("# Tools").expect("tools");
            assert!(repo < rules && rules < tools, "{}", request.system);
        }
        let review = &h.reviewer.requests()[0].system;
        assert!(review.starts_with(prompts::REVIEW.trim_end()), "{review}");
        assert!(
            review.ends_with("# Standing rules\n\ncite [tidy] on violations"),
            "{review}"
        );
    }

    #[test]
    fn helpers() {
        assert_eq!(extract_plan("chatter\n# Plan\n\n1. x"), "# Plan\n\n1. x\n");
        assert_eq!(
            extract_plan("   "),
            "# Plan\n\n(the model produced no plan)"
        );
        assert_eq!(tail_lines("a\nb\nc", 2), "b\nc");
        assert_eq!(preview("first line\nsecond"), "first line");
        assert_eq!(excerpt("abcdef", 3), "abc...");
        let _ = StopReason::EndTurn;
        let _ = Event::TextDelta(String::new());
    }
}
