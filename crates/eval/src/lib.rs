//! Eval harness that scores a model or a build against a fixed task set.
//!
//! A task is a folder under `eval/tasks/<name>/` with a `fixture/` repository,
//! a `task.md` prompt, a `check.sh` that exits 0 on success and a `meta.toml`.
//! The harness copies each fixture to a temp folder, runs `anvil --yes run`
//! on it, runs the check, and records pass/fail, tokens, wall time and cost.
//! Results are JSON files under `eval/results/`; [`compare`] prints two side
//! by side and [`gate`] applies the factory's regression rule.
//!
//! The factory calls [`run`] in-process, or [`run_with`] with its own
//! [`EvalRunner`] in tests. Depends on anvil-types only and never on
//! anvil-core or factory-core.

pub mod anvil;
pub mod baseline;
pub mod check;
pub mod compare;
pub mod error;
pub mod gate;
pub mod git;
pub mod results;
pub mod runner;
pub mod task;
pub mod telemetry;

use std::sync::Arc;

pub use anvil::{AnvilOptions, AnvilRunner, default_anvil_bin};
pub use baseline::{BASELINE_REPEAT, BaselineOptions, baseline, read_factory};
pub use compare::{Comparison, Flips, compare};
pub use error::EvalError;
pub use gate::{GateOutcome, MAX_REGRESSIONS, gate};
pub use results::{
    AttemptRecord, AttemptStatus, RESULTS_DIR, RunResult, SCHEMA_VERSION, TaskResult, Totals, Usage,
};
pub use runner::{
    AttemptContext, AttemptFuture, AttemptOutcome, EvalRunner, FakeRunner, Progress, ProgressFn,
    RunOptions, run_with,
};
pub use task::{TASKS_DIR, Task, TaskMeta, load_tasks};

/// Runs the task set with the real anvil.
///
/// # Arguments
///
/// * `options` - What to run; `options.provider` is forced on anvil.
/// * `anvil` - Which anvil to drive and whose home to seed.
///
/// # Returns
///
/// The result; write it with [`RunResult::write`].
///
/// # Errors
///
/// When the runner cannot be prepared or the run cannot start; see
/// [`AnvilRunner::new`] and [`run_with`].
pub async fn run(options: &RunOptions, anvil: AnvilOptions) -> Result<RunResult, EvalError> {
    let runner = AnvilRunner::new(anvil, options.provider.clone())?;
    run_with(options, Arc::new(runner)).await
}
