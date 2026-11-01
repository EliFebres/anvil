//! Fakes for every port, for this crate's tests and factory-cli's.
//!
//! Always compiled: they are small, pull in nothing, and the service's end
//! to end test in factory-cli needs them.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use anvil_types::Ticket;

use crate::error::{FactoryError, Result};
use crate::git::Git;
use crate::notify::{Event, Notifier};
use crate::ports::{
    BoxFuture, CheckOutcome, CiStatus, CodeHost, EvalGate, EvalGateReport, GateB, Implementer,
    ImplementerOutcome, ImplementerStatus, InlineComment, Outcome, Progress, PullRequest,
    ReviewInput, ReviewReport, ReviewSwarm, Tokens, Worker,
};
use crate::tickets::TicketQueue;

/// Locks a mutex, recovering from a poisoned one.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Keeps every event it is given.
#[derive(Debug, Default)]
pub struct RecordingNotifier {
    events: Mutex<Vec<Event>>,
}

impl RecordingNotifier {
    /// The events so far, oldest first.
    pub fn events(&self) -> Vec<Event> {
        lock(&self.events).clone()
    }
}

impl Notifier for RecordingNotifier {
    fn send<'a>(&'a self, event: &'a Event) -> BoxFuture<'a, Result<()>> {
        lock(&self.events).push(event.clone());
        Box::pin(std::future::ready(Ok(())))
    }
}

/// A pull request a fake host recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedPull {
    /// The number.
    pub number: u64,
    /// The head branch.
    pub branch: String,
    /// The title.
    pub title: String,
    /// The body.
    pub body: String,
    /// Labels added.
    pub labels: Vec<String>,
    /// Review bodies posted, with their inline comments.
    pub reviews: Vec<(String, Vec<InlineComment>)>,
    /// Whether it was squash-merged.
    pub merged: bool,
    /// The closing comment, when closed.
    pub closed: Option<String>,
}

/// A code host that records everything and needs no git; `main_ci` and the
/// pull request CI are scripted.
#[derive(Debug)]
pub struct FakeHost {
    main_ci: Mutex<CiStatus>,
    pull_ci: Mutex<CiStatus>,
    pulls: Mutex<Vec<RecordedPull>>,
    deleted: Mutex<Vec<String>>,
}

impl FakeHost {
    /// A host whose `main` and pull requests are green.
    pub fn green() -> Self {
        Self {
            main_ci: Mutex::new(CiStatus::Green),
            pull_ci: Mutex::new(CiStatus::Green),
            pulls: Mutex::new(Vec::new()),
            deleted: Mutex::new(Vec::new()),
        }
    }

    /// Changes what `main_ci` answers.
    pub fn set_main_ci(&self, status: CiStatus) {
        *lock(&self.main_ci) = status;
    }

    /// Changes what `wait_for_ci` answers.
    pub fn set_pull_ci(&self, status: CiStatus) {
        *lock(&self.pull_ci) = status;
    }

    /// The pull requests opened so far.
    pub fn pulls(&self) -> Vec<RecordedPull> {
        lock(&self.pulls).clone()
    }

    /// Branches deleted so far.
    pub fn deleted_branches(&self) -> Vec<String> {
        lock(&self.deleted).clone()
    }

    fn with_pull<T>(&self, number: u64, f: impl FnOnce(&mut RecordedPull) -> T) -> Result<T> {
        let mut pulls = lock(&self.pulls);
        let pull = pulls
            .iter_mut()
            .find(|p| p.number == number)
            .ok_or_else(|| FactoryError::Host {
                status: 404,
                message: format!("no pull request #{number}"),
            })?;
        Ok(f(pull))
    }
}

