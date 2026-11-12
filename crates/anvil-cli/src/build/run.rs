//! The build driver: plans a specification into work items, runs one fresh
//! session per item, decides "done" by running the checks itself, commits,
//! and stops with a reason when a budget or an item gives out.
//!
//! It knows nothing about providers or terminals: the [`Env`] trait is how
//! it asks for the planner and for a session, so tests drive it with
//! scripted fakes and the binary drives it with the real wiring.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anvil_core::agent::{CheckCommand, PlanDecision, PlanGate};
use anvil_core::context::{collect_text, new_session_id, session_dir};
use anvil_core::provider::{Message, Provider};
use anvil_core::tools::ToolContext;
use anvil_core::tools::shell::{self, Shell};
use anvil_types::{Price, SessionState};
use anyhow::{Context, bail};
use chrono::Utc;

use super::git;
use super::plan::{
    Finished, MAX_ATTEMPTS, TaskInputs, WorkItem, finished_summary, item_file_name, item_task,
    parse_plan, planning_request, render_item_file, render_list,
};
use super::state::{
    BuildState, BuildStatus, ItemState, ItemStatus, SCHEMA_VERSION, build_dir, pick_build,
};
use crate::status::StatusReporter;

/// How long one acceptance or project check command may run.
const CHECK_TIMEOUT: Duration = Duration::from_secs(900);

/// Lines of a failed check's output kept for the next attempt and the report.
const FAILURE_TAIL_LINES: usize = 20;

/// Exit code of a build that stopped before every item was done.
pub const STOPPED_EXIT: i32 = 2;

/// Exit code of a build the owner declined to start.
pub const DECLINED_EXIT: i32 = 1;

/// A request to run one item's session.
#[derive(Debug, Clone)]
pub struct SessionRequest {
    /// The task text.
    pub task: String,
    /// The session folder to use.
    pub session_dir: PathBuf,
    /// Weighted tokens the session may use; its share of the build's budget.
    pub token_limit: u64,
}

/// How a session ended, as far as the build cares.
#[derive(Debug, Clone, Default)]
pub struct SessionReport {
    /// Whether the loop reported success; the build never trusts it alone.
    pub reported_done: bool,
    /// One line about how the session ended.
    pub detail: String,
    /// Weighted tokens the session used.
    pub tokens: u64,
    /// Estimated cost, when a price is configured.
    pub cost_usd: Option<f64>,
}

/// What the build needs from the outside world.
pub trait Env {
    /// The provider that plans: the planner role, or the coder.
    ///
    /// # Errors
    ///
    /// When the provider cannot be built.
    fn planner(&self) -> anyhow::Result<Arc<dyn Provider>>;

    /// Runs one fresh session through the normal Plan, Build, Review,
    /// Verify loop and reports how it ended.
    async fn run_session(&self, request: SessionRequest) -> SessionReport;

    /// The status reporter for the build itself; `None` writes no status file.
    fn build_status(&self, _id: &str, _dir: &Path, _spec: &str) -> Option<StatusReporter> {
        None
    }
}

/// Everything that stays the same for the whole run of a build.
pub struct Settings {
    /// The project root.
    pub cwd: PathBuf,
    /// The shell that runs acceptance and project checks.
    pub shell: Shell,
    /// How the project's own check is found.
    pub check: CheckCommand,
    /// The most weighted tokens one session may use, from the configuration.
    pub session_tokens: u64,
    /// The coder's price, to turn a dollar budget into tokens for a session.
    pub price: Option<Price>,
}

/// Caps on a whole build, from the command line.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Budgets {
    /// Cap on cost in USD.
    pub usd: Option<f64>,
    /// Cap on weighted tokens.
    pub tokens: Option<u64>,
}

