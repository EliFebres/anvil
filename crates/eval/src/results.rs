//! Run results: the JSON written to `eval/results/` and read back by
//! `compare` and `gate`.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::EvalError;

/// Where results are written, relative to the repository root.
pub const RESULTS_DIR: &str = "eval/results";

/// The version of the JSON shape; bump it when a field changes meaning.
pub const SCHEMA_VERSION: u32 = 2;

/// The oldest schema [`RunResult::read`] still accepts: version 1 has no
/// `roles`, and its `model` is the coder.
pub const OLDEST_READABLE_SCHEMA: u32 = 1;

/// Token counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Uncached prompt tokens.
    pub input: u64,
    /// Completion tokens.
    pub output: u64,
    /// Prompt tokens served from the provider's cache.
    pub cached: u64,
}

impl Usage {
    /// Every prompt token the model read: uncached plus cached input.
    ///
    /// # Returns
    ///
    /// Input and cached added together.
    pub fn prompt(self) -> u64 {
        self.input.saturating_add(self.cached)
    }

    /// Every token counted.
    ///
    /// # Returns
    ///
    /// Input, output and cached added together.
    pub fn total(self) -> u64 {
        self.input
            .saturating_add(self.output)
            .saturating_add(self.cached)
    }

    /// Adds `other` into `self`.
    ///
    /// # Arguments
    ///
    /// * `other` - The counts to add.
    pub fn add(&mut self, other: Usage) {
        self.input = self.input.saturating_add(other.input);
        self.output = self.output.saturating_add(other.output);
        self.cached = self.cached.saturating_add(other.cached);
    }
}

/// The turns one loop state took, split by what each turn did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateTurns {
    /// Turns that made exactly one tool call.
    pub one_call: u32,
    /// Turns that made two or more tool calls.
    pub multi_call: u32,
    /// Turns that made no tool call outside review and verify.
    pub text: u32,
    /// Review and verify turns that made no tool call: a verdict.
    pub verdict: u32,
    /// Prompt tokens (uncached plus cached) these turns read.
    pub prompt_tokens: u64,
}

impl StateTurns {
    /// Every turn counted.
    ///
    /// # Returns
    ///
    /// The sum of the four kinds.
    pub fn total(&self) -> u32 {
        self.one_call + self.multi_call + self.text + self.verdict
    }

    /// Counts one turn of `state` that made `calls` tool calls.
    ///
    /// # Arguments
    ///
    /// * `state` - The loop state the turn ran in.
    /// * `calls` - The tool calls it made.
    /// * `prompt_tokens` - The prompt tokens it read.
    pub fn count(&mut self, state: &str, calls: u32, prompt_tokens: u64) {
        match calls {
            0 if state == "review" || state == "verify" => self.verdict += 1,
            0 => self.text += 1,
            1 => self.one_call += 1,
            _ => self.multi_call += 1,
        }
        self.prompt_tokens = self.prompt_tokens.saturating_add(prompt_tokens);
    }

    /// Adds `other` into `self`.
    ///
    /// # Arguments
    ///
    /// * `other` - The counts to add.
    pub fn add(&mut self, other: &StateTurns) {
        self.one_call += other.one_call;
        self.multi_call += other.multi_call;
        self.text += other.text;
        self.verdict += other.verdict;
        self.prompt_tokens = self.prompt_tokens.saturating_add(other.prompt_tokens);
    }
}

/// How one anvil run ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AttemptStatus {
    /// anvil reported done (exit 0).
    Done,
    /// anvil stopped before finishing (exit 2).
    Incomplete {
        /// anvil's reason, such as `turn_budget`.
        reason: String,
    },
    /// anvil stopped after a denied tool call (exit 3).
    Denied,
    /// anvil hit a provider error (exit 4).
    ProviderError,
    /// anvil refused to start, usually configuration (exit 1).
    SetupError,
    /// The harness killed anvil at the task's timeout.
    Timeout,
    /// anvil exited with an unexpected code or a signal.
    Crashed {
        /// The exit code, if there was one.
        code: Option<i32>,
    },
    /// The harness itself failed before or while running anvil.
    HarnessError {
        /// What went wrong.
        message: String,
    },
}

impl AttemptStatus {
    /// A short label for tables.
    ///
    /// # Returns
    ///
    /// `done`, `incomplete(reason)`, `timeout`, ...
    pub fn label(&self) -> String {
        match self {
            Self::Done => "done".to_owned(),
            Self::Incomplete { reason } => format!("incomplete({reason})"),
            Self::Denied => "denied".to_owned(),
            Self::ProviderError => "provider_error".to_owned(),
            Self::SetupError => "setup_error".to_owned(),
            Self::Timeout => "timeout".to_owned(),
            Self::Crashed { code: Some(code) } => format!("crashed({code})"),
            Self::Crashed { code: None } => "crashed(signal)".to_owned(),
            Self::HarnessError { message } => format!("harness_error({message})"),
        }
    }
}

