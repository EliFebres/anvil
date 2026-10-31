//! `eval baseline`: score the coder role and commit the result.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anvil_types::FactoryConfig;
use anvil_types::factory_config::FACTORY_FILE;

use crate::anvil::{AnvilOptions, AnvilRunner};
use crate::error::EvalError;
use crate::git;
use crate::results::RESULTS_DIR;
use crate::runner::{ProgressFn, RunOptions, run_with};
use crate::task::TASKS_DIR;

/// Attempts per task in a baseline; single runs lie.
pub const BASELINE_REPEAT: u32 = 3;

/// What a baseline needs.
pub struct BaselineOptions {
    /// The repository; `factory.toml`, `eval/tasks` and `eval/results` are under it.
    pub repo_root: PathBuf,
    /// The anvil to drive.
    pub anvil: AnvilOptions,
    /// Attempts at once; `None` takes `[eval] parallel` from `factory.toml`.
    pub parallel: Option<u32>,
    /// Called after every attempt.
    pub progress: Option<ProgressFn>,
}

/// Reads and validates `<repo_root>/factory.toml`.
///
/// # Arguments
///
/// * `repo_root` - The repository.
///
/// # Returns
///
/// The configuration.
///
/// # Errors
///
/// When the file is missing, does not parse or fails validation.
pub fn read_factory(repo_root: &Path) -> Result<FactoryConfig, EvalError> {
    let path = repo_root.join(FACTORY_FILE);
    let text =
        std::fs::read_to_string(&path).map_err(|source| EvalError::io("reading", &path, source))?;
    let config: FactoryConfig =
        toml::from_str(&text).map_err(|source| EvalError::toml(&path, source))?;
    config.validate()?;
    Ok(config)
}

/// Runs every task three times on the `coder` role, writes the result under
/// `eval/results/` and commits that one file.
///
/// # Arguments
///
/// * `options` - The repository and the anvil to drive.
///
/// # Returns
///
/// The result file committed.
///
/// # Errors
///
/// When `factory.toml` is invalid, the run cannot start, or git refuses the
/// commit.
pub async fn baseline(options: BaselineOptions) -> Result<PathBuf, EvalError> {
    let factory = read_factory(&options.repo_root)?;
    let coder = factory.roles.coder.clone();
    let runner = Arc::new(AnvilRunner::new(options.anvil, Some(coder.clone()))?);
    let run_options = RunOptions {
        repo_root: options.repo_root.clone(),
        tasks_dir: options.repo_root.join(TASKS_DIR),
        tags: Vec::new(),
        repeat: BASELINE_REPEAT,
        parallel: options.parallel.unwrap_or(factory.eval.parallel),
        provider: Some(coder),
        progress: options.progress,
    };
    let result = run_with(&run_options, runner).await?;
    let path = result.write(&options.repo_root.join(RESULTS_DIR))?;
    let message = format!(
        "Record the eval baseline at {} on {}: {}/{} tasks passing",
        result.git_sha,
        result.model.as_deref().unwrap_or("unknown model"),
        result.totals.tasks_passing,
        result.totals.tasks
    );
    git::commit_path(&options.repo_root, &path, &message).await?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_factory_validates() {
        let dir = tempfile::tempdir().expect("tmp");
        assert!(matches!(
            read_factory(dir.path()),
            Err(EvalError::Io {
                action: "reading",
                ..
            })
        ));
        std::fs::write(
            dir.path().join(FACTORY_FILE),
            FactoryConfig::template("owner/repo"),
        )
        .expect("write");
        let config = read_factory(dir.path()).expect("reads");
        assert_eq!(config.roles.coder, "anthropic");
        std::fs::write(
            dir.path().join(FACTORY_FILE),
            FactoryConfig::template("<owner>/<repo>"),
        )
        .expect("write");
        assert!(matches!(
            read_factory(dir.path()),
            Err(EvalError::FactoryConfig(_))
        ));
    }
}