impl CodeHost for FakeHost {
    fn main_ci(&self) -> BoxFuture<'_, Result<CiStatus>> {
        let status = lock(&self.main_ci).clone();
        Box::pin(std::future::ready(Ok(status)))
    }

    fn open_pull<'a>(
        &'a self,
        head_branch: &'a str,
        title: &'a str,
        body: &'a str,
    ) -> BoxFuture<'a, Result<PullRequest>> {
        let mut pulls = lock(&self.pulls);
        let number = pulls.len() as u64 + 1;
        pulls.push(RecordedPull {
            number,
            branch: head_branch.to_owned(),
            title: title.to_owned(),
            body: body.to_owned(),
            labels: Vec::new(),
            reviews: Vec::new(),
            merged: false,
            closed: None,
        });
        Box::pin(std::future::ready(Ok(PullRequest {
            number,
            url: format!("fake://pull/{number}"),
            head_sha: format!("sha-{head_branch}"),
        })))
    }

    fn post_review<'a>(
        &'a self,
        number: u64,
        _head_sha: &'a str,
        body: &'a str,
        comments: &'a [InlineComment],
    ) -> BoxFuture<'a, Result<()>> {
        let result = self.with_pull(number, |pull| {
            pull.reviews.push((body.to_owned(), comments.to_vec()));
        });
        Box::pin(std::future::ready(result))
    }

    fn add_label<'a>(&'a self, number: u64, label: &'a str) -> BoxFuture<'a, Result<()>> {
        let result = self.with_pull(number, |pull| pull.labels.push(label.to_owned()));
        Box::pin(std::future::ready(result))
    }

    fn wait_for_ci<'a>(
        &'a self,
        _head_sha: &'a str,
        _deadline: Instant,
    ) -> BoxFuture<'a, Result<CiStatus>> {
        let status = lock(&self.pull_ci).clone();
        Box::pin(std::future::ready(Ok(status)))
    }

    fn squash_merge<'a>(&'a self, number: u64, _title: &'a str) -> BoxFuture<'a, Result<()>> {
        let result = self.with_pull(number, |pull| pull.merged = true);
        Box::pin(std::future::ready(result))
    }

    fn delete_branch<'a>(&'a self, branch: &'a str) -> BoxFuture<'a, Result<()>> {
        lock(&self.deleted).push(branch.to_owned());
        Box::pin(std::future::ready(Ok(())))
    }

    fn close_pull<'a>(&'a self, number: u64, comment: &'a str) -> BoxFuture<'a, Result<()>> {
        let result = self.with_pull(number, |pull| pull.closed = Some(comment.to_owned()));
        Box::pin(std::future::ready(result))
    }
}

/// A code host over a bare repository standing in for GitHub: pull requests
/// are recorded, a squash merge really squashes the branch onto `main`, and
/// deleting a branch really deletes it.
#[derive(Debug)]
pub struct LocalHost {
    bare: PathBuf,
    scratch: PathBuf,
    inner: FakeHost,
}

impl LocalHost {
    /// A host over a bare repository.
    ///
    /// # Arguments
    ///
    /// * `bare` - The bare repository.
    /// * `scratch` - A folder for the clones a merge needs.
    pub fn new(bare: &Path, scratch: &Path) -> Self {
        Self {
            bare: bare.to_path_buf(),
            scratch: scratch.to_path_buf(),
            inner: FakeHost::green(),
        }
    }

    /// The recording host underneath, for assertions and scripting.
    pub fn recorder(&self) -> &FakeHost {
        &self.inner
    }

    async fn squash(&self, number: u64, title: &str) -> Result<()> {
        let branch = self.inner.with_pull(number, |pull| pull.branch.clone())?;
        let clone = self.scratch.join(format!("merge-{number}"));
        let _ = std::fs::remove_dir_all(&clone);
        std::fs::create_dir_all(&self.scratch)
            .map_err(|source| FactoryError::io("creating", &self.scratch, source))?;
        Git::new(&self.scratch)
            .run(&[
                "clone",
                "-q",
                "--branch",
                "main",
                &self.bare.to_string_lossy(),
                &clone.to_string_lossy(),
            ])
            .await?;
        let git = Git::new(&clone);
        git.run(&["merge", "--squash", &format!("origin/{branch}")])
            .await?;
        git.run(&["commit", "-q", "-m", title]).await?;
        git.push("origin", "main").await?;
        let _ = std::fs::remove_dir_all(&clone);
        self.inner.with_pull(number, |pull| pull.merged = true)
    }
}

