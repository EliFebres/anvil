//! The worker loop: one claimed ticket from a fresh worktree to a merged
//! pull request, a pull request held for a human, or a rejection.
//!
//! ```text
//! worktree from origin/main
//! loop (review rounds and one Gate B retry are budgeted)
//!   loop (one Gate A retry per ticket)
//!     implementer run -> commit -> secret scan -> Gate A: just check
//!   Gate C: review swarm (changes -> a fix round; a failed reviewer -> pause)
//!   changed path matches [eval_gate]? release build -> Gate B: eval against main
//! push -> pull request -> minors as comments
//! protected path touched? -> label needs-human, wait
//! CI green? -> squash merge, delete branch
//! ```
//!
//! Every verdict is an [`Outcome`]; an `Err` means the factory itself failed.

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use anvil_types::{FactoryConfig, Ticket};
use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::board::Stage;
use crate::budget::{Budget, BudgetExceeded};
use crate::error::{FactoryError, Result};
use crate::git::Git;
use crate::paths::FactoryHome;
use crate::ports::{
    BoxFuture, CiStatus, CodeHost, EvalGate, GateB, GateCost, Implementer, ImplementerStatus,
    InlineComment, MinorFinding, NEEDS_HUMAN_LABEL, Outcome, Progress, ProgressEvent, ReviewInput,
    ReviewSwarm, Tokens, Worker,
};
use crate::protected::ProtectedPaths;
use crate::render::{self, GateRecord, GateResult, Notes, PullBody};
use crate::secrets;
use crate::tickets::{TicketQueue, Transition};

/// The repository file the style reviewer receives.
const GUIDELINES_FILE: &str = "CLAUDE.md";

/// Why Gate B did not run, for the gate records, the notes and the pull request.
const EVAL_SKIPPED_DETAIL: &str = "no changed path matches [eval_gate] paths";

/// A clock, injected so tests can run out the wall clock.
pub type InstantClock = Arc<dyn Fn() -> Instant + Send + Sync>;

/// Everything the worker needs.
pub struct WorkerDeps {
    /// The factory's clone of the monorepo; worktrees hang off it.
    pub repo: Git,
    /// The remote, normally `origin`.
    pub remote: String,
    /// `~/.factory`.
    pub home: FactoryHome,
    /// `factory.toml`.
    pub config: FactoryConfig,
    /// Runs anvil and the build tools.
    pub implementer: Arc<dyn Implementer>,
    /// Gate B.
    pub eval_gate: Arc<dyn EvalGate>,
    /// Gate C.
    pub review: Arc<dyn ReviewSwarm>,
    /// GitHub, or a fake.
    pub host: Arc<dyn CodeHost>,
    /// Secret values that must never appear in the worktree.
    pub secrets: Vec<String>,
    /// The clock.
    pub clock: InstantClock,
}

/// Works claimed tickets.
pub struct TicketWorker {
    deps: WorkerDeps,
    protected: ProtectedPaths,
    /// `[eval_gate] paths`; `None` runs Gate B for every diff.
    eval_paths: Option<GlobSet>,
}

impl TicketWorker {
    /// Builds the worker, compiling the protected and eval-gate paths.
    ///
    /// # Arguments
    ///
    /// * `deps` - The parts.
    ///
    /// # Errors
    ///
    /// [`FactoryError::Config`] when a protected or eval-gate path is not a glob.
    pub fn new(deps: WorkerDeps) -> Result<Self> {
        let protected = ProtectedPaths::from_config(&deps.config.protected)?;
        let eval_paths = deps
            .config
            .eval_gate
            .as_ref()
            .map(|gate| compile_eval_paths(&gate.paths))
            .transpose()?;
        Ok(Self {
            deps,
            protected,
            eval_paths,
        })
    }

    /// `origin/main`.
    fn main_ref(&self) -> String {
        format!("{}/main", self.deps.remote)
    }