/// Plans a specification, saves the plan and runs the build.
///
/// # Arguments
///
/// * `env` - Planner and sessions.
/// * `settings` - Paths, shell and checks.
/// * `spec_path` - The specification file.
/// * `budgets` - Caps on the whole build.
/// * `gate` - Who approves the item list.
///
/// # Returns
///
/// The exit code: 0 when every item is done, [`STOPPED_EXIT`] when the
/// build stopped, [`DECLINED_EXIT`] when the owner aborted the plan.
///
/// # Errors
///
/// When the project is not a clean git repository, the specification
/// cannot be read, the planner cannot produce a valid list, or files
/// cannot be written.
pub async fn start<E: Env>(
    env: &E,
    settings: &Settings,
    spec_path: &Path,
    budgets: Budgets,
    gate: &dyn PlanGate,
) -> anyhow::Result<i32> {
    check_budgets(budgets)?;
    let branch = require_clean_repo(&settings.cwd)?;
    let spec = std::fs::read_to_string(spec_path)
        .with_context(|| format!("reading the specification {}", spec_path.display()))?;
    if spec.trim().is_empty() {
        bail!("the specification {} is empty", spec_path.display());
    }
    let planner = env.planner()?;
    let Some((items, planning_tokens)) = plan_items(planner.as_ref(), &spec, gate).await? else {
        eprintln!("build aborted; nothing was written");
        return Ok(DECLINED_EXIT);
    };
    let id = new_session_id();
    let dir = build_dir(&settings.cwd, &id);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    std::fs::write(dir.join("spec.md"), &spec).context("saving a copy of the specification")?;
    let mut state = BuildState {
        schema: SCHEMA_VERSION,
        id,
        created_at: Utc::now(),
        spec_path: spec_path.display().to_string(),
        branch,
        budget_usd: budgets.usd,
        budget_tokens: budgets.tokens,
        planning_tokens,
        status: BuildStatus::Running,
        detail: String::new(),
        items: items
            .into_iter()
            .enumerate()
            .map(|(index, item)| {
                let file = item_file_name(index + 1, &item.title);
                ItemState {
                    item,
                    file,
                    status: ItemStatus::Pending,
                    attempts: 0,
                    tokens: 0,
                    cost_usd: None,
                    commit: None,
                }
            })
            .collect(),
    };
    for item in &state.items {
        std::fs::write(dir.join(&item.file), render_item_file(&item.item))
            .with_context(|| format!("writing {}", item.file))?;
    }
    state.save(&dir)?;
    eprintln!(
        "build {} planned: {} work item(s)",
        state.id,
        state.items.len()
    );
    run_build(env, settings, &dir, &mut state, &spec).await
}

/// Continues a build from its first item that is not done.
///
/// # Arguments
///
/// * `env` - Planner and sessions.
/// * `settings` - Paths, shell and checks.
/// * `id` - The build id; the newest unfinished build when `None`.
/// * `budgets` - New caps that replace the saved ones where given.
///
/// # Returns
///
/// The exit code, as for [`start`].
///
/// # Errors
///
/// When there is no such build, the repository is on another branch or has
/// uncommitted changes (unless an item was left in progress, whose changes
/// are then saved to a side branch), or files cannot be written.
pub async fn resume<E: Env>(
    env: &E,
    settings: &Settings,
    id: Option<&str>,
    budgets: Budgets,
) -> anyhow::Result<i32> {
    check_budgets(budgets)?;
    let (id, mut state) = pick_build(&settings.cwd, id, true)?;
    if state.status == BuildStatus::Done {
        eprintln!("build {id} is already done");
        return Ok(0);
    }
    if !git::is_repo(&settings.cwd) {
        bail!("{} is not inside a git repository", settings.cwd.display());
    }
    let branch = git::current_branch(&settings.cwd)?;
    if branch != state.branch {
        bail!(
            "build {id} works on branch `{}` but `{branch}` is checked out; run `git checkout {}` first",
            state.branch,
            state.branch
        );
    }
    let interrupted = state
        .items
        .iter()
        .any(|item| item.status == ItemStatus::InProgress);
    if !interrupted {
        require_clean_tree(&settings.cwd)?;
    }
    state.budget_usd = budgets.usd.or(state.budget_usd);
    state.budget_tokens = budgets.tokens.or(state.budget_tokens);
    let dir = build_dir(&settings.cwd, &id);
    let spec =
        std::fs::read_to_string(dir.join("spec.md")).context("reading the saved specification")?;
    eprintln!("resuming build {id}: {} item(s) left", state.remaining());
    run_build(env, settings, &dir, &mut state, &spec).await
}

/// Refuses a budget that is not a positive, finite number.
fn check_budgets(budgets: Budgets) -> anyhow::Result<()> {
    if let Some(usd) = budgets.usd {
        anyhow::ensure!(
            usd.is_finite() && usd > 0.0,
            "--budget-usd must be a positive number"
        );
    }
    if budgets.tokens == Some(0) {
        bail!("--budget-tokens must be above zero");
    }
    Ok(())
}