impl CodeHost for LocalHost {
    fn main_ci(&self) -> BoxFuture<'_, Result<CiStatus>> {
        self.inner.main_ci()
    }

    fn open_pull<'a>(
        &'a self,
        head_branch: &'a str,
        title: &'a str,
        body: &'a str,
    ) -> BoxFuture<'a, Result<PullRequest>> {
        Box::pin(async move {
            let mut pull = self.inner.open_pull(head_branch, title, body).await?;
            pull.head_sha = Git::new(&self.bare).rev_parse(head_branch).await?;
            Ok(pull)
        })
    }

    fn post_review<'a>(
        &'a self,
        number: u64,
        head_sha: &'a str,
        body: &'a str,
        comments: &'a [InlineComment],
    ) -> BoxFuture<'a, Result<()>> {
        self.inner.post_review(number, head_sha, body, comments)
    }

    fn add_label<'a>(&'a self, number: u64, label: &'a str) -> BoxFuture<'a, Result<()>> {
        self.inner.add_label(number, label)
    }

    fn wait_for_ci<'a>(
        &'a self,
        head_sha: &'a str,
        deadline: Instant,
    ) -> BoxFuture<'a, Result<CiStatus>> {
        self.inner.wait_for_ci(head_sha, deadline)
    }

    fn squash_merge<'a>(&'a self, number: u64, title: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(self.squash(number, title))
    }

    fn delete_branch<'a>(&'a self, branch: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            Git::new(&self.bare)
                .run(&["update-ref", "-d", &format!("refs/heads/{branch}")])
                .await?;
            self.inner.delete_branch(branch).await
        })
    }

    fn close_pull<'a>(&'a self, number: u64, comment: &'a str) -> BoxFuture<'a, Result<()>> {
        self.inner.close_pull(number, comment)
    }
}

/// A worker that answers from a script and counts its calls.
///
/// Each call pops the next entry; when the script is empty it answers
/// `Merged { pr: 1 }`. An `Err` entry becomes a [`FactoryError::Sandbox`].
#[derive(Debug, Default)]
pub struct ScriptedWorker {
    script: Mutex<VecDeque<std::result::Result<Outcome, String>>>,
    calls: AtomicUsize,
    stages: Mutex<Vec<String>>,
}

impl ScriptedWorker {
    /// A worker that answers these outcomes in order.
    ///
    /// # Arguments
    ///
    /// * `script` - `Ok(outcome)` or `Err(message)` per call.
    pub fn scripted(script: Vec<std::result::Result<Outcome, String>>) -> Self {
        Self {
            script: Mutex::new(script.into()),
            calls: AtomicUsize::new(0),
            stages: Mutex::new(Vec::new()),
        }
    }

    /// How many tickets were handed to this worker.
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// Every stage reported, oldest first.
    pub fn stages(&self) -> Vec<String> {
        lock(&self.stages).clone()
    }
}

impl Worker for ScriptedWorker {
    fn work<'a>(
        &'a self,
        _ticket: &'a Ticket,
        _queue: &'a TicketQueue,
        progress: Progress,
    ) -> BoxFuture<'a, Result<Outcome>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        progress("scripted");
        lock(&self.stages).push("scripted".to_owned());
        let next = lock(&self.script).pop_front();
        let result = match next {
            Some(Ok(outcome)) => Ok(outcome),
            Some(Err(message)) => Err(FactoryError::Sandbox(message)),
            None => Ok(Outcome::Merged {
                pr: 1,
                cost_usd: None,
            }),
        };
        Box::pin(std::future::ready(result))
    }
}

/// One scripted implementer run: what it writes into the worktree and what it reports.
#[derive(Debug, Clone)]
pub struct ScriptedRun {
    /// Files written, repository-relative, before the outcome is reported.
    pub writes: Vec<(String, String)>,
    /// The outcome.
    pub outcome: ImplementerOutcome,
}