/// Where the check command a session ran came from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckSource {
    /// anvil detected a runner (or was configured with one).
    Detected,
    /// Only the approved plan declared one.
    Plan,
    /// No check command ran; the model verified by hand.
    #[default]
    None,
}

/// What one session spent on loop overheads rather than on the task.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Overheads {
    /// Build turns that did nothing but tick plan checkboxes.
    pub lone_tick_turns: u32,
    /// Reviews the session ran.
    pub review_rounds: u32,
    /// Whether the reviewer still wanted changes when the round cap stopped it.
    pub review_cap_hit: bool,
    /// Model turns spent in the verify state.
    pub verify_model_turns: u32,
    /// Where the session's check command came from.
    pub check_source: CheckSource,
    /// Whether the memory summarizer ran.
    pub memory_ran: bool,
    /// Why the memory summarizer was skipped, when anvil said so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_skip_reason: Option<String>,
}

/// The provider and model that served one role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleModel {
    /// The provider name from config.
    pub provider: String,
    /// The model name from config.
    pub model: String,
}

/// One attempt at one task.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttemptRecord {
    /// 1-based repeat index.
    pub index: u32,
    /// Whether `check.sh` exited 0.
    pub passed: bool,
    /// How the anvil run ended.
    pub status: AttemptStatus,
    /// The exit code of `check.sh`, if it ran to completion.
    pub check_exit: Option<i32>,
    /// anvil's session id, when it printed one.
    pub session: Option<String>,
    /// The coder provider, from telemetry.
    pub provider: Option<String>,
    /// The coder model, from telemetry.
    pub model: Option<String>,
    /// The provider and model of each role seen in telemetry (`planner`,
    /// `coder`, `reviewer`, `summarizer`); empty in results written before
    /// schema 2.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub roles: BTreeMap<String, RoleModel>,
    /// Tokens over every provider the run used.
    pub tokens: Usage,
    /// Tool calls the session made; 0 in results written before it was recorded.
    #[serde(default)]
    pub tool_calls: u32,
    /// Model turns over every provider; 0 in results written before it was recorded.
    #[serde(default)]
    pub turns: u32,
    /// Turns per loop state (`plan`, `build`, ...); empty in results written
    /// before it was recorded.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub turns_by_state: BTreeMap<String, StateTurns>,
    /// The loop overheads, written as fields of the attempt itself; `None`
    /// in results written before they were recorded.
    #[serde(flatten, default, skip_serializing_if = "Option::is_none")]
    pub overheads: Option<Overheads>,
    /// Cost in USD, when every provider used has a price.
    pub cost_usd: Option<f64>,
    /// Wall time of the anvil run.
    pub anvil_ms: u64,
    /// Wall time of `check.sh`.
    pub check_ms: u64,
    /// Wall time of the whole attempt, copy and check included.
    pub wall_ms: u64,
    /// The last lines anvil printed to stderr.
    pub stderr_tail: String,
    /// The last lines `check.sh` printed.
    pub check_tail: String,
}

/// Every attempt at one task.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskResult {
    /// The task name.
    pub name: String,
    /// The task's tags.
    pub tags: Vec<String>,
    /// The attempts, in index order.
    pub attempts: Vec<AttemptRecord>,
}

impl TaskResult {
    /// How many attempts passed.
    ///
    /// # Returns
    ///
    /// The count.
    pub fn passes(&self) -> usize {
        self.attempts.iter().filter(|a| a.passed).count()
    }

    /// Whether a strict majority of attempts passed.
    ///
    /// 1/1 and 2/3 pass; 1/2 and 1/3 do not.
    ///
    /// # Returns
    ///
    /// `true` for a strict majority.
    pub fn is_passing(&self) -> bool {
        self.passes() * 2 > self.attempts.len()
    }

    /// `PASS 2/3` or `FAIL 1/3`.
    ///
    /// # Returns
    ///
    /// The verdict text.
    pub fn verdict(&self) -> String {
        let word = if self.is_passing() { "PASS" } else { "FAIL" };
        format!("{word} {}/{}", self.passes(), self.attempts.len())
    }
}

/// The totals of a run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Totals {
    /// Tasks run.
    pub tasks: u32,
    /// Attempts run.
    pub attempts: u32,
    /// Attempts whose check passed.
    pub passed_attempts: u32,
    /// Tasks passing by strict majority.
    pub tasks_passing: u32,
    /// `passed_attempts / attempts`.
    pub pass_rate: f64,
    /// The lowest share of tasks passing in any one repeat.
    pub pass_rate_min: f64,
    /// The highest share of tasks passing in any one repeat.
    pub pass_rate_max: f64,
    /// Tokens over every attempt.
    pub tokens: Usage,
    /// Cost over every attempt, when every attempt could be priced.
    pub cost_usd: Option<f64>,
    /// `cost_usd / tasks_passing`, when both are known and non-zero.
    pub cost_per_passed_task: Option<f64>,
    /// Wall time of the whole run.
    pub wall_ms: u64,
}

