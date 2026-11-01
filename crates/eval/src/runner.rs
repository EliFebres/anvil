//! The run loop: every task, `repeat` times, at most `parallel` at once.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use chrono::Utc;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::check::{self, CheckResult, forward_slashes};
use crate::error::EvalError;
use crate::git;
use crate::results::{
    AttemptRecord, AttemptStatus, RunResult, SCHEMA_VERSION, TaskResult, Totals, Usage,
};
use crate::task::{Task, copy_fixture, load_tasks};

/// What one attempt at a task produced, before `check.sh` judges it.
#[derive(Debug, Clone, PartialEq)]
pub struct AttemptOutcome {
    /// How the run ended.
    pub status: AttemptStatus,
    /// The session id, when known.
    pub session: Option<String>,
    /// The coder provider, when known.
    pub provider: Option<String>,
    /// The coder model, when known.
    pub model: Option<String>,
    /// Tokens used.
    pub tokens: Usage,
    /// Cost in USD, when every provider used has a price.
    pub cost_usd: Option<f64>,
    /// Wall time of the run.
    pub anvil_ms: u64,
    /// The last lines of stderr.
    pub stderr_tail: String,
}

impl AttemptOutcome {
    /// An outcome for a harness failure: nothing ran.
    ///
    /// # Arguments
    ///
    /// * `message` - What went wrong.
    ///
    /// # Returns
    ///
    /// The outcome.
    pub fn harness_error(message: String) -> Self {
        AttemptOutcome {
            status: AttemptStatus::HarnessError { message },
            session: None,
            provider: None,
            model: None,
            tokens: Usage::default(),
            cost_usd: Some(0.0),
            anvil_ms: 0,
            stderr_tail: String::new(),
        }
    }
}

/// Everything a runner needs to know about one attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptContext<'a> {
    /// The task.
    pub task: &'a Task,
    /// The fixture copy to work in.
    pub work_dir: &'a Path,
    /// 1-based repeat index.
    pub index: u32,
    /// Environment the run and the check share: `EVAL_TASK_DIR`,
    /// `EVAL_PYTHON` and a per-attempt `CARGO_TARGET_DIR`.
    pub env: &'a [(String, String)],
}

/// The future a runner returns for one attempt.
pub type AttemptFuture<'a> =
    Pin<Box<dyn Future<Output = Result<AttemptOutcome, EvalError>> + Send + 'a>>;

/// Something that works on a fixture copy: anvil, or a fake in tests.
///
/// The harness copies the fixture, calls [`EvalRunner::run_attempt`], then
/// runs `check.sh`. A runner never runs the check itself.
pub trait EvalRunner: Send + Sync {
    /// What this runner is, for the result file.
    ///
    /// # Returns
    ///
    /// The anvil binary path, or a fake's label.
    fn label(&self) -> String;

    /// Works on one fixture copy.
    ///
    /// # Arguments
    ///
    /// * `attempt` - The task, the copy and the shared environment.
    ///
    /// # Returns
    ///
    /// The outcome. An `Err` is recorded as a harness error for that attempt
    /// and never aborts the run.
    fn run_attempt<'a>(&'a self, attempt: &'a AttemptContext<'a>) -> AttemptFuture<'a>;
}

/// A runner that follows a script: per task, one outcome per attempt index.
///
/// A scripted success writes `DONE` and `notes.md` into the fixture copy,
/// which the fixture tasks under `tests/fixtures/tasks` check for. Anything
/// not in the script fails. For tests of the harness and of the factory.
#[derive(Debug, Clone)]
pub struct FakeRunner {
    /// Outcomes per task name, by attempt index.
    script: HashMap<String, Vec<bool>>,
    /// Panic instead of answering, to test harness recovery.
    panic: bool,
}

impl FakeRunner {
    /// A runner that follows `entries`.
    ///
    /// # Arguments
    ///
    /// * `entries` - Task name and the outcome of each attempt, in order.
    ///
    /// # Returns
    ///
    /// The runner, shared so it can be handed to [`run_with`].
    pub fn scripted(entries: &[(&str, &[bool])]) -> Arc<dyn EvalRunner> {
        Arc::new(FakeRunner {
            script: entries
                .iter()
                .map(|(name, outcomes)| ((*name).to_owned(), outcomes.to_vec()))
                .collect(),
            panic: false,
        })
    }

    /// A runner that panics on every attempt.
    ///
    /// # Returns
    ///
    /// The runner.
    pub fn panicking() -> Arc<dyn EvalRunner> {
        Arc::new(FakeRunner {
            script: HashMap::new(),
            panic: true,
        })
    }
}

impl EvalRunner for FakeRunner {
    fn label(&self) -> String {
        "fake".to_owned()
    }

