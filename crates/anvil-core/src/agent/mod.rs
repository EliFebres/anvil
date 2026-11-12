//! The agent loop: an explicit state machine from task to verified change.
//!
//! [`AgentLoop::run`] drives [`state::transition`]. Each state runs model
//! turns against the coder provider (or one review completion against the
//! reviewer), the pure transition table picks the next state, and every
//! move is reported on the [`EventSink`]. Nothing here prints.

pub mod checks;
pub mod complete_step;
pub mod diff;
pub mod events;
pub mod gate;
pub mod plan;
pub mod prompts;
pub mod role;
pub mod state;
pub mod turn;
pub mod verdict;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anvil_types::LimitsConfig;
use serde::{Deserialize, Serialize};

pub use checks::{ChangeCheckFailure, ChangeChecks, CheckCommand, CheckSource};
pub use events::{EventSink, LoopEvent};
pub use gate::{AutoApprove, PlanDecision, PlanGate, ScriptedGate};
pub use prompts::{Instruction, PromptSection};
pub use role::{
    ESCALATE_AFTER_CHECK_FAILURES, ESCALATE_AFTER_REVIEW_CHANGES, EscalationReason, Role,
};
pub use state::{IncompleteReason, Rounds, State, Status};
pub use verdict::{ReviewVerdict, VerifyVerdict};

use self::plan::PlanCheck;
use self::prompts::{PromptInputs, system_prompt};
use self::state::{StateOutcome, Step, transition};
use self::turn::{Turn, complete_turn};
use self::verdict::parse_verdict;
use crate::context::{
    Conversation, SessionLog, SessionMeta, SessionRecord, SessionSnapshot, discover_instructions,
};
use crate::provider::{
    CompletionRequest, Message, Provider, ProviderError, ToolCall, ToolResult, ToolSpec, Usage,
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

/// The tool whose permission rules cover a check command a plan declares.
const SHELL_TOOL: &str = "shell";

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
    /// The user's home directory, for `~/.anvil/ANVIL.md`.
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
    /// Plans (unless `planner` is set), builds and verifies.
    pub coder: Arc<dyn Provider>,
    /// Plans, and takes build rounds the coder is stuck on. `None` means the
    /// coder plans, in one conversation shared with Build.
    pub planner: Option<Arc<dyn Provider>>,
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
    /// Spend per role, by whose provider served the completion.
    pub role_spend: BTreeMap<Role, Spend>,
}

/// How a run of model turns in one state ended.
enum ModelEnd {
    /// The model ended its turn without tool calls; here is its text.
    Finished(String),
    /// Something stopped the state before the model finished.
    Interrupted(StateOutcome),
}