impl Totals {
    /// Sums the task results.
    ///
    /// # Arguments
    ///
    /// * `tasks` - The task results.
    /// * `repeat` - How many attempts each task had.
    /// * `wall_ms` - The run's wall time.
    ///
    /// # Returns
    ///
    /// The totals; the spread is the share of tasks passing in the best and
    /// worst repeat.
    pub fn compute(tasks: &[TaskResult], repeat: u32, wall_ms: u64) -> Totals {
        let attempts: Vec<&AttemptRecord> = tasks.iter().flat_map(|t| &t.attempts).collect();
        let passed_attempts = attempts.iter().filter(|a| a.passed).count();
        let tasks_passing = tasks.iter().filter(|t| t.is_passing()).count();
        let mut tokens = Usage::default();
        let mut cost = Some(0.0);
        for attempt in &attempts {
            tokens.add(attempt.tokens);
            cost = match (cost, attempt.cost_usd) {
                (Some(total), Some(c)) => Some(total + c),
                _ => None,
            };
        }
        let (pass_rate_min, pass_rate_max) = spread(tasks, repeat);
        Totals {
            tasks: tasks.len() as u32,
            attempts: attempts.len() as u32,
            passed_attempts: passed_attempts as u32,
            tasks_passing: tasks_passing as u32,
            pass_rate: ratio(passed_attempts, attempts.len()),
            pass_rate_min,
            pass_rate_max,
            tokens,
            cost_usd: cost,
            cost_per_passed_task: cost
                .filter(|_| tasks_passing > 0)
                .map(|c| c / tasks_passing as f64),
            wall_ms,
        }
    }
}

/// The share of tasks passing in the worst and best repeat index.
fn spread(tasks: &[TaskResult], repeat: u32) -> (f64, f64) {
    if tasks.is_empty() || repeat == 0 {
        return (0.0, 0.0);
    }
    let mut min = f64::MAX;
    let mut max = f64::MIN;
    for index in 1..=repeat {
        let passing = tasks
            .iter()
            .filter(|t| t.attempts.iter().any(|a| a.index == index && a.passed))
            .count();
        let share = ratio(passing, tasks.len());
        min = min.min(share);
        max = max.max(share);
    }
    (min, max)
}

/// `part / whole`, or 0 when `whole` is 0.
fn ratio(part: usize, whole: usize) -> f64 {
    if whole == 0 {
        0.0
    } else {
        part as f64 / whole as f64
    }
}

/// What a run spent per attempt, computed from its attempts on demand so
/// results written before these numbers existed can still be compared.
///
/// "Per task" means per attempt: each attempt is one task run once.
#[derive(Debug, Clone, PartialEq)]
pub struct Efficiency {
    /// Attempts counted.
    pub attempts: usize,
    /// Passed attempts / attempts; 0 when there are none.
    pub pass_rate: f64,
    /// Mean model turns per attempt.
    pub turns_mean: f64,
    /// Prompt tokens over every attempt divided by turns over every attempt:
    /// how much context each turn re-sends. `None` when no turns were recorded.
    pub context_per_turn: Option<f64>,
    /// Mean prompt tokens (uncached plus cached input) per attempt.
    pub input_mean: f64,
    /// Median prompt tokens per attempt.
    pub input_median: f64,
    /// Mean uncached input tokens per attempt.
    pub uncached_input_mean: f64,
    /// Prompt tokens over every attempt divided by passed attempts.
    pub input_per_passed: Option<f64>,
    /// Mean tool calls per attempt.
    pub tool_calls_mean: f64,
    /// Mean turns per attempt that made two or more tool calls.
    pub multi_call_turns_mean: f64,
    /// Turns per loop state summed over every attempt; divide by `attempts`
    /// for a per-task figure.
    pub by_state: BTreeMap<String, StateTurns>,
    /// Loop overheads over the attempts that recorded them; `None` when no
    /// attempt did.
    pub overheads: Option<OverheadTotals>,
    /// Mean cost per attempt, when every attempt could be priced.
    pub cost_per_attempt: Option<f64>,
}

/// The loop overheads of a set of attempts, summed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OverheadTotals {
    /// Attempts that recorded their overheads.
    pub attempts: usize,
    /// Build turns that did nothing but tick plan checkboxes.
    pub lone_tick_turns: u64,
    /// Reviews run.
    pub review_rounds: u64,
    /// Attempts stopped by the review round cap.
    pub review_cap_hits: u64,
    /// Model turns spent in the verify state.
    pub verify_model_turns: u64,
    /// Attempts whose check command was detected.
    pub check_detected: u64,
    /// Attempts whose check command came from the plan alone.
    pub check_plan: u64,
    /// Attempts that ran no check command.
    pub check_none: u64,
    /// Attempts whose memory summarizer ran.
    pub memory_ran: u64,
    /// Attempts whose memory summarizer was skipped, per reason.
    pub memory_skipped: BTreeMap<String, u64>,
}

