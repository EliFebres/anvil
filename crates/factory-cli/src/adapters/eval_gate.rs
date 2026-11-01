//! Gate B: the eval harness on the branch build, compared with the newest
//! result on `main`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use eval::{AnvilOptions, AnvilRunner, EvalRunner, RunOptions, RunResult};
use factory_core::{BoxFuture, EvalGate, EvalGateReport, FactoryError, GateB, Git, Sandbox};

/// Where results live in the repository.
pub const RESULTS_DIR: &str = "eval/results";

/// What runs the attempts.
pub enum RunnerSource {
    /// The branch's release anvil, through the sandbox.
    Sandbox(Arc<Sandbox>),
    /// A fixed runner; tests pass eval's `FakeRunner`.
    Fixed(Arc<dyn EvalRunner>),
}

/// The eval gate over the real harness.
pub struct HarnessGate {
    runner: RunnerSource,
    repo: Git,
    remote: String,
    tasks_dir: PathBuf,
    parallel: u32,
    coder: String,
}

impl HarnessGate {
    /// Builds the gate.
    ///
    /// # Arguments
    ///
    /// * `runner` - What runs the attempts.
    /// * `repo` - The factory's clone, where `origin/main` holds the baseline.
    /// * `remote` - The remote, normally `origin`.
    /// * `tasks_dir` - The task folders.
    /// * `parallel` - Attempts at once.
    /// * `coder` - The coder provider the attempts run with.
    pub fn new(
        runner: RunnerSource,
        repo: Git,
        remote: &str,
        tasks_dir: &Path,
        parallel: u32,
        coder: &str,
    ) -> Self {
        Self {
            runner,
            repo,
            remote: remote.to_owned(),
            tasks_dir: tasks_dir.to_path_buf(),
            parallel,
            coder: coder.to_owned(),
        }
    }

    /// The newest eval result committed on `main`, by `finished_at`.
    ///
    /// # Returns
    ///
    /// The result, or `None` when `main` has none.
    ///
    /// # Errors
    ///
    /// When git fails or a result file does not parse.
    pub async fn baseline(&self) -> factory_core::Result<Option<RunResult>> {
        let main = format!("{}/main", self.remote);
        let paths = match self.repo.ls_tree(&main, RESULTS_DIR).await {
            Ok(paths) => paths,
            Err(FactoryError::Git { .. }) => return Ok(None),
            Err(error) => return Err(error),
        };
        let mut newest: Option<RunResult> = None;
        for path in paths.iter().filter(|p| p.ends_with(".json")) {
            let text = self.repo.show(&main, path).await?;
            let result: RunResult = serde_json::from_str(&text).map_err(|error| {
                FactoryError::Config(format!(
                    "eval result {path} on main does not parse: {error}"
                ))
            })?;
            if newest
                .as_ref()
                .is_none_or(|current| result.finished_at > current.finished_at)
            {
                newest = Some(result);
            }
        }
        Ok(newest)
    }

    /// The runner for a worktree's build.
    fn runner_for(&self, worktree: &Path) -> factory_core::Result<Arc<dyn EvalRunner>> {
        match &self.runner {
            RunnerSource::Fixed(runner) => Ok(Arc::clone(runner)),
            RunnerSource::Sandbox(sandbox) => {
                let (anvil_bin, home) = sandbox.eval_anvil(worktree)?;
                let runner = AnvilRunner::new(
                    AnvilOptions {
                        anvil_bin,
                        real_home: Some(home),
                    },
                    Some(self.coder.clone()),
                )
                .map_err(|error| FactoryError::Sandbox(format!("eval runner: {error}")))?;
                Ok(Arc::new(runner))
            }
        }
    }

    async fn gate(&self, worktree: &Path) -> factory_core::Result<GateB> {
        let Some(base) = self.baseline().await? else {
            return Ok(GateB::NoBaseline);
        };
        let runner = self.runner_for(worktree)?;
        let options = RunOptions {
            repo_root: worktree.to_path_buf(),
            tasks_dir: self.tasks_dir.clone(),
            tags: Vec::new(),
            repeat: 1,
            parallel: self.parallel,
            provider: Some(self.coder.clone()),
            progress: None,
        };
        let candidate = eval::run_with(&options, runner)
            .await
            .map_err(|error| FactoryError::Sandbox(format!("eval run: {error}")))?;
        let outcome = eval::gate(&base, &candidate);
        let comparison = eval::compare(&base, &candidate);
        let report = EvalGateReport {
            regressions: outcome.regressions.clone(),
            summary: format!("```\n{comparison}{outcome}```"),
            cost_usd: candidate.totals.cost_usd,
        };
        Ok(if outcome.passed {
            GateB::Passed(report)
        } else {
            GateB::Failed(report)
        })
    }
}

impl EvalGate for HarnessGate {
    fn run<'a>(&'a self, worktree: &'a Path) -> BoxFuture<'a, factory_core::Result<GateB>> {
        Box::pin(self.gate(worktree))
    }
}
