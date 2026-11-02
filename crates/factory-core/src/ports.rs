//! The ports the service and the worker drive: the implementer, the eval
//! gate, the review swarm, the code host and the notifier.
//!
//! `factory-core` depends on `anvil-types` only, so the eval harness and the
//! review swarm are reached through these traits; `factory-cli` wires the
//! real implementations and the tests use the fakes in [`crate::fakes`].

use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use anvil_types::Ticket;

use crate::board::{ContainerView, HealthView, NeedsHuman, ProviderLatency, Stage, UsageView};
use crate::error::Result;
use crate::tickets::TicketQueue;

/// A boxed future, so ports can be trait objects.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Tokens a model run used.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tokens {
    /// Uncached input tokens.
    pub input: u64,
    /// Output tokens.
    pub output: u64,
    /// Cached input tokens.
    pub cached: u64,
}

impl Tokens {
    /// Every token counted.
    pub fn total(self) -> u64 {
        self.input + self.output + self.cached
    }

    /// Adds another count.
    pub fn add(&mut self, other: Tokens) {
        self.input += other.input;
        self.output += other.output;
        self.cached += other.cached;
    }
}

/// How an implementer run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImplementerStatus {
    /// anvil finished its task.
    Done,
    /// anvil stopped early; the reason it printed.
    Incomplete(String),
    /// anvil exited with an error.
    Failed {
        /// The exit code, when there was one.
        code: Option<i32>,
    },
    /// The deadline passed and the run was killed.
    Timeout,
}

impl ImplementerStatus {
    /// Whether anvil stopped because a state or the session used its budget.
    ///
    /// # Returns
    ///
    /// True for an incomplete run whose reason is anvil's
    /// `the <state> state used its token|turn budget` or
    /// `the session used its token budget`.
    pub fn is_budget_stop(&self) -> bool {
        let Self::Incomplete(reason) = self else {
            return false;
        };
        reason.contains("used its token budget") || reason.contains("used its turn budget")
    }

    /// The anvil state whose budget ran out, such as `plan`.
    ///
    /// # Returns
    ///
    /// The state's name when the run stopped on a per-state budget, `None`
    /// otherwise (the session budget included).
    pub fn budget_state(&self) -> Option<&str> {
        if !self.is_budget_stop() {
            return None;
        }
        let Self::Incomplete(reason) = self else {
            return None;
        };
        let (before, _) = reason.split_once(" state used its ")?;
        let (_, state) = before.rsplit_once("the ")?;
        (!state.is_empty() && !state.contains(' ')).then_some(state)
    }
}

/// What one implementer run produced.
#[derive(Debug, Clone, PartialEq)]
pub struct ImplementerOutcome {
    /// How it ended.
    pub status: ImplementerStatus,
    /// Tokens the run used.
    pub tokens: Tokens,
    /// Cost in USD, when every provider used has a price.
    pub cost_usd: Option<f64>,
    /// The last lines of output.
    pub output_tail: String,
}

/// What a command in the sandbox produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckOutcome {
    /// Whether it exited zero.
    pub passed: bool,
    /// The last lines of its output.
    pub output_tail: String,
}

/// Runs anvil and the build tools inside the sandbox.
pub trait Implementer: Send + Sync {
    /// Readies the sandbox for a ticket: warms caches, starts the proxy.
    fn prepare<'a>(&'a self, worktree: &'a Path) -> BoxFuture<'a, Result<()>>;

    /// `anvil --yes --profile factory "<task>"` in the worktree.
    fn run<'a>(
        &'a self,
        worktree: &'a Path,
        task: &'a str,
        deadline: Instant,
    ) -> BoxFuture<'a, Result<ImplementerOutcome>>;

    /// Gate A: `just check` in the worktree.
    fn check<'a>(
        &'a self,
        worktree: &'a Path,
        deadline: Instant,
    ) -> BoxFuture<'a, Result<CheckOutcome>>;

    /// Builds the release anvil binary the eval gate runs.
    fn build_release<'a>(
        &'a self,
        worktree: &'a Path,
        deadline: Instant,
    ) -> BoxFuture<'a, Result<CheckOutcome>>;

    /// Hosts the sandbox refused to reach so far, for the ticket notes.
    fn denied_hosts(&self) -> Vec<String>;
}