impl OverheadTotals {
    /// Sums the overheads of the attempts that recorded them.
    ///
    /// # Arguments
    ///
    /// * `attempts` - The attempts.
    ///
    /// # Returns
    ///
    /// The totals, or `None` when no attempt recorded its overheads.
    pub fn of(attempts: &[&AttemptRecord]) -> Option<OverheadTotals> {
        let mut totals = OverheadTotals::default();
        for overheads in attempts.iter().filter_map(|a| a.overheads.as_ref()) {
            totals.attempts += 1;
            totals.lone_tick_turns += u64::from(overheads.lone_tick_turns);
            totals.review_rounds += u64::from(overheads.review_rounds);
            totals.review_cap_hits += u64::from(overheads.review_cap_hit);
            totals.verify_model_turns += u64::from(overheads.verify_model_turns);
            match overheads.check_source {
                CheckSource::Detected => totals.check_detected += 1,
                CheckSource::Plan => totals.check_plan += 1,
                CheckSource::None => totals.check_none += 1,
            }
            totals.memory_ran += u64::from(overheads.memory_ran);
            if let Some(reason) = &overheads.memory_skip_reason {
                *totals.memory_skipped.entry(reason.clone()).or_default() += 1;
            }
        }
        (totals.attempts > 0).then_some(totals)
    }
}

impl Efficiency {
    /// Computes the per-attempt figures of a set of task results.
    ///
    /// # Arguments
    ///
    /// * `tasks` - The task results.
    ///
    /// # Returns
    ///
    /// The figures; all zero (and `None` where undefined) when there are no
    /// attempts.
    pub fn of(tasks: &[TaskResult]) -> Efficiency {
        let attempts: Vec<&AttemptRecord> = tasks.iter().flat_map(|t| &t.attempts).collect();
        let count = attempts.len();
        let mut prompts: Vec<u64> = attempts.iter().map(|a| a.tokens.prompt()).collect();
        prompts.sort_unstable();
        let prompt_sum: u64 = prompts.iter().sum();
        let passed = attempts.iter().filter(|a| a.passed).count();
        let cost: Option<f64> = attempts.iter().map(|a| a.cost_usd).sum();
        let turns: u64 = attempts.iter().map(|a| u64::from(a.turns)).sum();
        let mut by_state: BTreeMap<String, StateTurns> = BTreeMap::new();
        for (state, counted) in attempts.iter().flat_map(|a| &a.turns_by_state) {
            by_state.entry(state.clone()).or_default().add(counted);
        }
        let multi_call: u64 = by_state.values().map(|s| u64::from(s.multi_call)).sum();
        Efficiency {
            attempts: count,
            pass_rate: ratio(passed, count),
            turns_mean: mean(turns, count),
            context_per_turn: (turns > 0).then(|| prompt_sum as f64 / turns as f64),
            input_mean: mean(prompt_sum, count),
            input_median: median(&prompts),
            uncached_input_mean: mean(attempts.iter().map(|a| a.tokens.input).sum(), count),
            input_per_passed: (passed > 0).then(|| prompt_sum as f64 / passed as f64),
            tool_calls_mean: mean(
                attempts.iter().map(|a| u64::from(a.tool_calls)).sum(),
                count,
            ),
            multi_call_turns_mean: mean(multi_call, count),
            by_state,
            overheads: OverheadTotals::of(&attempts),
            cost_per_attempt: cost.filter(|_| count > 0).map(|c| c / count as f64),
        }
    }
}

/// `sum / count`, or 0 when `count` is 0.
fn mean(sum: u64, count: usize) -> f64 {
    if count == 0 {
        0.0
    } else {
        sum as f64 / count as f64
    }
}

/// The median of sorted values, or 0 when there are none.
fn median(sorted: &[u64]) -> f64 {
    debug_assert!(sorted.is_sorted(), "median needs sorted values");
    match sorted.len() {
        0 => 0.0,
        n if n % 2 == 1 => sorted[n / 2] as f64,
        n => (sorted[n / 2 - 1] + sorted[n / 2]) as f64 / 2.0,
    }
}

/// One whole run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunResult {
    /// [`SCHEMA_VERSION`] at the time of writing.
    pub schema: u32,
    /// When the run started.
    pub started_at: DateTime<Utc>,
    /// When the run finished.
    pub finished_at: DateTime<Utc>,
    /// Short git sha of the repository under test.
    pub git_sha: String,
    /// The provider requested with `--provider`, if any.
    pub provider: Option<String>,
    /// The first coder model seen in telemetry.
    pub model: Option<String>,
    /// The first provider and model seen for each role; empty in results
    /// written before schema 2, where `model` is the coder.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub roles: BTreeMap<String, RoleModel>,
    /// What ran the attempts: the anvil binary path, or a fake's label.
    pub runner: String,
    /// Attempts per task.
    pub repeat: u32,
    /// Attempts run at once.
    pub parallel: u32,
    /// The tag filter, empty for all tasks.
    pub tags: Vec<String>,
    /// Per-task results, sorted by name.
    pub tasks: Vec<TaskResult>,
    /// The totals.
    pub totals: Totals,
    /// The kept attempt directories (`--keep`); never written to the file.
    #[serde(skip)]
    pub work_dir: Option<PathBuf>,
}