impl ScriptedRun {
    /// A run that finishes, writes these files and used `tokens` tokens.
    ///
    /// # Arguments
    ///
    /// * `writes` - `(path, content)` pairs.
    /// * `tokens` - Input tokens used; cost is a cent per thousand.
    pub fn done(writes: &[(&str, &str)], tokens: u64) -> Self {
        Self {
            writes: writes
                .iter()
                .map(|(p, c)| ((*p).to_owned(), (*c).to_owned()))
                .collect(),
            outcome: ImplementerOutcome {
                status: ImplementerStatus::Done,
                tokens: Tokens {
                    input: tokens,
                    output: 0,
                    cached: 0,
                },
                cost_usd: Some(tokens as f64 / 100_000.0),
                output_tail: "done".to_owned(),
            },
        }
    }

    /// A run that writes a skill file: the default happy path.
    pub fn default_edit() -> Self {
        Self::done(
            &[(
                "skills/hello/SKILL.md",
                "---\nname: hello\n---\n\nSay hello.\n",
            )],
            1000,
        )
    }
}

/// An implementer that follows a script and never runs anything.
#[derive(Debug, Default)]
pub struct ScriptedImplementer {
    runs: Mutex<VecDeque<ScriptedRun>>,
    checks: Mutex<VecDeque<bool>>,
    builds: Mutex<VecDeque<bool>>,
    denied: Vec<String>,
    tasks: Mutex<Vec<String>>,
    run_count: AtomicUsize,
    check_count: AtomicUsize,
    prepared: AtomicUsize,
}

impl ScriptedImplementer {
    /// Sets the runs; an exhausted script repeats [`ScriptedRun::default_edit`].
    pub fn with_runs(mut self, runs: Vec<ScriptedRun>) -> Self {
        self.runs = Mutex::new(runs.into());
        self
    }

    /// Sets the Gate A results; an exhausted script passes.
    pub fn with_checks(mut self, checks: Vec<bool>) -> Self {
        self.checks = Mutex::new(checks.into());
        self
    }

    /// Sets the release build results; an exhausted script passes.
    pub fn with_builds(mut self, builds: Vec<bool>) -> Self {
        self.builds = Mutex::new(builds.into());
        self
    }

    /// Sets what `denied_hosts` reports.
    pub fn with_denied(mut self, denied: Vec<String>) -> Self {
        self.denied = denied;
        self
    }

    /// Every task text received, oldest first.
    pub fn tasks(&self) -> Vec<String> {
        lock(&self.tasks).clone()
    }

    /// How many runs happened.
    pub fn runs(&self) -> usize {
        self.run_count.load(Ordering::SeqCst)
    }

    /// How many Gate A checks happened.
    pub fn checks(&self) -> usize {
        self.check_count.load(Ordering::SeqCst)
    }

    /// How many times `prepare` was called.
    pub fn prepared(&self) -> usize {
        self.prepared.load(Ordering::SeqCst)
    }
}

impl Implementer for ScriptedImplementer {
    fn prepare<'a>(&'a self, _worktree: &'a Path) -> BoxFuture<'a, Result<()>> {
        self.prepared.fetch_add(1, Ordering::SeqCst);
        Box::pin(std::future::ready(Ok(())))
    }

    fn run<'a>(
        &'a self,
        worktree: &'a Path,
        task: &'a str,
        _deadline: Instant,
    ) -> BoxFuture<'a, Result<ImplementerOutcome>> {
        self.run_count.fetch_add(1, Ordering::SeqCst);
        lock(&self.tasks).push(task.to_owned());
        let run = lock(&self.runs)
            .pop_front()
            .unwrap_or_else(ScriptedRun::default_edit);
        let result = (|| {
            for (path, content) in &run.writes {
                let full = worktree.join(path);
                if let Some(dir) = full.parent() {
                    std::fs::create_dir_all(dir)
                        .map_err(|source| FactoryError::io("creating", dir, source))?;
                }
                std::fs::write(&full, content)
                    .map_err(|source| FactoryError::io("writing", &full, source))?;
            }
            Ok(run.outcome)
        })();
        Box::pin(std::future::ready(result))
    }

    fn check<'a>(
        &'a self,
        _worktree: &'a Path,
        _deadline: Instant,
    ) -> BoxFuture<'a, Result<CheckOutcome>> {
        self.check_count.fetch_add(1, Ordering::SeqCst);
        let passed = lock(&self.checks).pop_front().unwrap_or(true);
        Box::pin(std::future::ready(Ok(CheckOutcome {
            passed,
            output_tail: if passed {
                "all checks passed".to_owned()
            } else {
                "error[E0425]: cannot find value `x` in this scope".to_owned()
            },
        })))
    }

    fn build_release<'a>(
        &'a self,
        _worktree: &'a Path,
        _deadline: Instant,
    ) -> BoxFuture<'a, Result<CheckOutcome>> {
        let passed = lock(&self.builds).pop_front().unwrap_or(true);
        Box::pin(std::future::ready(Ok(CheckOutcome {
            passed,
            output_tail: "Finished `release` profile".to_owned(),
        })))
    }

    fn denied_hosts(&self) -> Vec<String> {
        self.denied.clone()
    }
}