/// Checks the project is a git repository on a branch with a clean tree.
fn require_clean_repo(cwd: &Path) -> anyhow::Result<String> {
    if !git::is_repo(cwd) {
        bail!(
            "{} is not inside a git repository; a build commits its work as it goes and needs one",
            cwd.display()
        );
    }
    let branch = git::current_branch(cwd)?;
    require_clean_tree(cwd)?;
    Ok(branch)
}

/// Fails with the list of changes when the work tree is dirty.
fn require_clean_tree(cwd: &Path) -> anyhow::Result<()> {
    let dirty = git::dirty_paths(cwd)?;
    if dirty.is_empty() {
        return Ok(());
    }
    let shown: Vec<&str> = dirty.iter().take(5).map(String::as_str).collect();
    bail!(
        "the work tree has uncommitted changes (outside .anvil/); commit or stash them first:\n  {}{}",
        shown.join("\n  "),
        if dirty.len() > shown.len() {
            "\n  ..."
        } else {
            ""
        }
    );
}

/// Asks the planner for the list until the owner approves it.
///
/// Returns `None` when the owner aborts.
async fn plan_items(
    planner: &dyn Provider,
    spec: &str,
    gate: &dyn PlanGate,
) -> anyhow::Result<Option<(Vec<WorkItem>, u64)>> {
    let mut earlier: Vec<Message> = Vec::new();
    let mut tokens = 0;
    loop {
        let (items, reply, used) = ask_for_plan(planner, spec, &earlier).await?;
        tokens += used;
        match gate.review(&render_list(&items)) {
            PlanDecision::Approve => return Ok(Some((items, tokens))),
            PlanDecision::Abort => return Ok(None),
            PlanDecision::Revise(feedback) => {
                earlier.push(assistant(reply));
                earlier.push(Message::user(format!(
                    "The owner wants changes to the list: {feedback}\nReply again with the whole list in the same JSON format."
                )));
            }
        }
    }
}

