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
mod turn;
pub mod verdict;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use forge_types::LimitsConfig;
use serde::{Deserialize, Serialize};

pub use checks::CheckCommand;
pub use events::{EventSink, LoopEvent};
pub use gate::{AutoApprove, PlanDecision, PlanGate, ScriptedGate};
pub use prompts::Instruction;
pub use state::{IncompleteReason, Rounds, State, Status};
pub use verdict::{ReviewVerdict, VerifyVerdict};

use self::prompts::{PromptInputs, system_prompt};
use self::state::{StateOutcome, Step, transition};
use self::turn::{Turn, complete_turn};
use self::verdict::parse_verdict;
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
    /// Instruction files to put in the system prompt.
    pub instructions: Vec<Instruction>,
}

/// The models a run talks to.
#[derive(Clone)]
pub struct LoopProviders {
    /// Plans, builds, optimizes and verifies.
    pub coder: Arc<dyn Provider>,
    /// Reviews the diff. May be the same provider as `coder`.
    pub reviewer: Arc<dyn Provider>,
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
    /// The coder-side conversation, shared by every state except Review.
    messages: Vec<Message>,
    spend: BTreeMap<State, Spend>,
    visits: BTreeMap<State, u32>,
    rounds: Rounds,
    /// Whether any tool call was denied, declined or blocked.
    denial_seen: bool,
    /// Paths given to `write_file` and `edit_file`, for the review fallback.
    touched: BTreeSet<String>,
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
        Self {
            config,
            providers,
            registry,
            ctx,
            gate,
            events,
            messages: Vec::new(),
            spend: BTreeMap::new(),
            visits: BTreeMap::new(),
            rounds: Rounds::default(),
            denial_seen: false,
            touched: BTreeSet::new(),
            plan: None,
            baseline: None,
            pending_feedback: None,
            last_text: String::new(),
        }
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
        self.baseline = diff::snapshot_tree(&self.config.cwd, &self.config.session_dir).await;
        if self.baseline.is_none() {
            self.warn(
                "not a git repository; the reviewer will see touched files instead of a diff"
                    .to_owned(),
            );
        }
        self.messages.push(Message::user(self.config.task.clone()));

        let mut state = State::Plan;
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
        self.events.emit(LoopEvent::StateEntered {
            state,
            visit: *visit,
        });
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
                system: prompts::REVIEW.to_owned(),
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

    /// Verify: run the project's checks, or have the model write a test.
    async fn run_verify(&mut self) -> StateOutcome {
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
            self.messages.push(Message::user(intro));
        }
        let system = system_prompt(&PromptInputs {
            state,
            instructions: &self.config.instructions,
            tools: &tools,
            task: &self.config.task,
        });
        loop {
            if let Some(reason) = self.budget_exceeded(state) {
                return ModelEnd::Interrupted(StateOutcome::Stop(reason));
            }
            let request = CompletionRequest {
                system: system.clone(),
                messages: self.messages.clone(),
                tools: tools.clone(),
                max_output_tokens: None,
            };
            let turn = match complete_turn(&*self.providers.coder, request, &self.events).await {
                Ok(turn) => turn,
                Err(error) => return ModelEnd::Interrupted(self.provider_failure(error)),
            };
            self.record_turn(state, &turn);
            self.messages.push(turn.message());
            self.last_text = turn.text.clone();
            if turn.tool_calls.is_empty() {
                return ModelEnd::Finished(turn.text);
            }
            let results = self.run_tools(&turn.tool_calls).await;
            self.messages.push(Message::ToolResults { results });
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
            if matches!(call.name.as_str(), "write_file" | "edit_file")
                && let Some(path) = serde_json::from_str::<serde_json::Value>(&call.arguments_json)
                    .ok()
                    .and_then(|args| args.get("path").and_then(|p| p.as_str()).map(str::to_owned))
            {
                self.touched.insert(path);
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
        if self.touched.is_empty() {
            return "(no git diff available and no files were written)".to_owned();
        }
        let list = self
            .touched
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
    fn finish(self, status: Status) -> LoopResult {
        let status = match status {
            Status::Incomplete { reason } if self.denial_seen => Status::Denied { reason },
            other => other,
        };
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
        if !self.touched.is_empty() {
            text.push_str("\n## Files written\n\n");
            for path in &self.touched {
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
        events: mpsc::UnboundedReceiver<LoopEvent>,
    }

    impl Harness {
        fn new(coder: Vec<Script>, reviewer: Vec<Script>) -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let (tx, rx) = mpsc::unbounded_channel();
            drop(tx);
            Self {
                dir,
                coder: Arc::new(FakeProvider::new(coder)),
                reviewer: Arc::new(FakeProvider::new(reviewer)),
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
                instructions: Vec::new(),
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