    /// Works one ticket, cleaning the worktree up afterwards.
    async fn work_ticket(
        &self,
        ticket: &Ticket,
        queue: &TicketQueue,
        progress: Progress,
    ) -> Result<Outcome> {
        let id = ticket.id.to_string();
        let branch = format!("factory/{id}");
        let worktree = self.deps.home.work_dir(&id);
        progress(ProgressEvent::stage(
            Stage::Worktree,
            "preparing the worktree",
        ));
        self.deps.repo.fetch_prune(&self.deps.remote).await?;
        self.deps
            .repo
            .clone_for_ticket(&worktree, &self.deps.remote, &branch, &self.main_ref())
            .await?;
        let result = self.run(ticket, queue, &progress, &worktree, &branch).await;
        if let Err(error) = crate::git::remove_clone(&worktree) {
            tracing::warn!(%error, "could not remove the ticket's clone");
        }
        let _ = std::fs::remove_dir_all(self.deps.home.ticket_home(&id));
        result
    }

    /// The gates, the pull request and the merge policy.
    async fn run(
        &self,
        ticket: &Ticket,
        queue: &TicketQueue,
        progress: &Progress,
        worktree: &Path,
        branch: &str,
    ) -> Result<Outcome> {
        let deps = &self.deps;
        let wt = deps.repo.at(worktree);
        let main = self.main_ref();
        let mut run = Run::new(ticket, Budget::new((deps.clock)(), &deps.config.budget));
        let mut task = render::task(ticket);
        progress(ProgressEvent::stage(
            Stage::Worktree,
            "preparing the sandbox",
        ));
        deps.implementer.prepare(worktree).await?;

        let mut plan_retried = false;
        let mut eval_retried = false;
        let mut earlier_findings: Vec<String> = Vec::new();
        let minors = loop {
            run.gates.clear();
            // Gate A: implement, commit, scan, check.
            loop {
                if let Err(exceeded) = run.budget.check_time((deps.clock)()) {
                    return Ok(run.reject(exceeded.to_string(), deps.implementer.as_ref()));
                }
                run.implementer_runs += 1;
                let n = run.implementer_runs;
                progress(ProgressEvent::stage(
                    Stage::Implement,
                    format!("implementer run {n}"),
                ));
                progress(ProgressEvent::Counters {
                    run: n,
                    round: run.budget.review_rounds(),
                });
                let out = deps
                    .implementer
                    .run(worktree, &task, run.budget.deadline())
                    .await?;
                run.notes.section(
                    &format!("implementer run {n}: {}", describe_status(&out.status)),
                    &out.output_tail,
                );
                run.notes.line(&format!(
                    "tokens: {} (input {}, output {}, cached {}; weighted {}){}",
                    out.tokens.total(),
                    out.tokens.input,
                    out.tokens.output,
                    out.tokens.cached,
                    out.tokens.weighted(),
                    out.cost_usd
                        .map(|c| format!(", ${c:.2}"))
                        .unwrap_or_default()
                ));
                run.add_cost(out.cost_usd);
                run.record_cost(
                    progress,
                    format!("implementer run {n}"),
                    out.cost_usd,
                    Some(out.tokens.weighted()),
                );
                progress(ProgressEvent::RunUsage {
                    tokens: out.tokens,
                    cost_usd: out.cost_usd,
                });
                if let Err(exceeded) = run.budget.add_tokens(out.tokens.weighted()) {
                    return Ok(run.reject(exceeded.to_string(), deps.implementer.as_ref()));
                }
                if out.status == ImplementerStatus::Timeout {
                    let limit = BudgetExceeded::WallClock {
                        limit: run.budget.deadline() - (deps.clock)(),
                    };
                    let _ = limit;
                    return Ok(run.reject(
                        format!(
                            "wall clock budget of {} minutes exceeded during implementer run {n}",
                            deps.config.budget.wall_clock_minutes
                        ),
                        deps.implementer.as_ref(),
                    ));
                }
                let committed = wt
                    .commit_all(&format!("factory: {} (run {n})", ticket.title))
                    .await?;
                if !committed && wt.diff_names(&main, "HEAD").await?.is_empty() {
                    let ImplementerStatus::Incomplete(reason) = &out.status else {
                        return Ok(run.reject(
                            format!("implementer run {n} changed nothing"),
                            deps.implementer.as_ref(),
                        ));
                    };
                    let reason =
                        format!("implementer run {n} stopped before changing anything: {reason}");
                    // A plan that ran out of budget never reached the build, so
                    // one fresh attempt is worth it; anything else would repeat.
                    if out.status.budget_state() == Some("plan") && !plan_retried {
                        plan_retried = true;
                        run.notes.line(&format!("{reason}; retrying once"));
                        task = render::append_plan_retry(&task);
                        continue;
                    }
                    return Ok(run.reject(reason, deps.implementer.as_ref()));
                }
                let leaked = secrets::scan(worktree, &deps.secrets).await?;
                if !leaked.is_empty() {
                    let files: Vec<String> =
                        leaked.iter().map(|p| p.display().to_string()).collect();
                    run.notes
                        .line(&format!("secret values found in: {}", files.join(", ")));
                    return Ok(run.reject(
                        format!(
                            "a secret value appeared in the worktree ({}); nothing was pushed",
                            files.join(", ")
                        ),
                        deps.implementer.as_ref(),
                    ));
                }
                progress(ProgressEvent::stage(
                    Stage::GateA,
                    format!("gate A: just check (run {n})"),
                ));
                let check = deps
                    .implementer
                    .check(worktree, run.budget.deadline())
                    .await?;
                run.notes.section(
                    &format!("gate A after run {n}: {}", pass_word(check.passed)),
                    &check.output_tail,
                );
                if check.passed {
                    // Inside the factory `just check` formats before it checks,
                    // so what it reformatted must reach the pushed branch.
                    wt.commit_all(&format!("factory: cargo fmt (gate A, run {n})"))
                        .await?;
                    run.gates.push(GateRecord {
                        name: "A: just check".to_owned(),
                        result: GateResult::Pass,
                        detail: format!("passed after implementer run {n}"),
                    });
                    break;
                }
                if let Err(exceeded) = run.budget.gate_a_failed() {
                    run.gates.push(GateRecord {
                        name: "A: just check".to_owned(),
                        result: GateResult::Fail,
                        detail: format!("failed after implementer run {n}"),
                    });
                    return Ok(run.reject(format!("gate A: {exceeded}"), deps.implementer.as_ref()));
                }
                task = render::append_check_failure(&task, &check.output_tail);
            }

            // Gate C: the review swarm. It runs before the release build and
            // the eval, which are the expensive gates, so a change the
            // reviewers send back never pays for them.
            progress(ProgressEvent::stage(Stage::GateC, "gate C: review swarm"));
            let changed = wt.diff_names(&main, "HEAD").await?;
            let input = ReviewInput {
                ticket: ticket.clone(),
                diff: wt.diff_text(&main, "HEAD").await?,
                changed_files: changed.clone(),
                guidelines: deps.repo.show(&main, GUIDELINES_FILE).await.ok(),
                earlier_findings: earlier_findings.clone(),
            };
            let report = deps.review.review(&input).await?;
            run.add_cost(report.cost_usd);
            run.review_calls += 1;
            run.record_cost(
                progress,
                format!("C: review round {}", run.review_calls),
                report.cost_usd,
                Some(report.tokens.weighted()),
            );
            run.verdicts = report.verdicts.clone();
            run.notes.markdown(
                &format!(
                    "gate C: {} ({})",
                    if report.approved {
                        "approve"
                    } else {
                        "changes"
                    },
                    report.reason
                ),
                if report.findings_markdown.trim().is_empty() {
                    "no blocking findings"
                } else {
                    &report.findings_markdown
                },
            );
            if !report.failed.is_empty() {
                // An unreachable reviewer says nothing about the diff, so
                // neither an approval nor another implementer run is earned.
                let reason = format!(
                    "gate C: reviewer(s) could not be reached after retries: {}; no implementer run was spent",
                    report.failed.join(", ")
                );
                run.notes.line(&reason);
                run.gates.push(GateRecord {
                    name: "C: review".to_owned(),
                    result: GateResult::Fail,
                    detail: format!("reviewer(s) unreachable: {}", report.failed.join(", ")),
                });
                return Ok(Outcome::Paused { reason });
            }
            if !report.approved {
                run.gates.push(GateRecord {
                    name: "C: review".to_owned(),
                    result: GateResult::Fail,
                    detail: format!("changes: {}", report.reason),
                });
                if let Err(exceeded) = run.budget.review_asked_for_changes() {
                    return Ok(run.reject(format!("gate C: {exceeded}"), deps.implementer.as_ref()));
                }
                task = render::fix_task(ticket, &changed, &report.findings_markdown);
                earlier_findings.push(report.findings_markdown);
                continue;
            }
            run.gates.push(GateRecord {
                name: "C: review".to_owned(),
                result: GateResult::Pass,
                detail: format!("approve: {}", report.reason),
            });
            let minors = report.minors;

            // Gate B: the eval harness on the branch build, when the diff can
            // change anvil at all.
            if self.skips_eval(&changed) {
                run.notes
                    .line(&format!("gate B: skipped: {EVAL_SKIPPED_DETAIL}"));
                run.gates.push(GateRecord {
                    name: "B: eval".to_owned(),
                    result: GateResult::Skipped,
                    detail: EVAL_SKIPPED_DETAIL.to_owned(),
                });
                break minors;
            }
            if deps.eval_gate.needs_release_build() {
                progress(ProgressEvent::stage(
                    Stage::GateB,
                    "building the release binary",
                ));
                let build = deps
                    .implementer
                    .build_release(worktree, run.budget.deadline())
                    .await?;
                run.notes.section(
                    &format!("release build: {}", pass_word(build.passed)),
                    &build.output_tail,
                );
                if !build.passed {
                    return Ok(run.reject(
                        "the release build of anvil failed after `just check` passed".to_owned(),
                        deps.implementer.as_ref(),
                    ));
                }
            }
            progress(ProgressEvent::stage(
                Stage::GateB,
                "gate B: eval against main",
            ));
            match deps.eval_gate.run(worktree).await? {
                GateB::NoBaseline => {
                    return Ok(Outcome::Paused {
                        reason: FactoryError::NoBaseline.to_string(),
                    });
                }
                GateB::Failed(report) => {
                    run.notes.markdown("gate B: fail", &report.summary);
                    run.add_cost(report.cost_usd);
                    run.record_cost(
                        progress,
                        "B: eval".to_owned(),
                        report.cost_usd,
                        report.tokens.map(Tokens::weighted),
                    );
                    run.gates.push(GateRecord {
                        name: "B: eval".to_owned(),
                        result: GateResult::Fail,
                        detail: format!("{} regression(s)", report.regressions.len()),
                    });
                    // A branch that breaks anvil itself fails every task in
                    // seconds; one more run with the evidence can fix that.
                    if !eval_retried {
                        eval_retried = true;
                        run.notes.line(
                            "gate B failed; sending the regressions back to the implementer once",
                        );
                        task = render::append_eval_failure(&task, &report.summary);
                        continue;
                    }
                    return Ok(run.reject(
                        format!(
                            "gate B: {} eval task(s) regressed against main: {}",
                            report.regressions.len(),
                            report.regressions.join(", ")
                        ),
                        deps.implementer.as_ref(),
                    ));
                }
                GateB::Passed(report) => {
                    run.notes.markdown("gate B: pass", &report.summary);
                    run.add_cost(report.cost_usd);
                    run.record_cost(
                        progress,
                        "B: eval".to_owned(),
                        report.cost_usd,
                        report.tokens.map(Tokens::weighted),
                    );
                    run.gates.push(GateRecord {
                        name: "B: eval".to_owned(),
                        result: GateResult::Pass,
                        detail: format!("{} regression(s)", report.regressions.len()),
                    });
                    run.eval_summary = Some(report.summary);
                    break minors;
                }
            }
        };

        // Publish.
        progress(ProgressEvent::stage(Stage::Pr, "pushing the branch"));
        wt.push(&deps.remote, branch).await?;
        let changed = wt.diff_names(&main, "HEAD").await?;
        let mut held: Vec<String> = self
            .protected
            .held(&changed)
            .iter()
            .map(ToString::to_string)
            .collect();
        if self.protected.checks_cargo_dependencies() {
            for path in changed.iter().filter(|p| p.ends_with("Cargo.toml")) {
                let base = deps.repo.show(&main, path).await.unwrap_or_default();
                let head = wt.show("HEAD", path).await?;
                held.extend(
                    self.protected
                        .dependency_additions(path, &base, &head)
                        .iter()
                        .map(ToString::to_string),
                );
            }
        }
        let body = render::pull_body(&PullBody {
            ticket,
            gates: &run.gates,
            costs: &run.gate_costs,
            eval_summary: run.eval_summary.as_deref(),
            verdicts: &run.verdicts,
            held: &held,
        });
        progress(ProgressEvent::stage(Stage::Pr, "opening the pull request"));
        let pull = deps.host.open_pull(branch, &ticket.title, &body).await?;
        progress(ProgressEvent::PullRequest(pull.number));
        queue
            .transition(ticket.id, Transition::InReview { pr: pull.number })
            .await?;
        if !minors.is_empty() {
            let (inline, body) = split_minors(&minors);
            if let Err(error) = deps
                .host
                .post_review(pull.number, &pull.head_sha, &body, &inline)
                .await
            {
                tracing::warn!(%error, "could not post the minor findings");
            }
        }
        if !held.is_empty() {
            deps.host.add_label(pull.number, NEEDS_HUMAN_LABEL).await?;
            return Ok(Outcome::NeedsHuman {
                pr: pull.number,
                held,
                cost_usd: run.cost,
                gate_costs: run.gate_costs,
            });
        }

        progress(ProgressEvent::stage(Stage::Merge, "waiting for CI"));
        match deps
            .host
            .wait_for_ci(&pull.head_sha, run.budget.deadline())
            .await?
        {
            CiStatus::Green => {
                progress(ProgressEvent::stage(Stage::Merge, "merging"));
                deps.host.squash_merge(pull.number, &ticket.title).await?;
                deps.host.delete_branch(branch).await?;
                Ok(Outcome::Merged {
                    pr: pull.number,
                    cost_usd: run.cost,
                    gate_costs: run.gate_costs,
                })
            }
            CiStatus::Red { url, .. } => {
                let reason = format!("CI failed on the pull request: {url}");
                self.abandon(pull.number, branch, &reason).await;
                Ok(run.reject(reason, deps.implementer.as_ref()))
            }
            CiStatus::Pending => {
                let reason = format!(
                    "wall clock budget of {} minutes exceeded while waiting for CI",
                    deps.config.budget.wall_clock_minutes
                );
                self.abandon(pull.number, branch, &reason).await;
                Ok(run.reject(reason, deps.implementer.as_ref()))
            }
            CiStatus::Unknown => {
                let reason = "no `ci` check reported for the pull request".to_owned();
                self.abandon(pull.number, branch, &reason).await;
                Ok(run.reject(reason, deps.implementer.as_ref()))
            }
        }
    }