/// One planner exchange, with one re-ask when the reply is malformed.
async fn ask_for_plan(
    planner: &dyn Provider,
    spec: &str,
    earlier: &[Message],
) -> anyhow::Result<(Vec<WorkItem>, String, u64)> {
    let mut conversation = earlier.to_vec();
    let mut tokens = 0;
    let mut last_error = None;
    for _ in 0..2 {
        let (reply, usage) = collect_text(planner, planning_request(spec, &conversation))
            .await
            .context("the planner's completion")?;
        tokens += usage.map_or(0, |usage| usage.weighted());
        match parse_plan(&reply) {
            Ok(items) => return Ok((items, reply, tokens)),
            Err(error) => {
                conversation.push(assistant(reply));
                conversation.push(Message::user(format!(
                    "That reply could not be used: {error:#}\nReply again with the whole list in the required JSON block."
                )));
                last_error = Some(error);
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("no plan")))
        .context("the planner's list was malformed twice")
}

/// A text-only assistant message.
fn assistant(text: String) -> Message {
    Message::Assistant {
        text,
        reasoning: Vec::new(),
        tool_calls: Vec::new(),
    }
}

/// Runs the items that are not done, then reports and returns the exit code.
async fn run_build<E: Env>(
    env: &E,
    settings: &Settings,
    dir: &Path,
    state: &mut BuildState,
    spec: &str,
) -> anyhow::Result<i32> {
    let status = env.build_status(&state.id, dir, &state.spec_path);
    let outcome = run_items(env, settings, dir, state, spec, status.as_ref()).await;
    if let Some(status) = &status {
        match &outcome {
            Err(error) => status.fail(&format!("{error:#}")),
            Ok(_) => status.finish_as(session_state(state.status), &state.detail),
        }
        status.shutdown().await;
    }
    outcome?;
    eprintln!("\n{}", state.report());
    Ok(if state.status == BuildStatus::Done {
        0
    } else {
        STOPPED_EXIT
    })
}

/// The status-file state that matches a build's end.
fn session_state(status: BuildStatus) -> SessionState {
    match status {
        BuildStatus::Done => SessionState::Done,
        BuildStatus::NeedsInput => SessionState::NeedsInput,
        BuildStatus::Blocked => SessionState::Blocked,
        BuildStatus::Running => SessionState::Working,
    }
}

/// The loop over the items; leaves the outcome in `state`.
async fn run_items<E: Env>(
    env: &E,
    settings: &Settings,
    dir: &Path,
    state: &mut BuildState,
    spec: &str,
    status: Option<&StatusReporter>,
) -> anyhow::Result<()> {
    state.status = BuildStatus::Running;
    state.detail.clear();
    state.save(dir)?;
    for index in 0..state.items.len() {
        if state.items[index].status == ItemStatus::Done {
            continue;
        }
        if let Some(reason) = budget_reached(state) {
            return stop(state, dir, BuildStatus::Blocked, reason);
        }
        let number = index + 1;
        let title = state.items[index].item.title.clone();
        eprintln!("\n== work item {number} of {}: {title}", state.items.len());
        if let Some(status) = status {
            status.note_working(&format!("item {number} of {}: {title}", state.items.len()));
        }
        if let Some(failure) = run_item(env, settings, dir, state, index, spec).await? {
            let detail = format!(
                "work item {number} (`{title}`) failed {MAX_ATTEMPTS} attempts: {failure}; \
                 its last attempt is on branch `{}`; fix the cause and run `anvil build resume {}`",
                side_branch(&state.id, number, state.items[index].attempts),
                state.id
            );
            return stop(state, dir, BuildStatus::NeedsInput, detail);
        }
    }
    state.status = BuildStatus::Done;
    state.detail = "every work item is done".to_owned();
    state.save(dir)
}

/// Records why the build stopped.
fn stop(
    state: &mut BuildState,
    dir: &Path,
    status: BuildStatus,
    detail: String,
) -> anyhow::Result<()> {
    state.status = status;
    state.detail = detail;
    state.save(dir)
}

/// The reason to stop before the next item, when a cap is reached.
fn budget_reached(state: &BuildState) -> Option<String> {
    if let Some(cap) = state.budget_tokens
        && state.spent_tokens() >= cap
    {
        return Some(format!(
            "the token budget of {cap} is reached ({} used); raise it with `anvil build resume {} --budget-tokens <n>`",
            state.spent_tokens(),
            state.id
        ));
    }
    if let (Some(cap), Some(spent)) = (state.budget_usd, state.spent_usd())
        && spent >= cap
    {
        return Some(format!(
            "the cost budget of ${cap:.2} is reached (${spent:.2} spent); raise it with `anvil build resume {} --budget-usd <n>`",
            state.id
        ));
    }
    None
}

/// The tokens one session may use: an even share of what is left, and never
/// more than a session may use anyway.
fn session_share(state: &BuildState, settings: &Settings) -> u64 {
    let items_left = u64::try_from(state.remaining().max(1)).unwrap_or(1);
    let mut share = settings.session_tokens;
    if let Some(cap) = state.budget_tokens {
        share = share.min(cap.saturating_sub(state.spent_tokens()) / items_left);
    }
    if let (Some(cap), Some(price)) = (state.budget_usd, settings.price) {
        let left = cap - state.spent_usd().unwrap_or(0.0);
        let per_token = price.input.max(price.output) / 1_000_000.0;
        if per_token > 0.0 {
            // Truncation is intended: a share is a whole number of tokens.
            share = share.min((left / items_left as f64 / per_token).max(0.0) as u64);
        }
    }
    share.max(1)
}

/// The side branch that keeps a failed attempt.
fn side_branch(id: &str, number: usize, attempt: u32) -> String {
    format!("anvil-build/{id}/item-{number}-attempt-{attempt}")
}

/// Runs attempts at one item until its checks pass or its attempts are used.
///
/// Returns `None` when the item is done and the reason when it is not.
async fn run_item<E: Env>(
    env: &E,
    settings: &Settings,
    dir: &Path,
    state: &mut BuildState,
    index: usize,
    spec: &str,
) -> anyhow::Result<Option<String>> {
    let number = index + 1;
    let mut failures = 0;
    let mut previous_failure = None;
    if state.items[index].status == ItemStatus::InProgress {
        let attempt = state.items[index].attempts;
        git::save_attempt(
            &settings.cwd,
            &side_branch(&state.id, number, attempt),
            &state.branch,
            &format!(
                "{} (interrupted attempt {attempt})",
                state.items[index].item.title
            ),
        )?;
        eprintln!("the previous run stopped mid-item; its changes are on a side branch");
        failures = 1;
        previous_failure = Some("the earlier run stopped before the attempt finished".to_owned());
    }
    while failures < MAX_ATTEMPTS {
        let attempt = state.items[index].attempts + 1;
        state.items[index].attempts = attempt;
        state.items[index].status = ItemStatus::InProgress;
        state.save(dir)?;
        let task = {
            let finished: Vec<Finished<'_>> = state
                .items
                .iter()
                .filter(|item| item.status == ItemStatus::Done)
                .map(|item| Finished {
                    title: &item.item.title,
                    description: &item.item.description,
                })
                .collect();
            item_task(TaskInputs {
                number,
                total: state.items.len(),
                item: &state.items[index].item,
                spec,
                finished: &finished_summary(&finished),
                previous_failure: previous_failure.as_deref(),
            })
        };
        let request = SessionRequest {
            task,
            session_dir: session_dir(&settings.cwd, &new_session_id()),
            token_limit: session_share(state, settings),
        };
        let report = env.run_session(request).await;
        let entry = &mut state.items[index];
        entry.tokens = entry.tokens.saturating_add(report.tokens);
        entry.cost_usd = match (entry.cost_usd, report.cost_usd) {
            (Some(a), Some(b)) => Some(a + b),
            (a, b) => a.or(b),
        };
        match verify(settings, &entry.item).await {
            Ok(()) => {
                let hash = git::commit_all(&settings.cwd, &entry.item.title)?;
                entry.status = ItemStatus::Done;
                entry.commit = Some(hash);
                state.save(dir)?;
                eprintln!("work item {number} done");
                return Ok(None);
            }
            Err(reason) => {
                failures += 1;
                let session = if report.reported_done {
                    "the session reported success"
                } else {
                    "the session did not finish"
                };
                eprintln!(
                    "attempt {attempt} at work item {number} failed ({session}: {}): {reason}",
                    report.detail
                );
                git::save_attempt(
                    &settings.cwd,
                    &side_branch(&state.id, number, attempt),
                    &state.branch,
                    &format!(
                        "{} (failed attempt {attempt})",
                        state.items[index].item.title
                    ),
                )?;
                state.items[index].status = if failures >= MAX_ATTEMPTS {
                    ItemStatus::Failed
                } else {
                    ItemStatus::Pending
                };
                state.save(dir)?;
                previous_failure = Some(reason);
            }
        }
    }
    Ok(Some(
        previous_failure.unwrap_or_else(|| "no attempt ran".to_owned()),
    ))
}

/// Runs the acceptance command and then the project's own check; both must
/// exit 0. The session's own report is not consulted.
async fn verify(settings: &Settings, item: &WorkItem) -> Result<(), String> {
    let mut commands = vec![("acceptance command", item.acceptance.clone())];
    if let Some(check) = settings.check.resolve(&settings.cwd)
        && check != item.acceptance
    {
        commands.push(("project check", check));
    }
    let ctx = ToolContext {
        cwd: settings.cwd.clone(),
        session_dir: None,
        shell: settings.shell.clone(),
    };
    for (what, command) in commands {
        match shell::run_command(&ctx, &command, CHECK_TIMEOUT).await {
            Ok(run) if run.succeeded() => {}
            Ok(run) => {
                return Err(format!(
                    "the {what} `{command}` did not exit 0:\n{}",
                    tail(&run.into_output().content)
                ));
            }
            Err(error) => return Err(format!("the {what} `{command}` could not run: {error}")),
        }
    }
    Ok(())
}

/// The last lines of a command's output.
fn tail(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let from = lines.len().saturating_sub(FAILURE_TAIL_LINES);
    lines[from..].join("\n")
}

/// Prints one build's items.
///
/// # Arguments
///
/// * `cwd` - The project root.
/// * `id` - The build id; the newest build when `None`.
///
/// # Errors
///
/// When there is no such build.
pub fn print_status(cwd: &Path, id: Option<&str>) -> anyhow::Result<()> {
    let (_, state) = pick_build(cwd, id, false)?;
    print!("{}", state.report());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn side_branches_name_the_build_item_and_attempt() {
        assert_eq!(
            side_branch("01ABC", 2, 1),
            "anvil-build/01ABC/item-2-attempt-1"
        );
    }

    #[test]
    fn tail_keeps_the_last_lines() {
        let text: Vec<String> = (0..50).map(|n| format!("line {n}")).collect();
        let kept = tail(&text.join("\n"));
        assert_eq!(kept.lines().count(), FAILURE_TAIL_LINES);
        assert!(kept.ends_with("line 49"));
    }
}