/// What the eval gate found.
#[derive(Debug, Clone, PartialEq)]
pub struct EvalGateReport {
    /// Tasks passing on `main` and failing on the branch.
    pub regressions: Vec<String>,
    /// The compare table and the gate line, as Markdown.
    pub summary: String,
    /// What the run cost, when known.
    pub cost_usd: Option<f64>,
}

/// Gate B's verdict.
#[derive(Debug, Clone, PartialEq)]
pub enum GateB {
    /// At most one task regressed.
    Passed(EvalGateReport),
    /// More than one task regressed.
    Failed(EvalGateReport),
    /// `main` has no eval result to compare with; a configuration error that pauses the factory.
    NoBaseline,
}

/// Runs the eval harness on a branch build and compares with `main`.
pub trait EvalGate: Send + Sync {
    /// Gate B on the worktree, whose release anvil binary is built.
    fn run<'a>(&'a self, worktree: &'a Path) -> BoxFuture<'a, Result<GateB>>;

    /// Whether the worker builds the release anvil before [`EvalGate::run`];
    /// only a gate that runs no eval (the selftest's) says no.
    fn needs_release_build(&self) -> bool {
        true
    }
}

/// What the review swarm reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewInput {
    /// The ticket being worked.
    pub ticket: Ticket,
    /// The unified diff against `main`.
    pub diff: String,
    /// The files the diff touches.
    pub changed_files: Vec<String>,
    /// The repository's `CLAUDE.md`, when it has one.
    pub guidelines: Option<String>,
}

/// A minor finding to post as a review comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MinorFinding {
    /// The file, when the reviewer named one.
    pub file: Option<String>,
    /// The line in the new version, when the reviewer named one.
    pub line: Option<u32>,
    /// Whether the diff shows that line, so an inline comment can sit on it.
    pub anchored: bool,
    /// The comment text.
    pub body: String,
}

impl MinorFinding {
    /// `file:line`, `file` or `-`.
    pub fn location(&self) -> String {
        match (&self.file, self.line) {
            (Some(file), Some(line)) => format!("{file}:{line}"),
            (Some(file), None) => file.clone(),
            (None, _) => "-".to_owned(),
        }
    }
}

/// What the swarm decided.
#[derive(Debug, Clone, PartialEq)]
pub struct ReviewReport {
    /// Whether the change may merge.
    pub approved: bool,
    /// Why, in a few words.
    pub reason: String,
    /// The blocking findings as Markdown, for the implementer and the notes.
    pub findings_markdown: String,
    /// Every reviewer and its verdict word, for the pull request body.
    pub verdicts: Vec<(String, String)>,
    /// Findings that do not block, to post as comments.
    pub minors: Vec<MinorFinding>,
    /// What the review cost, when known.
    pub cost_usd: Option<f64>,
}

/// Reviews a diff with several models.
pub trait ReviewSwarm: Send + Sync {
    /// Gate C.
    fn review<'a>(&'a self, input: &'a ReviewInput) -> BoxFuture<'a, Result<ReviewReport>>;
}

/// What the code host says about a commit's CI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CiStatus {
    /// Every check passed.
    Green,
    /// A check failed.
    Red {
        /// The commit.
        sha: String,
        /// Where to look.
        url: String,
    },
    /// Checks are still running.
    Pending,
    /// No checks were found.
    Unknown,
}

/// An open pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequest {
    /// The number.
    pub number: u64,
    /// The web page.
    pub url: String,
    /// The head commit.
    pub head_sha: String,
}