    /// Whether the diff cannot change anvil, so Gate B has nothing to test.
    fn skips_eval(&self, changed: &[String]) -> bool {
        self.eval_paths
            .as_ref()
            .is_some_and(|paths| !changed.iter().any(|path| paths.is_match(path)))
    }

    /// Closes a pull request that will not merge and deletes its branch.
    async fn abandon(&self, number: u64, branch: &str, reason: &str) {
        let comment = format!("Closed by the factory: {reason}");
        if let Err(error) = self.deps.host.close_pull(number, &comment).await {
            tracing::warn!(%error, "could not close the pull request");
        }
        if let Err(error) = self.deps.host.delete_branch(branch).await {
            tracing::warn!(%error, "could not delete the branch");
        }
    }
}

impl Worker for TicketWorker {
    fn work<'a>(
        &'a self,
        ticket: &'a Ticket,
        queue: &'a TicketQueue,
        progress: Progress,
    ) -> BoxFuture<'a, Result<Outcome>> {
        Box::pin(self.work_ticket(ticket, queue, progress))
    }
}

/// What one ticket has accumulated so far.
struct Run {
    budget: Budget,
    notes: Notes,
    cost: Option<f64>,
    gates: Vec<GateRecord>,
    eval_summary: Option<String>,
    verdicts: Vec<(String, String)>,
    implementer_runs: u32,
    review_calls: u32,
    gate_costs: Vec<GateCost>,
}

