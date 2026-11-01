//! `eval`: scores a model or an anvil build against the task set.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use anvil_types::factory_config::DEFAULT_EVAL_PARALLEL;
use clap::{Parser, Subcommand};
use eval::{
    AnvilOptions, BaselineOptions, EvalError, Progress, ProgressFn, RESULTS_DIR, RunOptions,
    RunResult, TASKS_DIR,
};

/// Scores a model or an anvil build against the fixed task set.
#[derive(Debug, Parser)]
#[command(name = "eval", version, about, arg_required_else_help = true)]
struct Cli {
    /// What to do.
    #[command(subcommand)]
    command: Command,
}

/// Top-level subcommands.
#[derive(Debug, Subcommand)]
enum Command {
    /// Run the tasks and write a result file under eval/results/.
    Run {
        /// Provider to force as the coder; default: the configured roles.coder.
        #[arg(long)]
        provider: Option<String>,
        /// The anvil binary; default: anvil on PATH, then target/release, then target/debug.
        #[arg(long)]
        anvil_bin: Option<PathBuf>,
        /// Keep only tasks with one of these tags (comma separated).
        #[arg(long, value_delimiter = ',')]
        tags: Vec<String>,
        /// Attempts per task; 3 gives a pass rate with its spread.
        #[arg(long, default_value_t = 1)]
        repeat: u32,
        /// Attempts at once; default: [eval] parallel in factory.toml.
        #[arg(long)]
        parallel: Option<u32>,
    },
    /// Print two result files side by side with the tasks that flipped.
    Compare {
        /// The first result file.
        a: PathBuf,
        /// The second result file.
        b: PathBuf,
    },
    /// Exit 2 when the candidate regresses on more than one task.
    Gate {
        /// The result on trunk.
        #[arg(long)]
        base: PathBuf,
        /// The result on the branch.
        #[arg(long)]
        candidate: PathBuf,
    },
    /// Run every task three times on the coder role and commit the result.
    Baseline {
        /// The anvil binary; default as for `run`.
        #[arg(long)]
        anvil_bin: Option<PathBuf>,
        /// Attempts at once; default: [eval] parallel in factory.toml.
        #[arg(long)]
        parallel: Option<u32>,
    },
}

/// Everything went as asked.
const EXIT_OK: u8 = 0;

/// Setup or runtime error.
const EXIT_ERROR: u8 = 1;

/// The gate refused the candidate.
const EXIT_GATE_FAILED: u8 = 2;

fn main() -> ExitCode {
    let cli = Cli::parse();
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("error: starting the async runtime: {error}");
            return ExitCode::from(EXIT_ERROR);
        }
    };
    match runtime.block_on(dispatch(cli)) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("error: {}", report(&error));
            ExitCode::from(EXIT_ERROR)
        }
    }
}

/// Runs one subcommand.
async fn dispatch(cli: Cli) -> Result<u8, EvalError> {
    match cli.command {
        Command::Run {
            provider,
            anvil_bin,
            tags,
            repeat,
            parallel,
        } => {
            let repo_root = repo_root().await?;
            let options = RunOptions {
                tasks_dir: repo_root.join(TASKS_DIR),
                tags,
                repeat,
                parallel: parallel.unwrap_or_else(|| configured_parallel(&repo_root)),
                provider,
                progress: Some(progress_printer()),
                repo_root: repo_root.clone(),
            };
            let anvil = anvil_options(&repo_root, anvil_bin)?;
            let result = eval::run(&options, anvil).await?;
            let path = result.write(&repo_root.join(RESULTS_DIR))?;
            print!("\n{result}");
            println!("wrote {}", path.display());
            Ok(EXIT_OK)
        }
        Command::Compare { a, b } => {
            let a = RunResult::read(&a)?;
            let b = RunResult::read(&b)?;
            print!("{}", eval::compare(&a, &b));
            Ok(EXIT_OK)
        }
        Command::Gate { base, candidate } => {
            let base = RunResult::read(&base)?;
            let candidate = RunResult::read(&candidate)?;
            let outcome = eval::gate(&base, &candidate);
            print!("{outcome}");
            Ok(if outcome.passed {
                EXIT_OK
            } else {
                EXIT_GATE_FAILED
            })
        }
        Command::Baseline {
            anvil_bin,
            parallel,
        } => {
            let repo_root = repo_root().await?;
            let path = eval::baseline(BaselineOptions {
                anvil: anvil_options(&repo_root, anvil_bin)?,
                repo_root,
                parallel,
                progress: Some(progress_printer()),
            })
            .await?;
            println!("committed {}", path.display());
            Ok(EXIT_OK)
        }
    }
}

/// The repository root of the current folder.
async fn repo_root() -> Result<PathBuf, EvalError> {
    let cwd = std::env::current_dir()
        .map_err(|source| EvalError::io("reading", Path::new("."), source))?;
    eval::git::toplevel(&cwd).await
}

/// `[eval] parallel` from `factory.toml`, or the default when the file is
/// absent or invalid.
fn configured_parallel(repo_root: &Path) -> u32 {
    eval::read_factory(repo_root)
        .map(|factory| factory.eval.parallel)
        .unwrap_or(DEFAULT_EVAL_PARALLEL)
}

/// The anvil binary and the real home to seed from.
fn anvil_options(repo_root: &Path, anvil_bin: Option<PathBuf>) -> Result<AnvilOptions, EvalError> {
    let anvil_bin = match anvil_bin {
        Some(path) => path,
        None => eval::default_anvil_bin(repo_root)?,
    };
    let real_home = std::env::var_os("ANVIL_HOME")
        .map(PathBuf::from)
        .or_else(std::env::home_dir);
    Ok(AnvilOptions {
        anvil_bin,
        real_home,
    })
}

/// Prints one line per finished attempt.
fn progress_printer() -> ProgressFn {
    Arc::new(|progress: &Progress| {
        let record = &progress.record;
        let verdict = if record.passed { "PASS" } else { "FAIL" };
        let cost = record
            .cost_usd
            .map_or_else(|| "n/a".to_owned(), |c| format!("${c:.2}"));
        eprintln!(
            "[{}/{}] {} #{} {verdict} {} {:.1}k tok {cost} ({})",
            progress.finished,
            progress.total,
            progress.task,
            record.index,
            seconds(record.wall_ms),
            record.tokens.total() as f64 / 1000.0,
            record.status.label()
        );
    })
}

/// `48.1s`.
fn seconds(ms: u64) -> String {
    format!("{:.1}s", ms as f64 / 1000.0)
}

/// The error and every cause, joined with `: `.
fn report(error: &EvalError) -> String {
    let mut text = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        let cause_text = cause.to_string();
        if !text.ends_with(&cause_text) {
            text.push_str(": ");
            text.push_str(&cause_text);
        }
        source = cause.source();
    }
    text
}