/// One inline comment on a line the diff shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineComment {
    /// Repository-relative path.
    pub path: String,
    /// Line in the new version of the file.
    pub line: u32,
    /// The comment text.
    pub body: String,
}

/// The label a pull request that touches a protected path gets.
pub const NEEDS_HUMAN_LABEL: &str = "needs-human";

/// The code host, GitHub in production and a bare repository in tests.
pub trait CodeHost: Send + Sync {
    /// The CI status of the latest commit on `main`.
    fn main_ci(&self) -> BoxFuture<'_, Result<CiStatus>>;

    /// The CI status of `main` with the commit it is for, when the host
    /// says; by default the commit is unknown.
    fn main_ci_with_sha(&self) -> BoxFuture<'_, Result<(CiStatus, Option<String>)>> {
        Box::pin(async move { Ok((self.main_ci().await?, None)) })
    }

    /// Open pull requests labelled [`NEEDS_HUMAN_LABEL`], with the
    /// top-level paths they change; none by default.
    fn needs_human_prs(&self) -> BoxFuture<'_, Result<Vec<NeedsHuman>>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    /// Opens a pull request from a pushed branch into `main`.
    fn open_pull<'a>(
        &'a self,
        head_branch: &'a str,
        title: &'a str,
        body: &'a str,
    ) -> BoxFuture<'a, Result<PullRequest>>;

    /// Posts one `COMMENT` review: a body and inline comments.
    fn post_review<'a>(
        &'a self,
        number: u64,
        head_sha: &'a str,
        body: &'a str,
        comments: &'a [InlineComment],
    ) -> BoxFuture<'a, Result<()>>;

    /// Adds a label, creating it when the repository lacks it.
    fn add_label<'a>(&'a self, number: u64, label: &'a str) -> BoxFuture<'a, Result<()>>;

    /// Waits for the `ci` check of a commit until it completes or the deadline passes.
    fn wait_for_ci<'a>(
        &'a self,
        head_sha: &'a str,
        deadline: Instant,
    ) -> BoxFuture<'a, Result<CiStatus>>;

    /// Squash-merges the pull request with the given commit title.
    fn squash_merge<'a>(&'a self, number: u64, title: &'a str) -> BoxFuture<'a, Result<()>>;

    /// Deletes a branch on the host.
    fn delete_branch<'a>(&'a self, branch: &'a str) -> BoxFuture<'a, Result<()>>;

    /// Closes the pull request with a comment saying why.
    fn close_pull<'a>(&'a self, number: u64, comment: &'a str) -> BoxFuture<'a, Result<()>>;
}

/// How working a ticket ended.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// The pull request was squash-merged.
    Merged {
        /// The pull request number.
        pr: u64,
        /// What the ticket cost in model calls, when known.
        cost_usd: Option<f64>,
    },
    /// The pull request touches a protected path and waits for a human.
    NeedsHuman {
        /// The pull request number.
        pr: u64,
        /// The protected paths or rules it touched.
        held: Vec<String>,
        /// What the ticket cost in model calls, when known.
        cost_usd: Option<f64>,
    },
    /// A gate or a budget rejected the ticket.
    Rejected {
        /// Why, in one line.
        reason: String,
        /// The full gate output, for `<id>.notes.md`.
        notes: String,
        /// What the ticket cost in model calls, when known.
        cost_usd: Option<f64>,
    },
    /// The factory cannot continue safely; the ticket goes back to `open/`.
    Paused {
        /// Why.
        reason: String,
    },
}

/// Something the worker reports while it works, for `factory status` and
/// the dashboard.
#[derive(Debug, Clone, PartialEq)]
pub enum ProgressEvent {
    /// A pipeline stage began.
    Stage {
        /// The stage.
        stage: Stage,
        /// What the worker is doing, in words, such as `gate A: just check (run 1)`.
        text: String,
    },
    /// The implementer run or the review round changed.
    Counters {
        /// The implementer run, from 1.
        run: u32,
        /// Review rounds that asked for changes so far.
        round: u32,
    },
    /// An implementer run ended with this usage.
    RunUsage {
        /// Its tokens.
        tokens: Tokens,
        /// Its cost, when known.
        cost_usd: Option<f64>,
    },
    /// The pull request was opened.
    PullRequest(u64),
}