/// An eval gate that answers from a script; an exhausted script passes with
/// no regressions.
#[derive(Debug, Default)]
pub struct FakeEvalGate {
    script: Mutex<VecDeque<GateB>>,
    calls: AtomicUsize,
}

impl FakeEvalGate {
    /// A gate that answers these in order.
    pub fn scripted(script: Vec<GateB>) -> Self {
        Self {
            script: Mutex::new(script.into()),
            calls: AtomicUsize::new(0),
        }
    }

    /// How many times the gate ran.
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// A passing report with these regressions.
    pub fn report(regressions: &[&str]) -> EvalGateReport {
        EvalGateReport {
            regressions: regressions.iter().map(|r| (*r).to_owned()).collect(),
            summary: format!("```\ngate: {} regression(s)\n```", regressions.len()),
            cost_usd: Some(0.5),
        }
    }
}

impl EvalGate for FakeEvalGate {
    fn run<'a>(&'a self, _worktree: &'a Path) -> BoxFuture<'a, Result<GateB>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let next = lock(&self.script)
            .pop_front()
            .unwrap_or_else(|| GateB::Passed(Self::report(&[])));
        Box::pin(std::future::ready(Ok(next)))
    }
}

/// A review swarm that answers from a script; an exhausted script approves.
#[derive(Debug, Default)]
pub struct FakeSwarm {
    script: Mutex<VecDeque<ReviewReport>>,
    inputs: Mutex<Vec<ReviewInput>>,
}

impl FakeSwarm {
    /// A swarm that answers these in order.
    pub fn scripted(script: Vec<ReviewReport>) -> Self {
        Self {
            script: Mutex::new(script.into()),
            inputs: Mutex::new(Vec::new()),
        }
    }

    /// Every review request received.
    pub fn inputs(&self) -> Vec<ReviewInput> {
        lock(&self.inputs).clone()
    }

    /// An approving report from two reviewers.
    pub fn approved() -> ReviewReport {
        ReviewReport {
            approved: true,
            reason: "no blocking findings".to_owned(),
            findings_markdown: String::new(),
            verdicts: vec![
                ("openai/correctness".to_owned(), "approve".to_owned()),
                ("google/safety".to_owned(), "approve".to_owned()),
            ],
            minors: Vec::new(),
            cost_usd: Some(0.25),
        }
    }

    /// A `changes` report with one blocker.
    pub fn changes(finding: &str) -> ReviewReport {
        ReviewReport {
            approved: false,
            reason: "1 blocker".to_owned(),
            findings_markdown: format!("- **blocker** (openai/correctness): {finding}"),
            verdicts: vec![
                ("openai/correctness".to_owned(), "changes".to_owned()),
                ("google/safety".to_owned(), "approve".to_owned()),
            ],
            minors: Vec::new(),
            cost_usd: Some(0.25),
        }
    }
}

impl ReviewSwarm for FakeSwarm {
    fn review<'a>(&'a self, input: &'a ReviewInput) -> BoxFuture<'a, Result<ReviewReport>> {
        lock(&self.inputs).push(input.clone());
        let next = lock(&self.script)
            .pop_front()
            .unwrap_or_else(Self::approved);
        Box::pin(std::future::ready(Ok(next)))
    }
}