impl RunResult {
    /// `<YYYY-MM-DD>-<sha>-<model>.json`.
    ///
    /// The model falls back to the provider, then to `unknown`; characters
    /// outside `[A-Za-z0-9._-]` become `_`.
    ///
    /// # Returns
    ///
    /// The file name.
    pub fn file_name(&self) -> String {
        let model = self
            .model
            .as_deref()
            .or(self.provider.as_deref())
            .unwrap_or("unknown");
        format!(
            "{}-{}-{}.json",
            self.started_at.format("%Y-%m-%d"),
            sanitize(&self.git_sha),
            sanitize(model)
        )
    }

    /// `<sha> <model>` for table headers.
    ///
    /// # Returns
    ///
    /// The label.
    pub fn label(&self) -> String {
        let model = self
            .model
            .as_deref()
            .or(self.provider.as_deref())
            .unwrap_or(&self.runner);
        format!("{} {model}", self.git_sha)
    }

    /// The roles line of `eval compare`: each role with its model.
    ///
    /// # Returns
    ///
    /// `planner m1, coder m2`, or an empty string when the run used no more
    /// than the coder, so a coder-only run shows nothing extra.
    pub fn roles_label(&self) -> String {
        if self.roles.len() < 2 {
            return String::new();
        }
        self.roles
            .iter()
            .map(|(role, served)| format!("{role} {}", served.model))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Writes the result as pretty JSON into `dir`.
    ///
    /// # Arguments
    ///
    /// * `dir` - The results folder; created if missing.
    ///
    /// # Returns
    ///
    /// The file written. A name that exists already gets a `-2`, `-3`, ...
    /// suffix rather than being overwritten.
    ///
    /// # Errors
    ///
    /// When the folder or file cannot be written.
    pub fn write(&self, dir: &Path) -> Result<PathBuf, EvalError> {
        std::fs::create_dir_all(dir).map_err(|source| EvalError::io("creating", dir, source))?;
        let text = serde_json::to_string_pretty(self).map_err(|source| EvalError::Serialize {
            what: "the run result",
            source,
        })?;
        let path = free_path(dir, &self.file_name());
        std::fs::write(&path, text).map_err(|source| EvalError::io("writing", &path, source))?;
        Ok(path)
    }

    /// Reads a result file.
    ///
    /// # Arguments
    ///
    /// * `path` - The JSON file.
    ///
    /// # Returns
    ///
    /// The result.
    ///
    /// # Errors
    ///
    /// When the file cannot be read or parsed, or its schema version is not
    /// one this build reads (1 and 2).
    pub fn read(path: &Path) -> Result<RunResult, EvalError> {
        let text = std::fs::read_to_string(path)
            .map_err(|source| EvalError::io("reading", path, source))?;
        let result: RunResult = serde_json::from_str(&text).map_err(|source| EvalError::Json {
            path: path.to_owned(),
            source,
        })?;
        if !(OLDEST_READABLE_SCHEMA..=SCHEMA_VERSION).contains(&result.schema) {
            return Err(EvalError::Schema {
                path: path.to_owned(),
                found: result.schema,
                expected: SCHEMA_VERSION,
            });
        }
        Ok(result)
    }

    /// The result for one task.
    ///
    /// # Arguments
    ///
    /// * `name` - The task name.
    ///
    /// # Returns
    ///
    /// The task result, if the run included it.
    pub fn task(&self, name: &str) -> Option<&TaskResult> {
        self.tasks.iter().find(|t| t.name == name)
    }
}

impl fmt::Display for RunResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let width = self
            .tasks
            .iter()
            .map(|t| t.name.len())
            .max()
            .unwrap_or(4)
            .max(4);
        writeln!(f, "{:<width$}  result    status", "task")?;
        for task in &self.tasks {
            let statuses: Vec<String> = task
                .attempts
                .iter()
                .map(|a| {
                    let capped = a.overheads.as_ref().is_some_and(|o| o.review_cap_hit);
                    // A capped attempt whose check passed must not read as a plain pass.
                    format!(
                        "{}{}",
                        a.status.label(),
                        if capped { "[review-cap]" } else { "" }
                    )
                })
                .collect();
            writeln!(
                f,
                "{:<width$}  {:<8}  {}",
                task.name,
                task.verdict(),
                statuses.join(" ")
            )?;
        }
        let t = &self.totals;
        writeln!(f)?;
        writeln!(f, "tasks passing         {}/{}", t.tasks_passing, t.tasks)?;
        writeln!(
            f,
            "pass rate             {} (spread {}..{})",
            percent(t.pass_rate),
            percent(t.pass_rate_min),
            percent(t.pass_rate_max)
        )?;
        writeln!(f, "tokens                {}", thousands(t.tokens.total()))?;
        writeln!(f, "cost (USD)            {}", money(t.cost_usd))?;
        writeln!(f, "cost per passed task  {}", money(t.cost_per_passed_task))?;
        let e = Efficiency::of(&self.tasks);
        writeln!(
            f,
            "input tok / task      mean {}, median {}",
            thousands(e.input_mean as u64),
            thousands(e.input_median as u64)
        )?;
        writeln!(
            f,
            "input tok / passed    {}",
            e.input_per_passed
                .map_or_else(|| "n/a".to_owned(), |v| thousands(v as u64))
        )?;
        writeln!(f, "tool calls / task     {:.1}", e.tool_calls_mean)?;
        writeln!(f, "turns / task          {:.1}", e.turns_mean)?;
        writeln!(
            f,
            "context / turn        {}",
            e.context_per_turn
                .map_or_else(|| "n/a".to_owned(), |v| thousands(v as u64))
        )?;
        writeln!(f, "cost per task         {}", money(e.cost_per_attempt))?;
        if let Some(o) = &e.overheads {
            writeln!(f, "review cap hits       {}", o.review_cap_hits)?;
            writeln!(f, "lone tick turns       {}", o.lone_tick_turns)?;
            writeln!(f, "verify model turns    {}", o.verify_model_turns)?;
            writeln!(
                f,
                "memory ran            {} of {}",
                o.memory_ran, o.attempts
            )?;
        }
        writeln!(f, "wall                  {}", duration(t.wall_ms))
    }
}