    fn run_attempt<'a>(&'a self, attempt: &'a AttemptContext<'a>) -> AttemptFuture<'a> {
        Box::pin(async move {
            assert!(!self.panic, "scripted panic");
            let succeed = self
                .script
                .get(attempt.task.name.as_str())
                .and_then(|outcomes| outcomes.get(attempt.index as usize - 1))
                .copied()
                .unwrap_or(false);
            if succeed {
                std::fs::write(
                    attempt.work_dir.join("DONE"),
                    "done
",
                )
                .map_err(|e| EvalError::io("writing", attempt.work_dir, e))?;
                std::fs::write(
                    attempt.work_dir.join("notes.md"),
                    "a marker here
",
                )
                .map_err(|e| EvalError::io("writing", attempt.work_dir, e))?;
            }
            Ok(AttemptOutcome {
                status: if succeed {
                    AttemptStatus::Done
                } else {
                    AttemptStatus::Incomplete {
                        reason: "scripted".to_owned(),
                    }
                },
                session: Some("01ARZ3NDEKTSV4RRFFQ69G5FAV".to_owned()),
                provider: Some("fake".to_owned()),
                model: Some("fake-1".to_owned()),
                tokens: Usage {
                    input: 1000,
                    output: 100,
                    cached: 0,
                },
                cost_usd: Some(0.0012),
                anvil_ms: 10,
                stderr_tail: "done".to_owned(),
            })
        })
    }
}

/// One finished attempt, reported as the run goes.
#[derive(Debug, Clone, PartialEq)]
pub struct Progress {
    /// Attempts finished so far, this one included.
    pub finished: u32,
    /// Attempts in the run.
    pub total: u32,
    /// The task.
    pub task: String,
    /// The attempt.
    pub record: AttemptRecord,
}

/// Called after every attempt.
pub type ProgressFn = Arc<dyn Fn(&Progress) + Send + Sync>;

/// What to run.
#[derive(Clone)]
pub struct RunOptions {
    /// The repository under test; its sha goes into the result.
    pub repo_root: PathBuf,
    /// The folder of task folders.
    pub tasks_dir: PathBuf,
    /// Keep only tasks with one of these tags; empty keeps all.
    pub tags: Vec<String>,
    /// Attempts per task; at least 1.
    pub repeat: u32,
    /// Attempts at once; at least 1.
    pub parallel: u32,
    /// The provider requested, recorded in the result.
    pub provider: Option<String>,
    /// Called after every attempt.
    pub progress: Option<ProgressFn>,
}

impl std::fmt::Debug for RunOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunOptions")
            .field("repo_root", &self.repo_root)
            .field("tasks_dir", &self.tasks_dir)
            .field("tags", &self.tags)
            .field("repeat", &self.repeat)
            .field("parallel", &self.parallel)
            .field("provider", &self.provider)
            .field("progress", &self.progress.is_some())
            .finish()
    }
}

/// What every attempt of a run shares.
struct Shared {
    run_root: tempfile::TempDir,
    bash: PathBuf,
    python: PathBuf,
    runner: Arc<dyn EvalRunner>,
}

impl Shared {
    /// The environment for one attempt's run and check.
    ///
    /// Every attempt gets its own cargo target folder: fixture copies share
    /// a package name, and cargo would hand a later copy the binary built
    /// from an earlier one.
    fn env(&self, task: &Task, index: u32) -> Vec<(String, String)> {
        let target = self
            .run_root
            .path()
            .join("cargo-target")
            .join(format!("{}-{index}", task.name));
        vec![
            ("EVAL_TASK_DIR".to_owned(), forward_slashes(&task.dir)),
            ("EVAL_PYTHON".to_owned(), forward_slashes(&self.python)),
            ("CARGO_TARGET_DIR".to_owned(), forward_slashes(&target)),
        ]
    }
}

/// Runs every matching task with `runner` and collects the result.
///
/// # Arguments
///
/// * `options` - What to run.
/// * `runner` - What works on each fixture copy.
///
/// # Returns
///
/// The result, with tasks sorted by name and attempts by index.
///
/// # Errors
///
/// When an option is zero, no task matches, a task is invalid, or bash or
/// Python cannot be found. Failures inside one attempt are recorded as
/// [`AttemptStatus::HarnessError`] and never fail the run.
pub async fn run_with(
    options: &RunOptions,
    runner: Arc<dyn EvalRunner>,
) -> Result<RunResult, EvalError> {
    if options.repeat == 0 {
        return Err(EvalError::ZeroOption { option: "repeat" });
    }
    if options.parallel == 0 {
        return Err(EvalError::ZeroOption { option: "parallel" });
    }
    let tasks = load_tasks(&options.tasks_dir, &options.tags)?;
    let run_root = tempfile::Builder::new()
        .prefix("anvil-eval-")
        .tempdir()
        .map_err(|source| EvalError::io("creating", &std::env::temp_dir(), source))?;
    let shared = Arc::new(Shared {
        run_root,
        bash: check::find_bash()?,
        python: check::find_python()?,
        runner,
    });
    let started_at = Utc::now();
    let clock = Instant::now();
    let git_sha = match git::short_sha(&options.repo_root).await {
        Ok(sha) => sha,
        Err(error) => {
            tracing::warn!(%error, "no git sha for the repository under test");
            "unknown".to_owned()
        }
    };

    let records = fan_out(options, &shared, &tasks).await;
    let task_results: Vec<TaskResult> = tasks
        .iter()
        .map(|task| TaskResult {
            name: task.name.clone(),
            tags: task.meta.tags.clone(),
            attempts: records.get(&task.name).cloned().unwrap_or_default(),
        })
        .collect();
    let model = task_results
        .iter()
        .flat_map(|t| &t.attempts)
        .find_map(|a| a.model.clone());
    let totals = Totals::compute(
        &task_results,
        options.repeat,
        clock.elapsed().as_millis() as u64,
    );
    Ok(RunResult {
        schema: SCHEMA_VERSION,
        started_at,
        finished_at: Utc::now(),
        git_sha,
        provider: options.provider.clone(),
        model,
        runner: shared.runner.label(),
        repeat: options.repeat,
        parallel: options.parallel,
        tags: options.tags.clone(),
        tasks: task_results,
        totals,
    })
}