impl Run {
    fn new(ticket: &Ticket, budget: Budget) -> Self {
        Self {
            budget,
            notes: Notes::new(ticket),
            cost: Some(0.0),
            gates: Vec::new(),
            eval_summary: None,
            verdicts: Vec::new(),
            implementer_runs: 0,
            review_calls: 0,
            gate_costs: Vec::new(),
        }
    }

    /// Keeps one step's cost, notes it and reports it to the dashboard.
    fn record_cost(
        &mut self,
        progress: &Progress,
        step: String,
        cost_usd: Option<f64>,
        weighted_tokens: Option<u64>,
    ) {
        let cost = GateCost {
            step,
            cost_usd,
            weighted_tokens,
        };
        self.notes.line(&format!("cost: {}", describe_cost(&cost)));
        progress(ProgressEvent::GateCost(cost.clone()));
        self.gate_costs.push(cost);
    }

    /// Adds a cost; an unknown cost makes the total unknown.
    fn add_cost(&mut self, cost: Option<f64>) {
        self.cost = match (self.cost, cost) {
            (Some(total), Some(cost)) => Some(total + cost),
            _ => None,
        };
    }

    /// The rejection outcome with the notes closed out.
    fn reject(mut self, reason: String, implementer: &dyn Implementer) -> Outcome {
        let denied = implementer.denied_hosts();
        if !denied.is_empty() {
            self.notes.markdown(
                "egress refused by the allowlist",
                &denied
                    .iter()
                    .map(|host| format!("- `{host}`"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
        }
        self.notes.line(&format!("**Rejected:** {reason}"));
        if !self.gate_costs.is_empty() {
            let list = self
                .gate_costs
                .iter()
                .map(|cost| format!("- {}", describe_cost(cost)))
                .collect::<Vec<_>>()
                .join("\n");
            self.notes.markdown("cost per step", &list);
        }
        self.notes.line(&format!(
            "weighted tokens used: {}{}",
            self.budget.tokens_used(),
            self.cost
                .map(|c| format!(", cost ${c:.2}"))
                .unwrap_or_default()
        ));
        Outcome::Rejected {
            reason,
            notes: self.notes.text().to_owned(),
            cost_usd: self.cost,
            gate_costs: self.gate_costs,
        }
    }
}

/// Compiles the `[eval_gate] paths` globs.
fn compile_eval_paths(paths: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for path in paths {
        let glob = Glob::new(path).map_err(|error| {
            FactoryError::Config(format!("eval_gate path `{path}` is not a glob: {error}"))
        })?;
        builder.add(glob);
    }
    builder
        .build()
        .map_err(|error| FactoryError::Config(format!("eval_gate paths: {error}")))
}

/// `step: $0.01, 1000 weighted tokens`, with `cost unknown` and `-` for what was not reported.
fn describe_cost(cost: &GateCost) -> String {
    format!(
        "{}: {}, {} weighted tokens",
        cost.step,
        cost.cost_usd
            .map_or_else(|| "cost unknown".to_owned(), |usd| format!("${usd:.2}")),
        cost.weighted_tokens
            .map_or_else(|| "-".to_owned(), |tokens| tokens.to_string())
    )
}

/// One word for a gate result.
fn pass_word(passed: bool) -> &'static str {
    if passed { "pass" } else { "fail" }
}

/// One line for an implementer status.
fn describe_status(status: &ImplementerStatus) -> String {
    match status {
        ImplementerStatus::Done => "done".to_owned(),
        ImplementerStatus::Incomplete(reason) => format!("incomplete ({reason})"),
        ImplementerStatus::Failed { code: Some(code) } => format!("exited {code}"),
        ImplementerStatus::Failed { code: None } => "killed".to_owned(),
        ImplementerStatus::Timeout => "timed out".to_owned(),
    }
}

/// Splits the minors into inline comments (anchored on a line the diff
/// shows) and a review body that lists the rest.
///
/// # Arguments
///
/// * `minors` - The minor findings.
///
/// # Returns
///
/// The inline comments and the body.
pub fn split_minors(minors: &[MinorFinding]) -> (Vec<InlineComment>, String) {
    let mut inline = Vec::new();
    let mut rest = Vec::new();
    for minor in minors {
        match (&minor.file, minor.line, minor.anchored) {
            (Some(file), Some(line), true) => inline.push(InlineComment {
                path: file.clone(),
                line,
                body: minor.body.clone(),
            }),
            _ => rest.push(minor),
        }
    }
    let mut body = String::from("factory review: minor findings only; nothing blocks the merge.");
    if !rest.is_empty() {
        body.push_str(
            "

Outside the diff:
",
        );
        for minor in rest {
            body.push_str(&format!(
                "
- `{}`: {}",
                minor.location(),
                minor.body
            ));
        }
    }
    (inline, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_repository_eval_gate_globs_match_what_can_change_anvil() {
        let text = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../factory.toml"),
        )
        .expect("factory.toml");
        let config: FactoryConfig = toml::from_str(&text).expect("factory.toml parses");
        let gate = config.eval_gate.expect("factory.toml has [eval_gate]");
        let set = compile_eval_paths(&gate.paths).expect("globs compile");
        for path in [
            "crates/anvil-core/src/x.rs",
            "crates/anvil-cli/tests/cli.rs",
            "crates/eval/src/lib.rs",
            "skills/hello/SKILL.md",
            "eval/tasks/rs-grep/task.toml",
            "Cargo.lock",
            "Cargo.toml",
            "rust-toolchain.toml",
        ] {
            assert!(set.is_match(path), "{path} should run gate B");
        }
        for path in [
            "crates/factory-core/src/worker.rs",
            "README.md",
            "docs/x.md",
            "crates/factory-cli/Cargo.toml",
            "deploy/nuc/README.md",
        ] {
            assert!(!set.is_match(path), "{path} should skip gate B");
        }
    }

    #[test]
    fn a_bad_eval_gate_glob_is_a_config_error() {
        let error = compile_eval_paths(&["crates/[".to_owned()]).expect_err("not a glob");
        assert!(matches!(error, FactoryError::Config(_)), "{error}");
    }

    #[test]
    fn minors_with_an_anchored_line_go_inline_and_the_rest_into_the_body() {
        let minors = [
            MinorFinding {
                file: Some("a.rs".to_owned()),
                line: Some(3),
                anchored: true,
                body: "inline".to_owned(),
            },
            MinorFinding {
                file: Some("b.rs".to_owned()),
                line: Some(40),
                anchored: false,
                body: "line not in diff".to_owned(),
            },
            MinorFinding {
                file: None,
                line: None,
                anchored: false,
                body: "nowhere".to_owned(),
            },
        ];
        let (inline, body) = split_minors(&minors);
        assert_eq!(inline.len(), 1);
        assert_eq!(inline[0].path, "a.rs");
        assert!(body.contains("- `b.rs:40`: line not in diff"), "{body}");
        assert!(body.contains("- `-`: nowhere"));
        assert_eq!(
            describe_status(&ImplementerStatus::Failed { code: Some(3) }),
            "exited 3"
        );
    }
}