/// `dir/name`, or the first `dir/stem-N.json` that does not exist yet.
fn free_path(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let stem = name.strip_suffix(".json").unwrap_or(name);
    (2..)
        .map(|n| dir.join(format!("{stem}-{n}.json")))
        .find(|p| !p.exists())
        .unwrap_or(first)
}

/// Keeps `[A-Za-z0-9._-]`, replacing anything else with `_`.
fn sanitize(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// `88.3%`.
pub(crate) fn percent(rate: f64) -> String {
    format!("{:.1}%", rate * 100.0)
}

/// `1,234,567`.
pub(crate) fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `1.2345` or `n/a`.
pub(crate) fn money(value: Option<f64>) -> String {
    value.map_or_else(|| "n/a".to_owned(), |v| format!("{v:.4}"))
}

/// `12m03s` or `48.1s`.
pub(crate) fn duration(ms: u64) -> String {
    let secs = ms / 1000;
    if secs >= 60 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}

#[cfg(test)]
pub(crate) mod samples {
    //! Hand-built results shared by the result, compare and gate tests.

    use super::*;

    /// One attempt with the given outcome and a fixed cost.
    pub fn attempt(index: u32, passed: bool) -> AttemptRecord {
        AttemptRecord {
            index,
            passed,
            status: if passed {
                AttemptStatus::Done
            } else {
                AttemptStatus::Incomplete {
                    reason: "turn_budget".to_owned(),
                }
            },
            check_exit: Some(if passed { 0 } else { 1 }),
            session: Some(format!("01ARZ3NDEKTSV4RRFFQ69G5FA{index}")),
            provider: Some("fast".to_owned()),
            model: Some("m1".to_owned()),
            roles: BTreeMap::new(),
            tokens: Usage {
                input: 1000,
                output: 100,
                cached: 50,
            },
            tool_calls: if passed { 4 } else { 9 },
            turns: if passed { 3 } else { 7 },
            turns_by_state: BTreeMap::new(),
            overheads: None,
            cost_usd: Some(0.01),
            anvil_ms: 1500,
            check_ms: 200,
            wall_ms: 1800,
            stderr_tail: "done".to_owned(),
            check_tail: String::new(),
        }
    }

    /// A task whose attempts passed as `outcomes` says.
    pub fn task(name: &str, outcomes: &[bool]) -> TaskResult {
        TaskResult {
            name: name.to_owned(),
            tags: vec!["py".to_owned()],
            attempts: outcomes
                .iter()
                .enumerate()
                .map(|(i, &passed)| attempt(i as u32 + 1, passed))
                .collect(),
        }
    }

    /// A run over `tasks` with a fixed clock and sha.
    pub fn run(sha: &str, model: &str, tasks: Vec<TaskResult>) -> RunResult {
        let repeat = tasks.first().map_or(1, |t| t.attempts.len() as u32);
        let started_at = "2026-10-30T10:00:00Z".parse().expect("time");
        let totals = Totals::compute(&tasks, repeat, 125_000);
        RunResult {
            schema: SCHEMA_VERSION,
            started_at,
            finished_at: "2026-10-30T10:02:05Z".parse().expect("time"),
            git_sha: sha.to_owned(),
            provider: Some("fast".to_owned()),
            model: Some(model.to_owned()),
            roles: BTreeMap::new(),
            runner: "anvil".to_owned(),
            repeat,
            parallel: 2,
            tags: Vec::new(),
            tasks,
            totals,
            work_dir: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::samples::{attempt, run, task};
    use super::*;

    #[test]
    fn majority_rule_is_strict() {
        assert!(task("a", &[true]).is_passing());
        assert!(task("a", &[true, false, true]).is_passing());
        assert!(!task("a", &[true, false]).is_passing());
        assert!(!task("a", &[true, false, false]).is_passing());
        assert_eq!(task("a", &[true, false, true]).verdict(), "PASS 2/3");
        assert_eq!(task("a", &[false]).verdict(), "FAIL 0/1");
    }

    #[test]
    fn totals_sum_and_spread() {
        let tasks = vec![
            task("a", &[true, true, true]),
            task("b", &[true, false, true]),
            task("c", &[false, false, false]),
        ];
        let totals = Totals::compute(&tasks, 3, 9000);
        assert_eq!(totals.tasks, 3);
        assert_eq!(totals.attempts, 9);
        assert_eq!(totals.passed_attempts, 5);
        assert_eq!(totals.tasks_passing, 2);
        assert!((totals.pass_rate - 5.0 / 9.0).abs() < 1e-9);
        assert!((totals.pass_rate_min - 1.0 / 3.0).abs() < 1e-9);
        assert!((totals.pass_rate_max - 2.0 / 3.0).abs() < 1e-9);
        assert_eq!(totals.tokens.total(), 9 * 1150);
        assert!((totals.cost_usd.expect("priced") - 0.09).abs() < 1e-9);
        assert!((totals.cost_per_passed_task.expect("priced") - 0.045).abs() < 1e-9);
    }

    #[test]
    fn an_unpriced_attempt_makes_the_cost_unknown() {
        let mut t = task("a", &[true, true]);
        t.attempts[1].cost_usd = None;
        let totals = Totals::compute(&[t], 2, 1);
        assert_eq!(totals.cost_usd, None);
        assert_eq!(totals.cost_per_passed_task, None);
    }

    #[test]
    fn file_name_is_date_sha_model_and_never_overwrites() {
        let result = run("fe92bc2", "claude-opus-5-5", vec![task("a", &[true])]);
        assert_eq!(
            result.file_name(),
            "2026-10-30-fe92bc2-claude-opus-5-5.json"
        );
        let mut odd = result.clone();
        odd.model = Some("org/model:latest".to_owned());
        assert_eq!(odd.file_name(), "2026-10-30-fe92bc2-org_model_latest.json");
        let dir = tempfile::tempdir().expect("tmp");
        let first = result.write(dir.path()).expect("writes");
        let second = result.write(dir.path()).expect("writes");
        assert!(first.ends_with("2026-10-30-fe92bc2-claude-opus-5-5.json"));
        assert!(second.ends_with("2026-10-30-fe92bc2-claude-opus-5-5-2.json"));
        assert_eq!(RunResult::read(&first).expect("reads"), result);
    }

    #[test]
    fn schema_one_files_without_roles_still_read() {
        let mut result = run("abc", "m", vec![task("a", &[true])]);
        result.schema = 1;
        let dir = tempfile::tempdir().expect("tmp");
        let path = result.write(dir.path()).expect("writes");
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(!text.contains("\"roles\""), "an empty map is not written");
        let read = RunResult::read(&path).expect("schema 1 reads");
        assert_eq!(read.schema, 1);
        assert!(read.roles.is_empty());
        assert_eq!(read.model.as_deref(), Some("m"));
    }

    #[test]
    fn every_committed_result_still_reads() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../eval/results");
        let mut seen = 0;
        for entry in std::fs::read_dir(&dir).expect("eval/results exists") {
            let path = entry.expect("entry").path();
            if path.extension().is_some_and(|ext| ext == "json") {
                RunResult::read(&path)
                    .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
                seen += 1;
            }
        }
        assert!(seen > 0, "the repository has committed results");
    }

    #[test]
    fn roles_label_shows_only_beyond_the_coder() {
        let mut result = run("abc", "m", vec![task("a", &[true])]);
        let served = |model: &str| RoleModel {
            provider: "p".to_owned(),
            model: model.to_owned(),
        };
        assert_eq!(result.roles_label(), "");
        result.roles.insert("coder".to_owned(), served("cheap"));
        assert_eq!(result.roles_label(), "");
        result.roles.insert("planner".to_owned(), served("dear"));
        assert_eq!(result.roles_label(), "coder cheap, planner dear");
    }

    #[test]
    fn read_rejects_other_schema_versions() {
        let mut result = run("abc", "m", vec![task("a", &[true])]);
        result.schema = 99;
        let dir = tempfile::tempdir().expect("tmp");
        let path = result.write(dir.path()).expect("writes");
        assert!(matches!(
            RunResult::read(&path),
            Err(EvalError::Schema { found: 99, .. })
        ));
        std::fs::write(&path, "{").expect("write");
        assert!(matches!(
            RunResult::read(&path),
            Err(EvalError::Json { .. })
        ));
    }

    #[test]
    fn json_shape_is_stable() {
        let result = run(
            "fe92bc2",
            "m1",
            vec![task("a", &[true, false]), task("b", &[true, true])],
        );
        insta::assert_json_snapshot!(result);
    }

    #[test]
    fn overheads_are_flat_fields_and_absent_in_older_files() {
        let mut recorded = attempt(1, true);
        recorded.overheads = Some(Overheads {
            lone_tick_turns: 1,
            review_rounds: 4,
            review_cap_hit: true,
            verify_model_turns: 2,
            check_source: CheckSource::Plan,
            memory_ran: false,
            memory_skip_reason: Some("uneventful_small_diff".to_owned()),
        });
        let json = serde_json::to_value(&recorded).expect("serializes");
        assert_eq!(json["review_cap_hit"], true);
        assert_eq!(json["check_source"], "plan");
        assert_eq!(json["memory_skip_reason"], "uneventful_small_diff");
        assert!(json.get("overheads").is_none(), "the fields are flat");
        let back: AttemptRecord = serde_json::from_value(json).expect("reads");
        assert_eq!(back, recorded);

        let older = serde_json::to_value(attempt(1, true)).expect("serializes");
        assert!(older.get("lone_tick_turns").is_none());
        let back: AttemptRecord = serde_json::from_value(older).expect("reads");
        assert_eq!(back.overheads, None);

        let totals = OverheadTotals::of(&[&recorded, &attempt(2, true)]).expect("one recorded");
        assert_eq!((totals.attempts, totals.review_cap_hits), (1, 1));
        assert_eq!((totals.check_plan, totals.memory_ran), (1, 0));
        assert_eq!(totals.memory_skipped["uneventful_small_diff"], 1);
        assert_eq!(OverheadTotals::of(&[&attempt(1, true)]), None);
    }

    #[test]
    fn efficiency_takes_means_medians_and_per_pass_figures() {
        let mut t = task("t", &[true, false, true, false]);
        for (attempt, input) in t.attempts.iter_mut().zip([100, 400, 200, 300]) {
            attempt.tokens = Usage {
                input,
                output: 7,
                cached: 0,
            };
        }
        let even = Efficiency::of(std::slice::from_ref(&t));
        assert_eq!(even.input_mean, 250.0);
        assert_eq!(even.input_median, 250.0);
        assert_eq!(even.input_per_passed, Some(500.0));
        assert_eq!(even.tool_calls_mean, 6.5);
        assert_eq!(even.cost_per_attempt, Some(0.01));

        t.attempts.pop();
        assert_eq!(Efficiency::of(std::slice::from_ref(&t)).input_median, 200.0);

        let mut build = StateTurns::default();
        build.count("build", 3, 100);
        build.count("build", 1, 50);
        let mut review = StateTurns::default();
        review.count("review", 0, 20);
        for attempt in &mut t.attempts {
            attempt.turns_by_state =
                BTreeMap::from([("build".to_owned(), build), ("review".to_owned(), review)]);
        }
        let states = Efficiency::of(std::slice::from_ref(&t));
        assert_eq!(states.multi_call_turns_mean, 1.0);
        assert_eq!(states.by_state["build"].total(), 6);
        assert_eq!(states.by_state["build"].prompt_tokens, 450);
        assert_eq!(states.by_state["review"].verdict, 3);

        let failing = task("f", &[false, false]);
        assert_eq!(Efficiency::of(&[failing]).input_per_passed, None);
        let empty = Efficiency::of(&[]);
        assert_eq!(empty.input_mean, 0.0);
        assert_eq!(empty.cost_per_attempt, None);
    }

    #[test]
    fn summary_text_is_stable() {
        let result = run(
            "fe92bc2",
            "m1",
            vec![
                task("py-one", &[true, false, true]),
                task("rs-two", &[false, false, false]),
            ],
        );
        insta::assert_snapshot!(result.to_string());
    }

    #[test]
    fn a_review_cap_hit_shows_in_the_summary_even_when_the_check_passed() {
        let mut capped = task("py-capped", &[true]);
        capped.attempts[0].status = AttemptStatus::Denied;
        capped.attempts[0].overheads = Some(Overheads {
            review_rounds: 4,
            review_cap_hit: true,
            ..Overheads::default()
        });
        let text = run("fe92bc2", "m1", vec![capped]).to_string();
        assert!(text.contains("PASS 1/1  denied[review-cap]"), "{text}");
        assert!(text.contains("review cap hits       1"), "{text}");
    }

    #[test]
    fn number_formatting() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_234_567), "1,234,567");
        assert_eq!(percent(0.8833), "88.3%");
        assert_eq!(money(None), "n/a");
        assert_eq!(money(Some(1.23456)), "1.2346");
        assert_eq!(duration(48_100), "48.1s");
        assert_eq!(duration(723_000), "12m03s");
    }
}