/// Spawns every attempt, bounded by a semaphore, and collects the records
/// by task name with attempts sorted by index.
async fn fan_out(
    options: &RunOptions,
    shared: &Arc<Shared>,
    tasks: &[Task],
) -> BTreeMap<String, Vec<AttemptRecord>> {
    let semaphore = Arc::new(Semaphore::new(options.parallel as usize));
    let mut set = JoinSet::new();
    let mut spawned = HashMap::new();
    for task in tasks {
        for index in 1..=options.repeat {
            let handle = set.spawn(one_attempt(
                Arc::clone(shared),
                task.clone(),
                index,
                Arc::clone(&semaphore),
            ));
            spawned.insert(handle.id(), (task.name.clone(), index));
        }
    }
    let total = spawned.len() as u32;
    let mut finished = 0;
    let mut records: BTreeMap<String, Vec<AttemptRecord>> = BTreeMap::new();
    while let Some(joined) = set.join_next_with_id().await {
        let (task, record) = match joined {
            Ok((_, finished_attempt)) => finished_attempt,
            Err(error) => {
                let (task, index) = spawned
                    .get(&error.id())
                    .cloned()
                    .unwrap_or_else(|| ("unknown".to_owned(), 0));
                let outcome =
                    AttemptOutcome::harness_error(format!("attempt task failed: {error}"));
                let check = CheckResult::not_run("not run: the attempt task failed".to_owned());
                (task, record_of(index, outcome, check, 0))
            }
        };
        finished += 1;
        if let Some(progress) = &options.progress {
            progress(&Progress {
                finished,
                total,
                task: task.clone(),
                record: record.clone(),
            });
        }
        records.entry(task).or_default().push(record);
    }
    for attempts in records.values_mut() {
        attempts.sort_by_key(|a| a.index);
    }
    records
}

/// Copies the fixture, runs the runner, then always runs `check.sh`.
async fn one_attempt(
    shared: Arc<Shared>,
    task: Task,
    index: u32,
    semaphore: Arc<Semaphore>,
) -> (String, AttemptRecord) {
    let permit = semaphore.acquire_owned().await;
    let clock = Instant::now();
    let work_dir = shared
        .run_root
        .path()
        .join(format!("{}-{index}", task.name));
    let env = shared.env(&task, index);
    let outcome = match prepare(&task, &work_dir).await {
        Ok(()) => {
            let attempt = AttemptContext {
                task: &task,
                work_dir: &work_dir,
                index,
                env: &env,
            };
            shared
                .runner
                .run_attempt(&attempt)
                .await
                .unwrap_or_else(|error| AttemptOutcome::harness_error(error.to_string()))
        }
        Err(error) => AttemptOutcome::harness_error(error.to_string()),
    };
    let check = check::run_check(
        &shared.bash,
        &task.check_script(),
        &work_dir,
        task.timeout(),
        &env,
    )
    .await
    .unwrap_or_else(|error| CheckResult::not_run(error.to_string()));
    drop(permit);
    let wall_ms = clock.elapsed().as_millis() as u64;
    (task.name, record_of(index, outcome, check, wall_ms))
}

/// Copies the fixture into `work_dir` and gives it a git history.
async fn prepare(task: &Task, work_dir: &Path) -> Result<(), EvalError> {
    copy_fixture(task, work_dir)?;
    git::init_fixture_repo(work_dir).await
}

/// Joins a runner's outcome and the check into one record.
fn record_of(
    index: u32,
    outcome: AttemptOutcome,
    check: CheckResult,
    wall_ms: u64,
) -> AttemptRecord {
    AttemptRecord {
        index,
        passed: check.passed,
        status: outcome.status,
        check_exit: check.exit_code,
        session: outcome.session,
        provider: outcome.provider,
        model: outcome.model,
        tokens: outcome.tokens,
        cost_usd: outcome.cost_usd,
        anvil_ms: outcome.anvil_ms,
        check_ms: check.check_ms,
        wall_ms,
        stderr_tail: outcome.stderr_tail,
        check_tail: check.output_tail,
    }
}