impl ProgressEvent {
    /// A stage event.
    ///
    /// # Arguments
    ///
    /// * `stage` - The stage that began.
    /// * `text` - What the worker is doing, in words.
    ///
    /// # Returns
    ///
    /// The event.
    pub fn stage(stage: Stage, text: impl Into<String>) -> Self {
        Self::Stage {
            stage,
            text: text.into(),
        }
    }
}

/// Receives what the worker reports.
pub type Progress = Arc<dyn Fn(ProgressEvent) + Send + Sync>;

/// Looks at the machine for the dashboard: the worker image, disk, the
/// container of a running step, live usage and provider latency. Every
/// answer is best effort; what cannot be learned stays empty.
pub trait Probe: Send + Sync {
    /// The machine's health. `main_ci`, `errors_in_a_row` and the provider
    /// fields are the service's and stay empty here.
    fn health(&self) -> BoxFuture<'_, HealthView>;

    /// One minimal request to every provider the factory uses.
    fn ping_providers(&self) -> BoxFuture<'_, Vec<ProviderLatency>>;

    /// The container running a step of a ticket, with its cpu and memory.
    fn container<'a>(&'a self, ticket_id: &'a str) -> BoxFuture<'a, Option<ContainerView>>;

    /// What the implementer run in a worktree has used so far.
    fn usage(&self, worktree: &Path) -> Option<UsageView>;
}

/// A probe that learns nothing, for tests and machines without a sandbox.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoProbe;

impl Probe for NoProbe {
    fn health(&self) -> BoxFuture<'_, HealthView> {
        Box::pin(async { HealthView::default() })
    }

    fn ping_providers(&self) -> BoxFuture<'_, Vec<ProviderLatency>> {
        Box::pin(async { Vec::new() })
    }

    fn container<'a>(&'a self, _ticket_id: &'a str) -> BoxFuture<'a, Option<ContainerView>> {
        Box::pin(async { None })
    }

    fn usage(&self, _worktree: &Path) -> Option<UsageView> {
        None
    }
}

/// Works one claimed ticket from worktree to outcome.
pub trait Worker: Send + Sync {
    /// Works the ticket.
    ///
    /// # Arguments
    ///
    /// * `ticket` - The claimed ticket.
    /// * `queue` - The queue, for the `in_review` transition once a pull request exists.
    /// * `progress` - Where to report the current stage.
    ///
    /// # Returns
    ///
    /// The outcome. An `Err` means the factory itself failed, not the ticket.
    fn work<'a>(
        &'a self,
        ticket: &'a Ticket,
        queue: &'a TicketQueue,
        progress: Progress,
    ) -> BoxFuture<'a, Result<Outcome>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// anvil's own wording, from `IncompleteReason`'s Display in anvil-core:
    /// the factory reads it from the output, so a change there must show here.
    #[test]
    fn budget_stops_name_their_state() {
        let stop = |reason: &str| ImplementerStatus::Incomplete(reason.to_owned());
        let plan = stop("the plan state used its token budget");
        assert!(plan.is_budget_stop());
        assert_eq!(plan.budget_state(), Some("plan"));
        let turns = stop("the build state used its turn budget (after a denied tool call)");
        assert_eq!(turns.budget_state(), Some("build"));
        let session = stop("the session used its token budget");
        assert!(session.is_budget_stop());
        assert_eq!(session.budget_state(), None);
        let review = stop("the reviewer requested changes 3 times; giving up");
        assert!(!review.is_budget_stop());
        assert_eq!(review.budget_state(), None);
        assert!(!ImplementerStatus::Done.is_budget_stop());
    }
}
