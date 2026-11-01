//! Run results: the JSON written to `eval/results/` and read back by
//! `compare` and `gate`.

use std::fmt;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::EvalError;

/// Where results are written, relative to the repository root.
pub const RESULTS_DIR: &str = "eval/results";

/// The version of the JSON shape; bump it when a field changes meaning.
pub const SCHEMA_VERSION: u32 = 1;

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
    /// Tokens over every provider the run used.
    pub tokens: Usage,
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
    /// When the file cannot be read or parsed, or its schema version differs.
    pub fn read(path: &Path) -> Result<RunResult, EvalError> {
        let text = std::fs::read_to_string(path)
            .map_err(|source| EvalError::io("reading", path, source))?;
        let result: RunResult = serde_json::from_str(&text).map_err(|source| EvalError::Json {
            path: path.to_owned(),
            source,
        })?;
        if result.schema != SCHEMA_VERSION {
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
            let statuses: Vec<String> = task.attempts.iter().map(|a| a.status.label()).collect();
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
            tokens: Usage {
                input: 1000,
                output: 100,
                cached: 50,
            },
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
            runner: "anvil".to_owned(),
            repeat,
            parallel: 2,
            tags: Vec::new(),
            tasks,
            totals,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::samples::{run, task};
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