/// The order the current build takes through review and checks.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Lane {
    /// No check command: review, then the model verifies by hand.
    Full,
    /// There is a check command: it runs before the review.
    CheckFirst,
    /// The checks passed before the review; the reviewer sees their output.
    Checked {
        /// Each command that ran, with the last lines it printed.
        runs: Vec<(String, String)>,
    },
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
    role_spend: BTreeMap<Role, Spend>,
    visits: BTreeMap<State, u32>,
    rounds: Rounds,
    /// Whether any tool call was denied, declined or blocked.
    denial_seen: bool,
    /// Paths given to `write_file` and `edit_file`, for the review fallback.
    written: BTreeSet<String>,
    /// Paths read or written, for instruction file discovery.
    touched: BTreeSet<PathBuf>,
    plan: Option<String>,
    /// The command the plan's `Check:` line declares, as last read.
    declared_check: Option<String>,
    /// `declared_check` when the permission rules allow it: what Verify runs.
    plan_check: Option<String>,
    /// Git tree of the working tree at session start.
    baseline: Option<String>,
    /// What to tell the model when Build is entered again.
    pending_feedback: Option<String>,
    /// The notes of the latest approving review, for the summary.
    review_notes: Vec<String>,
    /// The path the last finished build takes through review and checks.
    lane: Lane,
    last_text: String,
    /// Whether the coder's build conversation (with a planner) already exists.
    build_seeded: bool,
    /// Verify -> Build edges taken in a row, for escalation.
    check_failures: u32,
    /// Review -> Build edges taken in a row, for escalation.
    review_changes: u32,
    /// Whether the coming Build round runs on the planner.
    escalate_build: bool,
    /// One line per escalation, for the summary.
    escalations: Vec<String>,
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
        mut registry: ToolRegistry,
        mut ctx: ToolContext,
        gate: Box<dyn PlanGate>,
        events: EventSink,
    ) -> Self {
        ctx.session_dir = Some(config.session_dir.clone());
        registry.register(Box::new(complete_step::CompleteStep));
        let session_glob = format!("{}/**", ctx.display_path(&config.session_dir));
        let rules = [
            format!("write_file:{session_glob}"),
            format!("edit_file:{session_glob}"),
            complete_step::COMPLETE_STEP.to_owned(),
        ];
        for rule in rules {
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
            role_spend: BTreeMap::new(),
            visits: BTreeMap::new(),
            rounds: Rounds::default(),
            denial_seen: false,
            written: BTreeSet::new(),
            touched: BTreeSet::new(),
            plan: None,
            baseline: None,
            pending_feedback: None,
            declared_check: None,
            plan_check: None,
            review_notes: Vec::new(),
            lane: Lane::Full,
            last_text: String::new(),
            build_seeded: false,
            check_failures: 0,
            review_changes: 0,
            escalate_build: false,
            escalations: Vec::new(),
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
        agent.build_seeded = matches!(
            snapshot.last_state,
            Some(State::Build | State::Review | State::Verify | State::Done)
        );
        agent.plan = std::fs::read_to_string(snapshot.dir.join(PLAN_FILE)).ok();
        if let Some(plan) = agent.plan.clone() {
            agent.adopt_plan_check(&plan);
        }
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
                    self.track_retries(state, next);
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

    /// Counts consecutive failures and, when the coder is stuck and there is
    /// a planner, marks the coming Build round to run on it.
    fn track_retries(&mut self, from: State, to: State) {
        match (from, to) {
            (State::Verify, State::Build) => self.check_failures += 1,
            (State::Review, State::Build) => self.review_changes += 1,
            (State::Verify, _) => self.check_failures = 0,
            (State::Review, _) => self.review_changes = 0,
            _ => {}
        }
        if to != State::Build || self.providers.planner.is_none() {
            return;
        }
        let reason = if self.check_failures >= ESCALATE_AFTER_CHECK_FAILURES {
            self.check_failures = 0;
            EscalationReason::CheckFailures
        } else if self.review_changes >= ESCALATE_AFTER_REVIEW_CHANGES {
            self.review_changes = 0;
            EscalationReason::ReviewChanges
        } else {
            return;
        };
        let round = self.visits.get(&State::Build).copied().unwrap_or(0) + 1;
        self.escalate_build = true;
        self.escalations
            .push(format!("Build round {round} ran on the planner: {reason}"));
        self.events.emit(LoopEvent::Escalated { reason, round });
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
        let mut sections = self.config.sections.clone();
        if state == State::Plan {
            sections.push(self.check_section());
        }
        system_prompt(&PromptInputs {
            state,
            instructions: &instructions,
            sections: &sections,
            tools,
            task: &self.config.task,
        })
    }

    /// Tells the planner whether a check command was detected, so its
    /// `Check:` line can say `auto` or must supply one.
    fn check_section(&self) -> PromptSection {
        let text = match self.config.check.resolve(&self.config.cwd) {
            Some(command) => format!(
                "anvil detected this project's check command: `{command}`. It runs after the build without you. End the plan with `Check: auto`, or with a command of your own only when `{command}` cannot exercise the change."
            ),
            None => "anvil found no check command for this project, so `Check: auto` would check nothing. End the plan with a `Check:` line that gives a command.".to_owned(),
        };
        PromptSection {
            heading: "Check command".to_owned(),
            text,
        }
    }

    /// Reads the plan's `Check:` line and, when it names a different
    /// command than before, asks the permission rules about the new one.
    fn adopt_plan_check(&mut self, plan: &str) {
        let declared = match plan::check_line(plan) {
            PlanCheck::Command(command) => Some(command),
            PlanCheck::Auto | PlanCheck::Missing => None,
        };
        if declared == self.declared_check {
            return;
        }
        self.plan_check = declared.clone().filter(|command| self.permitted(command));
        self.declared_check = declared;
    }

    /// Whether the permission rules let anvil run a plan's check command;
    /// warns when they do not.
    fn permitted(&self, command: &str) -> bool {
        let args = serde_json::json!({ "command": command });
        match self.registry.permit(SHELL_TOOL, &args, &self.config.cwd) {
            Ok(()) => true,
            Err(reason) => {
                self.warn(format!("the plan's check command will not run: {reason}"));
                false
            }
        }
    }

    /// The check commands Verify runs, in order: the detected or configured
    /// one, then the plan's own when it is a different command.
    fn check_commands(&self) -> Vec<(CheckSource, String)> {
        let detected = self.config.check.resolve(&self.config.cwd);
        let declared = self
            .plan_check
            .clone()
            .filter(|command| Some(command) != detected.as_ref());
        detected
            .map(|command| (CheckSource::Detected, command))
            .into_iter()
            .chain(declared.map(|command| (CheckSource::Plan, command)))
            .collect()
    }

    /// The provider that serves completions for a role.
    fn provider_for(&self, role: Role) -> &dyn Provider {
        match role {
            Role::Planner => self
                .providers
                .planner
                .as_deref()
                .unwrap_or(&*self.providers.coder),
            Role::Coder => &*self.providers.coder,
            Role::Reviewer => &*self.providers.reviewer,
            Role::Summarizer => &*self.providers.summarizer,
        }
    }

    /// The role that plans: the planner when there is one, else the coder.
    fn plan_role(&self) -> Role {
        if self.providers.planner.is_some() {
            Role::Planner
        } else {
            Role::Coder
        }
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
        let summarizer = Arc::clone(&self.providers.summarizer);
        let result = self
            .conversation
            .compact(
                &*summarizer,
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
                self.add_role_spend(Role::Summarizer, compaction.usage);
                self.events.emit(LoopEvent::Compacted {
                    dropped_messages: compaction.dropped_messages,
                    summary_path: compaction.summary_path,
                    usage: compaction.usage,
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
                .run_model_state(State::Plan, self.plan_role(), intro.take(), tools.clone())
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
                PlanDecision::Approve => {
                    self.adopt_plan_check(&plan);
                    return StateOutcome::Finished;
                }
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
        if self.providers.planner.is_some() && !self.build_seeded {
            self.seed_build_conversation();
        }
        let role = if std::mem::take(&mut self.escalate_build) {
            Role::Planner
        } else {
            Role::Coder
        };
        let intro = self.pending_feedback.take().unwrap_or_else(|| {
            format!(
                "The plan above was approved. Implement it now without reading it back; its file is `{}`.",
                self.plan_path()
            )
        });
        if let ModelEnd::Interrupted(outcome) = self
            .run_model_state(State::Build, role, Some(intro), self.all_tools())
            .await
        {
            return outcome;
        }
        // The plan's check can itself be wrong, and the plan file is where
        // Build corrects the plan, so the line is read again after each build.
        if let Ok(plan) = std::fs::read_to_string(self.config.session_dir.join(PLAN_FILE)) {
            self.adopt_plan_check(&plan);
        }
        self.lane = if self.check_commands().is_empty() {
            Lane::Full
        } else {
            Lane::CheckFirst
        };
        if self.lane == Lane::CheckFirst {
            StateOutcome::ReadyToCheck
        } else {
            StateOutcome::Finished
        }
    }

    /// Starts the coder's own conversation from the task and the approved
    /// plan, because it cannot continue the planner's.
    fn seed_build_conversation(&mut self) {
        let plan = self.plan.as_deref().unwrap_or("# Plan\n\n(no plan)\n");
        let seed = Message::user(format!("# Task\n\n{}\n\n{plan}", self.config.task));
        self.conversation = Conversation::from_messages(vec![seed.clone()]);
        self.log(SessionRecord::Rebased {
            messages: vec![seed],
        });
        self.build_seeded = true;
    }

    /// Review: one separate completion against the reviewer with the diff.
    async fn run_review(&mut self) -> StateOutcome {
        if let Some(reason) = self.budget_exceeded(State::Review) {
            return StateOutcome::Stop(reason);
        }
        let diff = self.session_diff().await;
        let mut brief = format!(
            "# Task\n\n{}\n\n# Round\n\n{}\n\n# Plan\n\n{}\n\n# Diff\n\n```diff\n{diff}\n```",
            self.config.task,
            self.rounds.review + 1,
            self.plan.as_deref().unwrap_or("(no plan)")
        );
        if let Lane::Checked { runs } = &self.lane {
            brief.push_str("\n\n# Check");
            for (command, tail) in runs {
                brief.push_str(&format!("\n\n`{command}` passed:\n\n```\n{tail}\n```"));
            }
        }
        let mut messages = vec![Message::user(brief)];
        let mut verdict = None;
        for attempt in 0..2 {
            let request = CompletionRequest {
                system: prompts::review_prompt(&self.config.review_sections),
                messages: messages.clone(),
                tools: Vec::new(),
                max_output_tokens: None,
            };
            let turn = match complete_turn(self.provider_for(Role::Reviewer), request, &self.events)
                .await
            {
                Ok(turn) => turn,
                Err(error) => return self.provider_failure(error),
            };
            self.record_turn(State::Review, Role::Reviewer, &turn);
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
            ReviewVerdict::Approve { notes: Vec::new() }
        });
        self.events.emit(LoopEvent::ReviewVerdict {
            verdict: verdict.clone(),
        });
        match verdict {
            ReviewVerdict::Approve { notes } => {
                self.review_notes = notes;
                self.complete_open_steps();
                match self.lane {
                    Lane::Checked { .. } => StateOutcome::ApprovedAfterCheck,
                    Lane::Full | Lane::CheckFirst => StateOutcome::ReviewApproved,
                }
            }
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

    /// Ticks every plan step the model left open. An approved change means
    /// the plan is done, so this never costs a model turn.
    fn complete_open_steps(&self) {
        let path = self.config.session_dir.join(PLAN_FILE);
        let Ok(plan) = std::fs::read_to_string(&path) else {
            return;
        };
        let ticked = plan::tick_all(&plan);
        if ticked == plan {
            return;
        }
        match std::fs::write(&path, ticked) {
            Ok(()) => self.events.emit(LoopEvent::PlanUpdated { path }),
            Err(error) => self.warn(format!("cannot write {}: {error}", path.display())),
        }
    }

    /// Verify: the change checks first, then every check command, or have
    /// the model write a test when there is none.
    async fn run_verify(&mut self) -> StateOutcome {
        let before_review = self.lane == Lane::CheckFirst;
        if before_review {
            // A failure below goes back to Build, which picks the lane again.
            self.lane = Lane::Full;
        }
        if let Some(outcome) = self.run_change_checks().await {
            return outcome;
        }
        let commands = self.check_commands();
        if commands.is_empty() {
            return self.run_model_verify().await;
        }
        let mut runs = Vec::with_capacity(commands.len());
        for (source, command) in commands {
            let (passed, tail) = self.run_check(source, &command).await;
            if !passed {
                let hint = match source {
                    CheckSource::Plan => format!(
                        " This command is the plan's `Check:` line. If the command itself is wrong, correct that line in `{}` and say why.",
                        self.plan_path()
                    ),
                    CheckSource::Detected | CheckSource::Standing => String::new(),
                };
                self.pending_feedback = Some(format!(
                    "Verification failed. `{command}` exited with an error:\n```\n{tail}\n```\nFix the cause, then end your turn with what you changed.{hint}"
                ));
                return StateOutcome::VerifyFailed(tail);
            }
            runs.push((command, tail));
        }
        if before_review {
            self.lane = Lane::Checked { runs };
            return StateOutcome::CheckedBeforeReview;
        }
        StateOutcome::VerifyPassed
    }

    /// Runs one check command and reports it; returns whether it passed and
    /// the last lines it printed.
    async fn run_check(&self, source: CheckSource, command: &str) -> (bool, String) {
        self.events.emit(LoopEvent::CheckStarted {
            command: command.to_owned(),
        });
        let (passed, tail) = match shell::run_command(&self.ctx, command, CHECK_TIMEOUT).await {
            Ok(run) => (
                run.succeeded(),
                tail_lines(&run.into_output().content, CHECK_TAIL_LINES),
            ),
            Err(error) => (false, format!("could not run `{command}`: {error}")),
        };
        self.events.emit(LoopEvent::CheckFinished {
            command: command.to_owned(),
            source,
            passed,
            output_tail: tail.clone(),
        });
        (passed, tail)
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
            source: CheckSource::Standing,
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
            .run_model_state(
                State::Verify,
                Role::Coder,
                Some(intro.to_owned()),
                self.all_tools(),
            )
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

    /// Runs `role`'s turns in `state` until the model stops calling tools.
    async fn run_model_state(
        &mut self,
        state: State,
        role: Role,
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
            let turn = match complete_turn(self.provider_for(role), request, &self.events).await {
                Ok(turn) => turn,
                Err(error) => return ModelEnd::Interrupted(self.provider_failure(error)),
            };
            self.record_turn(state, role, &turn);
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
            if let Some(edit) = crate::tools::preview::proposed_edit(call, &self.ctx) {
                self.events.emit(edit);
            }
            let execution = self.registry.execute(call, &self.ctx).await;
            if execution.outcome.is_denial() {
                self.denial_seen = true;
            }
            if matches!(call.name.as_str(), "read_file" | "write_file" | "edit_file") {
                for path in file_paths(&call.arguments_json) {
                    self.touched.insert(self.ctx.resolve(&path));
                    if call.name != "read_file" {
                        self.written.insert(path);
                    }
                }
            }
            self.events.emit(LoopEvent::ToolFinished {
                call_id: call.id.clone(),
                name: call.name.clone(),
                outcome: execution.outcome,
                preview: preview(&execution.result.content),
                stats: execution.stats,
            });
            results.push(execution.result);
        }
        results
    }

    /// Adds a completion to a role's spend.
    fn add_role_spend(&mut self, role: Role, usage: Option<Usage>) {
        let spend = self.role_spend.entry(role).or_default();
        spend.turns += 1;
        spend.tokens = spend
            .tokens
            .saturating_add(usage.map(|usage| usage.weighted()).unwrap_or(0));
    }

    /// Adds a turn to the state's and the role's spend and reports it.
    fn record_turn(&mut self, state: State, role: Role, turn: &Turn) {
        let spend = self.spend.entry(state).or_default();
        spend.turns += 1;
        spend.tokens = spend
            .tokens
            .saturating_add(turn.usage.map(|usage| usage.weighted()).unwrap_or(0));
        self.add_role_spend(role, turn.usage);
        self.events.emit(LoopEvent::TurnFinished {
            state,
            role,
            stop_reason: turn.stop_reason.clone(),
            usage: turn.usage,
            latency_ms: turn.latency_ms,
            retries: turn.retries,
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
            return Some(IncompleteReason::TokenBudget {
                state,
                used: spend.tokens,
                limit: budget.max_tokens,
            });
        }
        None
    }

    /// Maps a provider error to the outcome that ends the run.
    fn provider_failure(&self, error: ProviderError) -> StateOutcome {
        match error {
            ProviderError::BudgetExceeded { used, limit } => {
                StateOutcome::Stop(IncompleteReason::SessionBudget { used, limit })
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
            role_spend: self.role_spend,
        }
    }

    /// Renders the summary.
    fn summary(&self, status: &Status) -> String {
        let mut text = format!(
            "# anvil session summary\n\nStatus: {status}\n\nTask: {}\n\nRounds: review {}, verify {}\n\n## Spend\n\n",
            self.config.task, self.rounds.review, self.rounds.verify
        );
        for (state, spend) in &self.spend {
            text.push_str(&format!(
                "- {state}: {} turn(s), {} tokens\n",
                spend.turns, spend.tokens
            ));
        }
        if self.providers.planner.is_some() {
            for (role, spend) in &self.role_spend {
                text.push_str(&format!(
                    "- role {role}: {} turn(s), {} tokens\n",
                    spend.turns, spend.tokens
                ));
            }
        }
        if !self.escalations.is_empty() {
            text.push_str("\n## Escalations\n\n");
            for line in &self.escalations {
                text.push_str(&format!("- {line}\n"));
            }
        }
        if !self.written.is_empty() {
            text.push_str("\n## Files written\n\n");
            for path in &self.written {
                text.push_str(&format!("- {path}\n"));
            }
        }
        if !self.review_notes.is_empty() {
            text.push_str("\n## Reviewer notes\n\n");
            for note in &self.review_notes {
                text.push_str(&format!("- {note}\n"));
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

/// The file paths a `read_file`, `write_file` or `edit_file` call names:
/// `path`, plus every `files[].path` of a multi-file read.
fn file_paths(arguments_json: &str) -> Vec<String> {
    let Ok(args) = serde_json::from_str::<serde_json::Value>(arguments_json) else {
        return Vec::new();
    };
    let single = args.get("path").and_then(|p| p.as_str()).map(str::to_owned);
    let listed = args
        .get("files")
        .and_then(|files| files.as_array())
        .into_iter()
        .flatten()
        .filter_map(|file| file.get("path").and_then(|p| p.as_str()).map(str::to_owned));
    single.into_iter().chain(listed).collect()
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
        /// The planner the next agent gets; `None` leaves the coder planning.
        planner: Option<Arc<FakeProvider>>,
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
                planner: None,
                events: rx,
            }
        }

        fn session_dir(&self) -> PathBuf {
            self.dir.path().join(".anvil").join("sessions").join("t1")
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
                    planner: self
                        .planner
                        .clone()
                        .map(|planner| planner as Arc<dyn Provider>),
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

        /// Makes the project a git repository, so the loop can measure its diff.
        fn git_init(&self) {
            let status = std::process::Command::new("git")
                .args(["init", "-q"])
                .current_dir(self.dir.path())
                .status()
                .expect("git runs");
            assert!(status.success());
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

    fn write_lines(path: &str, lines: usize) -> ToolCall {
        let content: String = (0..lines).map(|i| format!("line {i}\n")).collect();
        ToolCall {
            id: format!("w-{path}"),
            name: "write_file".to_owned(),
            arguments_json: serde_json::json!({"path": path, "content": content}).to_string(),
        }
    }

    #[tokio::test]
    async fn a_small_diff_checks_first_and_one_review_ends_the_run() {
        let mut h = Harness::new(
            vec![
                FakeProvider::text_turn(PLAN),
                FakeProvider::tool_turn(vec![write_call("a.txt")]),
                FakeProvider::text_turn("built"),
            ],
            vec![approve()],
        );
        h.git_init();
        let agent = h.default_agent(
            CheckCommand::Command("echo all-green".to_owned()),
            "allow = [\"write_file\"]",
        );
        assert_eq!(agent.run().await.status, Status::Done);
        assert_eq!(
            h.states(),
            [State::Plan, State::Build, State::Verify, State::Review]
        );
        assert_eq!(h.coder.requests().len(), 3);
        let brief = format!("{:?}", h.reviewer.requests()[0].messages);
        assert!(brief.contains("# Check"), "{brief}");
        assert!(brief.contains("all-green"), "{brief}");
    }

    #[tokio::test]
    async fn a_failed_first_check_is_fixed_and_checked_again_before_the_review() {
        let mut h = Harness::new(
            vec![
                FakeProvider::text_turn(PLAN),
                FakeProvider::tool_turn(vec![write_call("a.txt")]),
                FakeProvider::text_turn("built"),
                FakeProvider::tool_turn(vec![write_call("fixed.txt")]),
                FakeProvider::text_turn("fixed"),
            ],
            vec![approve()],
        );
        h.git_init();
        let agent = h.default_agent(
            CheckCommand::Command("test -f fixed.txt".to_owned()),
            "allow = [\"write_file\"]",
        );
        let result = agent.run().await;
        assert_eq!(result.status, Status::Done);
        assert_eq!(result.rounds.verify, 1);
        assert_eq!(
            h.states(),
            [
                State::Plan,
                State::Build,
                State::Verify,
                State::Build,
                State::Verify,
                State::Review
            ]
        );
        let brief = format!("{:?}", h.reviewer.requests()[0].messages);
        assert!(brief.contains("# Check"), "{brief}");
    }

    #[tokio::test]
    async fn a_large_diff_checks_first_too() {
        let mut h = Harness::new(
            vec![
                FakeProvider::text_turn(PLAN),
                FakeProvider::tool_turn(vec![write_lines("big.txt", diff::SMALL_DIFF_LINES)]),
                FakeProvider::text_turn("built"),
            ],
            vec![approve()],
        );
        h.git_init();
        let agent = h.default_agent(
            CheckCommand::Command("exit 0".to_owned()),
            "allow = [\"write_file\"]",
        );
        assert_eq!(agent.run().await.status, Status::Done);
        assert_eq!(
            h.states(),
            [State::Plan, State::Build, State::Verify, State::Review]
        );
    }

    /// A plan that ends with the given `Check:` line.
    fn plan_with_check(check: &str) -> Script {
        FakeProvider::text_turn(&format!(
            "# Plan\n\n1. [ ] Write hello.txt\n\nCheck: {check}\n"
        ))
    }

    fn states_in(events: &[LoopEvent]) -> Vec<State> {
        events
            .iter()
            .filter_map(|event| match event {
                LoopEvent::StateEntered { state, .. } => Some(*state),
                _ => None,
            })
            .collect()
    }

    fn check_events(events: &[LoopEvent]) -> Vec<(CheckSource, String, bool)> {
        events
            .iter()
            .filter_map(|event| match event {
                LoopEvent::CheckFinished {
                    command,
                    source,
                    passed,
                    ..
                } => Some((*source, command.clone(), *passed)),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn a_plan_declared_check_runs_with_no_model_turn_in_verify() {
        let mut h = Harness::new(
            vec![
                plan_with_check("test -f hello.txt"),
                FakeProvider::tool_turn(vec![write_call("hello.txt")]),
                FakeProvider::text_turn("built"),
            ],
            vec![approve()],
        );
        // An empty project: nothing is detected, so the plan's command is the only check.
        let agent = h.default_agent(CheckCommand::Detect, "allow = [\"write_file\", \"shell\"]");
        let result = agent.run().await;
        assert_eq!(result.status, Status::Done);
        let events = h.drain();
        assert_eq!(
            states_in(&events),
            [State::Plan, State::Build, State::Verify, State::Review]
        );
        assert_eq!(
            h.coder.calls(),
            3,
            "plan and two build turns; none in verify"
        );
        assert!(!result.spend.contains_key(&State::Verify));
        assert_eq!(
            check_events(&events),
            [(CheckSource::Plan, "test -f hello.txt".to_owned(), true)]
        );
        let brief = format!("{:?}", h.reviewer.requests()[0].messages);
        assert!(brief.contains("# Check"), "{brief}");
        assert!(brief.contains("`test -f hello.txt` passed"), "{brief}");
        let plan_system = &h.coder.requests()[0].system;
        assert!(plan_system.contains("# Check command"), "{plan_system}");
        assert!(
            plan_system.contains("found no check command"),
            "{plan_system}"
        );
    }

    #[tokio::test]
    async fn a_failing_plan_declared_check_sends_its_output_to_build() {
        let mut h = Harness::new(
            vec![
                plan_with_check("test -f fixed.txt"),
                FakeProvider::tool_turn(vec![write_call("hello.txt")]),
                FakeProvider::text_turn("built"),
                FakeProvider::tool_turn(vec![write_call("fixed.txt")]),
                FakeProvider::text_turn("fixed"),
            ],
            vec![approve()],
        );
        let agent = h.default_agent(CheckCommand::None, "allow = [\"write_file\", \"shell\"]");
        let result = agent.run().await;
        assert_eq!(result.status, Status::Done);
        let events = h.drain();
        assert_eq!(result.rounds.verify, 1);
        assert_eq!(
            states_in(&events),
            [
                State::Plan,
                State::Build,
                State::Verify,
                State::Build,
                State::Verify,
                State::Review
            ]
        );
        let rebuild = &h.coder.requests()[3];
        assert!(rebuild.messages.iter().any(|m| matches!(
            m,
            Message::User { text } if text.contains("Verification failed") && text.contains("test -f fixed.txt")
        )));
        let checks = check_events(&events);
        assert_eq!(
            checks.iter().map(|check| check.2).collect::<Vec<_>>(),
            [false, true]
        );
    }

    #[tokio::test]
    async fn a_check_line_the_build_corrects_in_the_plan_file_is_the_one_that_runs_next() {
        let fix_check = ToolCall {
            id: "fix-check".to_owned(),
            name: "edit_file".to_owned(),
            arguments_json: serde_json::json!({
                "path": ".anvil/sessions/t1/plan.md",
                "old": "Check: test -f wrong-name.txt",
                "new": "Check: test -f hello.txt"
            })
            .to_string(),
        };
        let mut h = Harness::new(
            vec![
                plan_with_check("test -f wrong-name.txt"),
                FakeProvider::tool_turn(vec![write_call("hello.txt")]),
                FakeProvider::text_turn("built"),
                FakeProvider::tool_turn(vec![fix_check]),
                FakeProvider::text_turn("the check named the wrong file"),
            ],
            vec![approve()],
        );
        let agent = h.default_agent(CheckCommand::None, "allow = [\"write_file\", \"shell\"]");
        let result = agent.run().await;
        assert_eq!(result.status, Status::Done);
        assert_eq!(result.rounds.verify, 1);
        let events = h.drain();
        assert_eq!(
            check_events(&events),
            [
                (
                    CheckSource::Plan,
                    "test -f wrong-name.txt".to_owned(),
                    false
                ),
                (CheckSource::Plan, "test -f hello.txt".to_owned(), true)
            ]
        );
        let rebuild = &h.coder.requests()[3];
        assert!(rebuild.messages.iter().any(|m| matches!(
            m,
            Message::User { text } if text.contains("If the command itself is wrong, correct that line")
        )));
    }

    #[tokio::test]
    async fn a_detected_check_and_a_plan_declared_one_both_run() {
        let mut h = Harness::new(
            vec![
                plan_with_check("`echo from-the-plan`"),
                FakeProvider::text_turn("built"),
            ],
            vec![approve()],
        );
        let agent = h.default_agent(
            CheckCommand::Command("echo detected-ok".to_owned()),
            "allow = [\"shell\"]",
        );
        assert_eq!(agent.run().await.status, Status::Done);
        assert_eq!(
            check_events(&h.drain()),
            [
                (CheckSource::Detected, "echo detected-ok".to_owned(), true),
                (CheckSource::Plan, "echo from-the-plan".to_owned(), true)
            ]
        );
        let brief = format!("{:?}", h.reviewer.requests()[0].messages);
        assert!(
            brief.contains("detected-ok") && brief.contains("from-the-plan"),
            "{brief}"
        );
        let plan_system = &h.coder.requests()[0].system;
        assert!(
            plan_system.contains("detected this project's check command: `echo detected-ok`"),
            "{plan_system}"
        );
    }

    #[tokio::test]
    async fn check_auto_and_a_repeat_of_the_detected_command_run_it_once() {
        for check in ["auto", "echo detected-ok"] {
            let mut h = Harness::new(
                vec![plan_with_check(check), FakeProvider::text_turn("built")],
                vec![approve()],
            );
            let agent = h.default_agent(
                CheckCommand::Command("echo detected-ok".to_owned()),
                "allow = [\"shell\"]",
            );
            assert_eq!(agent.run().await.status, Status::Done);
            assert_eq!(
                check_events(&h.drain()),
                [(CheckSource::Detected, "echo detected-ok".to_owned(), true)],
                "Check: {check}"
            );
        }
    }

    #[tokio::test]
    async fn a_plan_check_the_permission_rules_refuse_is_not_run() {
        let mut h = Harness::new(
            vec![
                plan_with_check("echo never-runs"),
                FakeProvider::text_turn("built"),
                FakeProvider::text_turn("```json\n{\"verdict\": \"pass\"}\n```"),
            ],
            vec![approve()],
        );
        // No rule allows `shell` and the prompt answers no.
        let agent = h.default_agent(CheckCommand::None, "");
        let result = agent.run().await;
        assert_eq!(result.status, Status::Done);
        let events = h.drain();
        assert_eq!(
            states_in(&events),
            [State::Plan, State::Build, State::Review, State::Verify],
            "without a usable check the model verifies after the review"
        );
        assert!(check_events(&events).is_empty());
        assert!(events.iter().any(|e| matches!(
            e,
            LoopEvent::Warning(text) if text.contains("the plan's check command will not run")
        )));
    }

    #[tokio::test]
    async fn happy_path_visits_every_state_in_order() {
        let mut h = Harness::new(
            vec![
                FakeProvider::text_turn(PLAN),
                FakeProvider::text_turn("built"),
            ],
            vec![approve()],
        );
        let agent = h.default_agent(CheckCommand::Command("exit 0".to_owned()), "");
        let result = agent.run().await;
        assert_eq!(result.status, Status::Done);
        assert_eq!(
            h.states(),
            vec![State::Plan, State::Build, State::Verify, State::Review]
        );
        let plan = std::fs::read_to_string(h.session_dir().join(PLAN_FILE)).expect("plan");
        assert!(plan.starts_with("# Plan\n"), "{plan}");
        assert!(!plan.contains("Looked around"));
        let summary = std::fs::read_to_string(&result.summary_path).expect("summary");
        assert!(summary.contains("Status: done"));
        assert_eq!(result.spend[&State::Review].turns, 1);
        assert_eq!(h.coder.calls(), 2);
        assert_eq!(h.reviewer.calls(), 1);
    }

    #[tokio::test]
    async fn build_runs_tools_and_reviewer_sees_touched_files() {
        let mut h = Harness::new(
            vec![
                FakeProvider::text_turn(PLAN),
                FakeProvider::tool_turn(vec![write_call("hello.txt")]),
                FakeProvider::text_turn("built"),
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
    async fn every_call_of_one_turn_runs_and_answers_in_one_message() {
        let mut h = Harness::new(
            vec![
                FakeProvider::text_turn(PLAN),
                FakeProvider::tool_turn(vec![write_call("a.txt"), write_call("b.txt")]),
                FakeProvider::text_turn("built"),
            ],
            vec![approve()],
        );
        let agent = h.default_agent(
            CheckCommand::Command("exit 0".to_owned()),
            "allow = [\"write_file\"]",
        );
        assert_eq!(agent.run().await.status, Status::Done);
        assert!(h.dir.path().join("a.txt").exists());
        assert!(h.dir.path().join("b.txt").exists());
        let next = &h.coder.requests()[2];
        let Some(Message::ToolResults { results }) = next.messages.last() else {
            panic!("the turn after the calls ends with their results");
        };
        let ids: Vec<&str> = results.iter().map(|r| r.call_id.as_str()).collect();
        assert_eq!(ids, ["w-a.txt", "w-b.txt"]);
    }

    #[tokio::test]
    async fn approval_notes_reach_the_summary_and_never_the_builder() {
        let mut h = Harness::new(
            vec![
                FakeProvider::text_turn(PLAN),
                FakeProvider::text_turn("b1"),
                FakeProvider::text_turn("b2"),
            ],
            vec![
                changes(),
                FakeProvider::text_turn(
                    "```json\n{\"verdict\": \"approve\", \"notes\": [\"lib.rs: `total` could fold into `sum`\"]}\n```",
                ),
            ],
        );
        let agent = h.default_agent(CheckCommand::Command("exit 0".to_owned()), "");
        let result = agent.run().await;
        assert_eq!(result.status, Status::Done);
        assert!(
            result
                .summary
                .contains("## Reviewer notes\n\n- lib.rs: `total` could fold into `sum`"),
            "{}",
            result.summary
        );
        let briefs: Vec<String> = h
            .reviewer
            .requests()
            .iter()
            .map(|request| format!("{:?}", request.messages))
            .collect();
        assert!(briefs[0].contains("# Round\\n\\n1\\n"), "{}", briefs[0]);
        assert!(briefs[1].contains("# Round\\n\\n2\\n"), "{}", briefs[1]);
        let sent_to_coder = format!("{:?}", h.coder.requests());
        assert!(
            !sent_to_coder.contains("could fold into"),
            "{sent_to_coder}"
        );
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
        assert_eq!(states.len(), 13);
        assert_eq!(
            states[1..],
            [State::Build, State::Verify, State::Review].repeat(4)
        );
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
                FakeProvider::text_turn("b2"),
                FakeProvider::text_turn("b3"),
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
        assert_eq!(states[1..], [State::Build, State::Verify].repeat(3));
        assert_eq!(
            h.reviewer.calls(),
            0,
            "a failing check never reaches review"
        );
        let rebuild = &h.coder.requests()[2];
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
        assert!(
            matches!(
                result.status,
                Status::Incomplete {
                    reason: IncompleteReason::TokenBudget {
                        state: State::Plan,
                        used,
                        limit: 10,
                    }
                } if used >= 10
            ),
            "{:?}",
            result.status
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
        assert!(
            matches!(
                result.status,
                Status::Incomplete {
                    reason: IncompleteReason::SessionBudget { used, limit: 1 }
                } if used >= 1
            ),
            "{:?}",
            result.status
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
                FakeProvider::text_turn(
                    "ran it\n```json\n{\"verdict\": \"fail\", \"details\": \"assert broke\"}\n```",
                ),
                FakeProvider::text_turn("fixed"),
                FakeProvider::text_turn("```json\n{\"verdict\": \"pass\"}\n```"),
            ],
            vec![approve(), approve()],
        );
        let agent = h.default_agent(CheckCommand::None, "");
        let result = agent.run().await;
        assert_eq!(result.status, Status::Done);
        assert_eq!(result.rounds.verify, 1);
        let rebuild = &h.coder.requests()[3];
        assert!(rebuild.messages.iter().any(|m| matches!(
            m,
            Message::User { text } if text.contains("assert broke")
        )));
        let verify_request = &h.coder.requests()[2];
        assert!(verify_request.system.contains("# State: VERIFY"));
    }

    #[tokio::test]
    async fn unparsable_review_verdict_is_reasked_then_approved() {
        let mut h = Harness::new(
            vec![
                FakeProvider::text_turn(PLAN),
                FakeProvider::text_turn("built"),
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
                        "path": ".anvil/sessions/t1/plan.md",
                        "old": "1. [ ]",
                        "new": "1. [x]"
                    })
                    .to_string(),
                }]),
                FakeProvider::text_turn("built"),
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

    #[tokio::test]
    async fn complete_step_ticks_without_prompting_and_approval_ticks_the_rest() {
        let mut h = Harness::new(
            vec![
                FakeProvider::text_turn(PLAN),
                FakeProvider::tool_turn(vec![ToolCall {
                    id: "done-1".to_owned(),
                    name: "complete_step".to_owned(),
                    arguments_json: "{\"step\": 1}".to_owned(),
                }]),
                FakeProvider::text_turn("built"),
                FakeProvider::text_turn("addressed"),
            ],
            vec![changes(), approve()],
        );
        let agent = h.default_agent(CheckCommand::Command("exit 0".to_owned()), "");
        let plan_path = h.session_dir().join(PLAN_FILE);
        let result = agent.run().await;
        assert_eq!(result.status, Status::Done);
        let events = h.drain();
        assert!(events.iter().any(|e| matches!(
            e,
            LoopEvent::ToolFinished { outcome: ToolOutcome::Completed, name, .. } if name == "complete_step"
        )));
        let updates = events
            .iter()
            .filter(|e| matches!(e, LoopEvent::PlanUpdated { .. }))
            .count();
        assert_eq!(updates, 1, "the back-fill is announced once");
        let plan = std::fs::read_to_string(plan_path).expect("plan");
        assert_eq!(plan, "# Plan\n\n1. [x] Write hello.txt\n2. [x] Check it\n");
        // Plan, two build turns and one rework turn: none was spent on step 2's box.
        assert_eq!(h.coder.calls(), 4);
        let build = &h.coder.requests()[1];
        assert!(build.tools.iter().any(|tool| tool.name == "complete_step"));
        assert!(
            h.coder.requests()[0]
                .tools
                .iter()
                .all(|tool| tool.name != "complete_step"),
            "Plan is read-only"
        );
    }

    #[tokio::test]
    async fn a_rejected_change_leaves_open_steps_open() {
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
        let plan_path = h.session_dir().join(PLAN_FILE);
        agent.run().await;
        let plan = std::fs::read_to_string(plan_path).expect("plan");
        assert!(plan.contains("1. [ ] Write hello.txt"), "{plan}");
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

    /// Lets a test keep a handle on the prompt the registry owns.
    struct SharedPrompt(Arc<crate::tools::ScriptedPrompt>);

    impl crate::tools::PermissionPrompt for SharedPrompt {
        fn ask(&self, request: &crate::tools::PermissionRequest) -> PromptAnswer {
            self.0.ask(request)
        }
    }

    /// An `edit_file` turn on `path` whose usage reports `prompt_tokens`.
    fn heavy_edit_turn(id: &str, path: &str, prompt_tokens: u64) -> Script {
        vec![
            Ok(Event::ToolCall(ToolCall {
                id: id.to_owned(),
                name: "edit_file".to_owned(),
                arguments_json: serde_json::json!({"path": path, "old": "old", "new": "new"})
                    .to_string(),
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
    async fn session_approval_survives_compaction_and_a_retried_call() {
        let mut h = Harness::with_summarizer(
            vec![
                FakeProvider::text_turn(PLAN),
                heavy_edit_turn("e0", "f0.txt", 100),
                heavy_edit_turn("e1", "./f1.txt", 100),
                heavy_edit_turn("e2", "f2.txt", 100),
                heavy_edit_turn("e3", "f3.txt", 900),
                heavy_edit_turn("e4", "f4.txt", 100),
                heavy_edit_turn("e0", "f0.txt", 100),
                FakeProvider::text_turn("built"),
            ],
            vec![approve()],
            vec![FakeProvider::text_turn("## Task\nsummary of earlier work")],
        );
        for i in 0..5 {
            std::fs::write(h.dir.path().join(format!("f{i}.txt")), "old\n").expect("write");
        }
        let mut config = h.config(CheckCommand::Command("exit 0".to_owned()));
        config.context_window = 1000;
        let prompt = Arc::new(crate::tools::ScriptedPrompt::new(vec![
            PromptAnswer::Session,
        ]));
        let registry = ToolRegistry::with_builtins(
            Permissions::empty(),
            Box::new(SharedPrompt(Arc::clone(&prompt))),
        );
        let (tx, rx) = mpsc::unbounded_channel();
        h.events = rx;
        let agent = AgentLoop::new(
            config,
            LoopProviders {
                coder: Arc::clone(&h.coder) as Arc<dyn Provider>,
                planner: None,
                reviewer: Arc::clone(&h.reviewer) as Arc<dyn Provider>,
                summarizer: Arc::clone(&h.summarizer) as Arc<dyn Provider>,
            },
            registry,
            ToolContext::new(h.dir.path()),
            Box::new(AutoApprove),
            EventSink::new(tx),
        );
        let result = agent.run().await;
        assert_eq!(result.status, Status::Done);
        assert_eq!(h.summarizer.calls(), 1, "one compaction");
        assert_eq!(prompt.requests().len(), 1, "only the first edit prompts");
        for i in 0..5 {
            let text =
                std::fs::read_to_string(h.dir.path().join(format!("f{i}.txt"))).expect("read");
            assert_eq!(text, "new\n", "f{i}.txt");
        }
        let events = h.drain();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, LoopEvent::Compacted { .. }))
        );
        assert!(!events.iter().any(|e| matches!(
            e,
            LoopEvent::ToolFinished {
                outcome: ToolOutcome::DeclinedByUser,
                ..
            }
        )));
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
        let resumed_coder = Arc::new(FakeProvider::new(vec![FakeProvider::text_turn(
            "finished the build",
        )]));
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
                planner: None,
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
        assert_eq!(h.states(), vec![State::Build, State::Verify, State::Review]);
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
                    arguments_json: serde_json::json!({"files": [{"path": "crates/a/src/lib.rs"}]})
                        .to_string(),
                }]),
                FakeProvider::text_turn("built"),
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
        assert_eq!(
            file_paths(r#"{"files": [{"path": "a.rs"}, {"path": "b/c.rs", "offset": 3}]}"#),
            ["a.rs", "b/c.rs"]
        );
        assert_eq!(file_paths(r#"{"path": "x"}"#), ["x"]);
        assert!(file_paths("{").is_empty());
        let base = requests[2].system.find("You are anvil").expect("base");
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
                FakeProvider::text_turn("b2"),
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
        let rebuild = &h.coder.requests()[3];
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

    #[tokio::test]
    async fn without_a_planner_the_coder_requests_do_not_change() {
        let mut h = Harness::new(
            vec![
                FakeProvider::text_turn(PLAN),
                FakeProvider::tool_turn(vec![write_call("a.txt")]),
                FakeProvider::text_turn("built"),
                FakeProvider::tool_turn(vec![write_call("fixed.txt")]),
                FakeProvider::text_turn("fixed"),
            ],
            vec![approve()],
        );
        h.git_init();
        let agent = h.default_agent(
            CheckCommand::Command("test -f fixed.txt".to_owned()),
            "allow = [\"write_file\"]",
        );
        assert_eq!(agent.run().await.status, Status::Done);
        // The prompt files check out with CRLF on Windows; the snapshot must
        // be the same on every platform.
        let requests = serde_json::to_string_pretty(&h.coder.requests())
            .expect("requests serialize")
            .replace("\\r\\n", "\\n");
        insta::assert_snapshot!(requests);
    }

    /// Gives the next agent a planner that replays `scripts`.
    fn add_planner(h: &mut Harness, scripts: Vec<Script>) -> Arc<FakeProvider> {
        let planner = Arc::new(FakeProvider::new(scripts));
        h.planner = Some(Arc::clone(&planner));
        planner
    }

    fn list_dir_turn() -> Script {
        FakeProvider::tool_turn(vec![ToolCall {
            id: "ls".to_owned(),
            name: "list_dir".to_owned(),
            arguments_json: "{}".to_owned(),
        }])
    }

    #[tokio::test]
    async fn a_planner_plans_and_the_coder_builds_from_the_approved_plan() {
        let mut h = Harness::new(vec![FakeProvider::text_turn("built")], vec![approve()]);
        let planner = add_planner(&mut h, vec![list_dir_turn(), FakeProvider::text_turn(PLAN)]);
        let agent = h.default_agent(CheckCommand::Command("exit 0".to_owned()), "");
        assert_eq!(agent.run().await.status, Status::Done);

        assert_eq!(planner.calls(), 2, "exploration and the plan");
        assert_eq!(h.coder.calls(), 1, "only the build");
        let first = &h.coder.requests()[0];
        assert_eq!(first.messages.len(), 2, "seed and build intro only");
        let Message::User { text } = &first.messages[0] else {
            panic!("the seed is a user message: {:?}", first.messages[0]);
        };
        assert!(text.contains("add a --dry-run flag"), "{text}");
        assert!(text.contains("1. [ ] Write hello.txt"), "{text}");
        assert!(
            first
                .messages
                .iter()
                .all(|m| matches!(m, Message::User { .. })),
            "no planner turns leak into the build conversation"
        );
        assert!(matches!(
            &first.messages[1],
            Message::User { text } if text.contains("The plan above was approved")
        ));
    }

    #[tokio::test]
    async fn a_plan_revision_stays_on_the_planner() {
        let mut h = Harness::new(vec![FakeProvider::text_turn("built")], vec![approve()]);
        let planner = add_planner(
            &mut h,
            vec![
                FakeProvider::text_turn(PLAN),
                FakeProvider::text_turn("# Plan\n\n1. [ ] shorter\n"),
            ],
        );
        let config = h.config(CheckCommand::Command("exit 0".to_owned()));
        let coder = Arc::clone(&h.coder) as Arc<dyn Provider>;
        let gate = ScriptedGate::new(vec![PlanDecision::Revise("make it shorter".to_owned())]);
        let agent = h.agent(config, coder, "", Box::new(gate));
        assert_eq!(agent.run().await.status, Status::Done);

        assert_eq!(planner.calls(), 2);
        assert!(planner.requests()[1].messages.iter().any(|m| matches!(
            m,
            Message::User { text } if text.contains("make it shorter")
        )));
        assert_eq!(h.coder.calls(), 1);
        let seed = format!("{:?}", h.coder.requests()[0].messages[0]);
        assert!(seed.contains("shorter"), "{seed}");
        assert!(!seed.contains("Write hello.txt"), "{seed}");
    }

    /// Runs a planner session whose reviewer asks for changes three times
    /// and then approves, so the third build escalates.
    async fn review_escalation_run() -> (Harness, Arc<FakeProvider>, LoopResult) {
        let mut h = Harness::new(
            vec![
                FakeProvider::text_turn("b1"),
                FakeProvider::text_turn("b2"),
                FakeProvider::text_turn("b4"),
            ],
            vec![changes(), changes(), changes(), approve()],
        );
        let planner = add_planner(
            &mut h,
            vec![FakeProvider::text_turn(PLAN), FakeProvider::text_turn("b3")],
        );
        let agent = h.default_agent(CheckCommand::Command("exit 0".to_owned()), "");
        let result = agent.run().await;
        (h, planner, result)
    }

    #[tokio::test]
    async fn two_review_change_requests_send_one_build_round_to_the_planner() {
        let (mut h, planner, result) = review_escalation_run().await;
        assert_eq!(result.status, Status::Done);
        assert_eq!(planner.calls(), 2, "the plan and build round 3");
        assert_eq!(h.coder.calls(), 3, "rounds 1, 2 and 4");

        let escalated = &planner.requests()[1];
        assert!(matches!(
            &escalated.messages[0],
            Message::User { text } if text.contains("# Task")
        ));
        assert!(
            escalated.messages.iter().any(|m| matches!(
                m,
                Message::User { text } if text.contains("reviewer requested changes")
            )),
            "the escalated round continues the build conversation"
        );
        let after = &h.coder.requests()[2];
        assert!(
            after
                .messages
                .iter()
                .any(|m| matches!(m, Message::Assistant { .. })),
            "the coder returns to the same conversation"
        );

        let events = h.drain();
        let escalations: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, LoopEvent::Escalated { .. }))
            .collect();
        assert_eq!(
            escalations,
            [&LoopEvent::Escalated {
                reason: EscalationReason::ReviewChanges,
                round: 3
            }]
        );
        assert!(
            result.summary.contains("## Escalations"),
            "{}",
            result.summary
        );
        assert!(
            result.summary.contains("Build round 3 ran on the planner"),
            "{}",
            result.summary
        );
    }

    #[tokio::test]
    async fn two_failed_checks_send_the_next_build_round_to_the_planner() {
        let mut h = Harness::new(
            vec![FakeProvider::text_turn("b1"), FakeProvider::text_turn("b2")],
            vec![approve()],
        );
        let planner = add_planner(
            &mut h,
            vec![
                FakeProvider::text_turn(PLAN),
                FakeProvider::tool_turn(vec![write_call("ok.txt")]),
                FakeProvider::text_turn("b3"),
            ],
        );
        h.git_init();
        let agent = h.default_agent(
            CheckCommand::Command("test -f ok.txt".to_owned()),
            "allow = [\"write_file\"]",
        );
        let result = agent.run().await;
        assert_eq!(result.status, Status::Done);
        assert_eq!(result.rounds.verify, 2);
        assert_eq!(planner.calls(), 3, "the plan and two turns of round 3");
        assert_eq!(h.coder.calls(), 2);
        assert!(h.drain().contains(&LoopEvent::Escalated {
            reason: EscalationReason::CheckFailures,
            round: 3
        }));
        assert!(
            result.summary.contains("## Escalations"),
            "{}",
            result.summary
        );
    }

    #[tokio::test]
    async fn without_a_planner_nothing_escalates() {
        let mut h = Harness::new(
            vec![
                FakeProvider::text_turn(PLAN),
                FakeProvider::text_turn("b1"),
                FakeProvider::text_turn("b2"),
                FakeProvider::text_turn("b3"),
            ],
            vec![],
        );
        let agent = h.default_agent(CheckCommand::Command("exit 1".to_owned()), "");
        let result = agent.run().await;
        assert_eq!(result.rounds.verify, 2);
        assert!(
            !h.drain()
                .iter()
                .any(|e| matches!(e, LoopEvent::Escalated { .. }))
        );
        assert!(!result.summary.contains("Escalations"));
        assert!(!result.summary.contains("- role "), "{}", result.summary);
        assert_eq!(h.coder.calls(), 4, "the coder plans and builds three times");
    }

    #[tokio::test]
    async fn spend_is_attributed_to_the_role_whose_provider_served_it() {
        let (_h, _planner, result) = review_escalation_run().await;
        let turns = |role| result.role_spend.get(&role).map(|spend| spend.turns);
        assert_eq!(
            turns(Role::Planner),
            Some(2),
            "plan and the escalated round"
        );
        assert_eq!(turns(Role::Coder), Some(3));
        assert_eq!(turns(Role::Reviewer), Some(4));
        assert_eq!(turns(Role::Summarizer), None);
        assert_eq!(result.role_spend[&Role::Planner].tokens, 30);
        assert_eq!(
            result.spend[&State::Build].turns,
            4,
            "per-state spend stays"
        );
        for line in [
            "- role planner: 2 turn(s), 30 tokens",
            "- role coder: 3 turn(s), 45 tokens",
            "- role reviewer: 4 turn(s), 60 tokens",
        ] {
            assert!(result.summary.contains(line), "{}", result.summary);
        }
    }

    #[tokio::test]
    async fn compaction_spend_goes_to_the_summarizer() {
        let mut h = Harness::with_summarizer(
            vec![
                heavy_tool_turn("t1", 100),
                heavy_tool_turn("t2", 100),
                heavy_tool_turn("t3", 100),
                heavy_tool_turn("t4", 100),
                heavy_tool_turn("t5", 900),
                FakeProvider::text_turn("built"),
            ],
            vec![approve()],
            vec![FakeProvider::text_turn("## Task\nsummary of earlier work")],
        );
        add_planner(&mut h, vec![FakeProvider::text_turn(PLAN)]);
        let mut config = h.config(CheckCommand::Command("exit 0".to_owned()));
        config.context_window = 1000;
        let coder = Arc::clone(&h.coder) as Arc<dyn Provider>;
        let agent = h.agent(config, coder, "", Box::new(AutoApprove));
        let result = agent.run().await;
        assert_eq!(result.status, Status::Done);
        let turns = |role| result.role_spend.get(&role).map(|spend| spend.turns);
        assert_eq!(turns(Role::Summarizer), Some(1));
        assert_eq!(turns(Role::Planner), Some(1));
        assert_eq!(turns(Role::Coder), Some(6));
    }

    #[tokio::test]
    async fn resuming_in_build_restores_the_build_conversation_not_the_planners() {
        let mut h = Harness::new(vec![list_dir_turn()], vec![]);
        add_planner(&mut h, vec![list_dir_turn(), FakeProvider::text_turn(PLAN)]);
        let mut config = h.config(CheckCommand::Command("exit 0".to_owned()));
        config.limits.build.max_turns = 1;
        let coder = Arc::clone(&h.coder) as Arc<dyn Provider>;
        let first = h
            .agent(config, coder, "", Box::new(AutoApprove))
            .run()
            .await;
        assert!(matches!(first.status, Status::Incomplete { .. }));

        let snapshot = crate::context::load_session(h.dir.path(), "t1").expect("log");
        assert_eq!(snapshot.last_state, Some(State::Build));
        assert_eq!(
            snapshot.messages.len(),
            4,
            "seed, intro, tool call and results; the planner's turns are gone"
        );
        assert!(matches!(
            &snapshot.messages[0],
            Message::User { text } if text.contains("# Task") && text.contains("Write hello.txt")
        ));

        let resumed_coder = Arc::new(FakeProvider::new(vec![FakeProvider::text_turn("done")]));
        let resumed_planner = Arc::new(FakeProvider::new(vec![]));
        h.reviewer = Arc::new(FakeProvider::new(vec![approve()]));
        let (tx, rx) = mpsc::unbounded_channel();
        h.events = rx;
        let agent = AgentLoop::resume(
            snapshot,
            h.config(CheckCommand::Command("exit 0".to_owned())),
            LoopProviders {
                coder: Arc::clone(&resumed_coder) as Arc<dyn Provider>,
                planner: Some(Arc::clone(&resumed_planner) as Arc<dyn Provider>),
                reviewer: Arc::clone(&h.reviewer) as Arc<dyn Provider>,
                summarizer: Arc::clone(&h.summarizer) as Arc<dyn Provider>,
            },
            ToolRegistry::with_builtins(
                Permissions::empty(),
                Box::new(StaticPrompt(PromptAnswer::No)),
            ),
            ToolContext::new(h.dir.path()),
            Box::new(AutoApprove),
            EventSink::new(tx),
        );
        assert_eq!(agent.run().await.status, Status::Done);
        assert_eq!(resumed_planner.calls(), 0);
        let request = &resumed_coder.requests()[0];
        assert!(matches!(
            &request.messages[0],
            Message::User { text } if text.contains("# Task") && text.contains("Write hello.txt")
        ));
        assert_eq!(request.messages.len(), 5, "saved 4 and the resume note");
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
