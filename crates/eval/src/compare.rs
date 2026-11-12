//! `eval compare`: two results side by side, and which tasks flipped.

use std::collections::BTreeSet;
use std::fmt;

use crate::results::{
    Efficiency, OverheadTotals, RunResult, TaskResult, money, percent, thousands,
};

/// Which tasks changed verdict between two runs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Flips {
    /// Tasks failing in the first run and passing in the second.
    pub fixed: Vec<String>,
    /// Tasks passing in the first run and failing in the second.
    pub regressed: Vec<String>,
    /// Tasks in the first run that the second run did not include.
    pub missing: Vec<String>,
}

impl Flips {
    /// Compares the per-task verdicts of two runs.
    ///
    /// Only tasks present in both runs can flip; a task the second run did
    /// not include is listed under `missing` instead.
    ///
    /// # Arguments
    ///
    /// * `a` - The earlier run.
    /// * `b` - The later run.
    ///
    /// # Returns
    ///
    /// The flips, each list sorted by task name.
    pub fn between(a: &RunResult, b: &RunResult) -> Flips {
        let mut flips = Flips::default();
        for task in &a.tasks {
            let Some(other) = b.task(&task.name) else {
                flips.missing.push(task.name.clone());
                continue;
            };
            match (task.is_passing(), other.is_passing()) {
                (false, true) => flips.fixed.push(task.name.clone()),
                (true, false) => flips.regressed.push(task.name.clone()),
                _ => {}
            }
        }
        flips.fixed.sort();
        flips.regressed.sort();
        flips.missing.sort();
        flips
    }
}

/// The totals of one run, as the comparison table shows them.
#[derive(Debug, Clone, PartialEq)]
pub struct RunSummary {
    /// `<sha> <model>`.
    pub label: String,
    /// Tasks passing by majority.
    pub tasks_passing: u32,
    /// Tasks run.
    pub tasks: u32,
    /// Attempt pass rate.
    pub pass_rate: f64,
    /// Tokens over the run.
    pub tokens: u64,
    /// Cost over the run, when known.
    pub cost_usd: Option<f64>,
    /// Cost per passing task, when known.
    pub cost_per_passed_task: Option<f64>,
    /// What the run spent per attempt.
    pub efficiency: Efficiency,
    /// Each role with its model; empty when the run used no more than the coder.
    pub roles: String,
}

impl From<&RunResult> for RunSummary {
    fn from(run: &RunResult) -> Self {
        RunSummary {
            label: run.label(),
            tasks_passing: run.totals.tasks_passing,
            tasks: run.totals.tasks,
            pass_rate: run.totals.pass_rate,
            tokens: run.totals.tokens.total(),
            cost_usd: run.totals.cost_usd,
            cost_per_passed_task: run.totals.cost_per_passed_task,
            efficiency: Efficiency::of(&run.tasks),
            roles: run.roles_label(),
        }
    }
}

/// One task in both runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompareRow {
    /// The task name.
    pub task: String,
    /// The first run's verdict, or `None` when it did not include the task.
    pub a: Option<String>,
    /// The second run's verdict, or `None` when it did not include the task.
    pub b: Option<String>,
}

/// Two runs side by side.
#[derive(Debug, Clone, PartialEq)]
pub struct Comparison {
    /// The first run's totals.
    pub a: RunSummary,
    /// The second run's totals.
    pub b: RunSummary,
    /// Every task either run included, sorted by name.
    pub rows: Vec<CompareRow>,
    /// Which tasks flipped.
    pub flips: Flips,
    /// The figures split into small and large tasks, when either run has a
    /// task tagged [`LARGE_TAG`]; empty otherwise.
    pub groups: Vec<Group>,
}

/// The tag that puts a task in the large group.
pub const LARGE_TAG: &str = "large";

/// One group of tasks in both runs.
#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    /// `small` or `large`.
    pub name: &'static str,
    /// The first run's figures for the group.
    pub a: Efficiency,
    /// The second run's figures for the group.
    pub b: Efficiency,
}

/// Compares two results.
///
/// # Arguments
///
/// * `a` - The first run.
/// * `b` - The second run.
///
/// # Returns
///
/// The comparison; print it with `Display`.
pub fn compare(a: &RunResult, b: &RunResult) -> Comparison {
    let names: BTreeSet<&str> = a
        .tasks
        .iter()
        .chain(&b.tasks)
        .map(|t| t.name.as_str())
        .collect();
    let rows = names
        .into_iter()
        .map(|name| CompareRow {
            task: name.to_owned(),
            a: a.task(name).map(|t| t.verdict()),
            b: b.task(name).map(|t| t.verdict()),
        })
        .collect();
    Comparison {
        a: RunSummary::from(a),
        b: RunSummary::from(b),
        rows,
        flips: Flips::between(a, b),
        groups: groups(a, b),
    }
}

impl fmt::Display for Comparison {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name_width = self
            .rows
            .iter()
            .map(|r| r.task.len())
            .max()
            .unwrap_or(0)
            .max("cost per passed task".len());
        let col = self.a.label.len().max(self.b.label.len()).max(10);
        writeln!(
            f,
            "{:<name_width$}  {:<col$}  {}",
            "task", self.a.label, self.b.label
        )?;
        writeln!(
            f,
            "{}  {}  {}",
            "-".repeat(name_width),
            "-".repeat(col),
            "-".repeat(col)
        )?;
        for row in &self.rows {
            let mark = if self.flips.fixed.contains(&row.task) {
                "  fixed"
            } else if self.flips.regressed.contains(&row.task) {
                "  regressed"
            } else {
                ""
            };
            let b = row.b.as_deref().unwrap_or("-");
            let b = if mark.is_empty() {
                b.to_owned()
            } else {
                format!("{b:<col$}")
            };
            writeln!(
                f,
                "{:<name_width$}  {:<col$}  {b}{mark}",
                row.task,
                row.a.as_deref().unwrap_or("-"),
            )?;
        }
        writeln!(f)?;
        let mut totals = |label: &str, a: String, b: String| -> fmt::Result {
            writeln!(f, "{label:<name_width$}  {a:<col$}  {b}")
        };
        if !self.a.roles.is_empty() || !self.b.roles.is_empty() {
            totals("roles", self.a.roles.clone(), self.b.roles.clone())?;
        }
        totals(
            "tasks passing",
            format!("{}/{}", self.a.tasks_passing, self.a.tasks),
            format!("{}/{}", self.b.tasks_passing, self.b.tasks),
        )?;
        totals(
            "pass rate",
            percent(self.a.pass_rate),
            percent(self.b.pass_rate),
        )?;
        totals("tokens", thousands(self.a.tokens), thousands(self.b.tokens))?;
        totals("cost (USD)", money(self.a.cost_usd), money(self.b.cost_usd))?;
        totals(
            "cost per passed task",
            money(self.a.cost_per_passed_task),
            money(self.b.cost_per_passed_task),
        )?;
        write_efficiency(f, name_width, col, &self.a.efficiency, &self.b.efficiency)?;
        for group in &self.groups {
            writeln!(f)?;
            writeln!(f, "{} tasks", group.name)?;
            write_efficiency(f, name_width, col, &group.a, &group.b)?;
        }
        writeln!(f)?;
        writeln!(
            f,
            "input tokens / passed task: {}",
            change(
                self.a.efficiency.input_per_passed,
                self.b.efficiency.input_per_passed
            )
        )?;
        writeln!(
            f,
            "flipped: {} fixed, {} regressed",
            self.flips.fixed.len(),
            self.flips.regressed.len()
        )?;
        if !self.flips.fixed.is_empty() {
            writeln!(f, "  fixed:     {}", self.flips.fixed.join(", "))?;
        }
        if !self.flips.regressed.is_empty() {
            writeln!(f, "  regressed: {}", self.flips.regressed.join(", "))?;
        }
        if !self.flips.missing.is_empty() {
            writeln!(f, "  not in second run: {}", self.flips.missing.join(", "))?;
        }
        Ok(())
    }
}

/// Small and large groups, or none when neither run has a large task.
fn groups(a: &RunResult, b: &RunResult) -> Vec<Group> {
    let is_large = |task: &&TaskResult| task.tags.iter().any(|tag| tag == LARGE_TAG);
    let split = |run: &RunResult| -> (Vec<TaskResult>, Vec<TaskResult>) {
        run.tasks.iter().cloned().partition(|task| !is_large(&task))
    };
    let (a_small, a_large) = split(a);
    let (b_small, b_large) = split(b);
    if a_large.is_empty() && b_large.is_empty() {
        return Vec::new();
    }
    vec![
        Group {
            name: "small",
            a: Efficiency::of(&a_small),
            b: Efficiency::of(&b_small),
        },
        Group {
            name: "large",
            a: Efficiency::of(&a_large),
            b: Efficiency::of(&b_large),
        },
    ]
}

/// Writes the per-attempt figures of two runs, with the change between them.
fn write_efficiency(
    f: &mut fmt::Formatter<'_>,
    name_width: usize,
    col: usize,
    a: &Efficiency,
    b: &Efficiency,
) -> fmt::Result {
    let rows: Vec<(&str, Option<f64>, Option<f64>, usize)> = vec![
        (
            "attempts",
            Some(a.attempts as f64),
            Some(b.attempts as f64),
            0,
        ),
        (
            "pass rate %",
            Some(a.pass_rate * 100.0),
            Some(b.pass_rate * 100.0),
            1,
        ),
        ("turns / task", Some(a.turns_mean), Some(b.turns_mean), 1),
        ("context / turn", a.context_per_turn, b.context_per_turn, 0),
        (
            "input / task mean",
            Some(a.input_mean),
            Some(b.input_mean),
            0,
        ),
        (
            "input / task median",
            Some(a.input_median),
            Some(b.input_median),
            0,
        ),
        (
            "uncached / task",
            Some(a.uncached_input_mean),
            Some(b.uncached_input_mean),
            0,
        ),
        (
            "input / passed task",
            a.input_per_passed,
            b.input_per_passed,
            0,
        ),
        (
            "tool calls / task",
            Some(a.tool_calls_mean),
            Some(b.tool_calls_mean),
            1,
        ),
        (
            "2+ call turns / task",
            Some(a.multi_call_turns_mean),
            Some(b.multi_call_turns_mean),
            1,
        ),
        ("cost per task", a.cost_per_attempt, b.cost_per_attempt, 4),
    ];
    for (label, a, b, digits) in rows {
        let shown =
            |v: Option<f64>| v.map_or_else(|| "n/a".to_owned(), |v| format!("{v:.digits$}"));
        writeln!(
            f,
            "{label:<name_width$}  {:<col$}  {:<col$}  {}",
            shown(a),
            shown(b),
            change(a, b)
        )?;
    }
    write_states(f, name_width, a, b)?;
    write_overheads(f, name_width, col, a, b)
}

/// Reads one count out of a run's overhead totals.
type Count = fn(&OverheadTotals) -> u64;

/// Writes the loop overheads of two runs: counts over the attempts that
/// recorded them, `n/a` for a run written before they were recorded, and
/// nothing when neither run has them.
fn write_overheads(
    f: &mut fmt::Formatter<'_>,
    name_width: usize,
    col: usize,
    a: &Efficiency,
    b: &Efficiency,
) -> fmt::Result {
    let (a, b) = (a.overheads.as_ref(), b.overheads.as_ref());
    if a.is_none() && b.is_none() {
        return Ok(());
    }
    let skipped = |o: &OverheadTotals| o.memory_skipped.values().sum::<u64>();
    let rows: Vec<(&str, Count)> = vec![
        ("attempts recorded", |o| o.attempts as u64),
        ("lone tick turns", |o| o.lone_tick_turns),
        ("reviews", |o| o.review_rounds),
        ("review cap hits", |o| o.review_cap_hits),
        ("verify model turns", |o| o.verify_model_turns),
        ("check detected", |o| o.check_detected),
        ("check from plan", |o| o.check_plan),
        ("check none", |o| o.check_none),
        ("memory ran", |o| o.memory_ran),
    ];
    let shown = |v: Option<u64>| v.map_or_else(|| "n/a".to_owned(), |v| v.to_string());
    writeln!(f, "{:<name_width$}  totals over the attempts", "overheads")?;
    for (label, value) in rows {
        let label = format!("  {label}");
        writeln!(
            f,
            "{label:<name_width$}  {:<col$}  {}",
            shown(a.map(value)),
            shown(b.map(value))
        )?;
    }
    writeln!(
        f,
        "{:<name_width$}  {:<col$}  {}",
        "  memory skipped",
        shown(a.map(skipped)),
        shown(b.map(skipped))
    )?;
    let mut reasons: Vec<&String> = a
        .iter()
        .chain(b.iter())
        .flat_map(|o| o.memory_skipped.keys())
        .collect();
    reasons.sort();
    reasons.dedup();
    for reason in reasons {
        let count = |o: Option<&OverheadTotals>| {
            o.map(|o| o.memory_skipped.get(reason).copied().unwrap_or(0))
        };
        let label = format!("    {reason}");
        writeln!(
            f,
            "{label:<name_width$}  {:<col$}  {}",
            shown(count(a)),
            shown(count(b))
        )?;
    }
    Ok(())
}

/// The loop states in the order a session visits them; others sort after.
// `optimize` is a state the loop no longer has; results recorded while it
// existed still print it in its old place.
const STATE_ORDER: [&str; 6] = ["plan", "build", "review", "optimize", "verify", "memory"];

/// Writes turns per task for each loop state, split into one-call, 2+ call,
/// text and verdict turns, with the prompt tokens per turn of that state.
fn write_states(
    f: &mut fmt::Formatter<'_>,
    name_width: usize,
    a: &Efficiency,
    b: &Efficiency,
) -> fmt::Result {
    let mut states: Vec<&String> = a.by_state.keys().chain(b.by_state.keys()).collect();
    states.sort_by_key(|state| {
        let rank = STATE_ORDER.iter().position(|known| known == state);
        (rank.unwrap_or(STATE_ORDER.len()), state.as_str())
    });
    states.dedup();
    if states.is_empty() {
        return Ok(());
    }
    writeln!(
        f,
        "{:<name_width$}  turns/task (1 call, 2+ calls, text, verdict; context/turn)",
        "state"
    )?;
    for state in states {
        let total = |e: &Efficiency| {
            e.by_state
                .get(state)
                .map(|s| f64::from(s.total()) / e.attempts.max(1) as f64)
        };
        writeln!(
            f,
            "{:<name_width$}  {}  |  {}  {}",
            format!("  {state}"),
            state_cell(a, state),
            state_cell(b, state),
            change(total(a), total(b))
        )?;
    }
    Ok(())
}

/// `6.1 (3.0, 2.0, 1.1, 0.0; 8,400)` for one state of one run, or `-`.
fn state_cell(e: &Efficiency, state: &str) -> String {
    let Some(s) = e.by_state.get(state) else {
        return "-".to_owned();
    };
    let per = |n: u32| f64::from(n) / e.attempts.max(1) as f64;
    let context = match s.total() {
        0 => 0,
        turns => s.prompt_tokens / u64::from(turns),
    };
    format!(
        "{:.1} ({:.1}, {:.1}, {:.1}, {:.1}; {})",
        per(s.total()),
        per(s.one_call),
        per(s.multi_call),
        per(s.text),
        per(s.verdict),
        thousands(context)
    )
}

/// The relative change from `a` to `b` as `-31.2%`, or `n/a`.
fn change(a: Option<f64>, b: Option<f64>) -> String {
    match (a, b) {
        (Some(a), Some(b)) if a > 0.0 => format!("{:+.1}%", (b - a) / a * 100.0),
        _ => "n/a".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::results::samples::{run, task};

    #[test]
    fn flips_only_count_tasks_in_both_runs() {
        let a = run(
            "aaa",
            "m1",
            vec![
                task("fixed-later", &[false, false, false]),
                task("broken-later", &[true, true, true]),
                task("stable", &[true, true, false]),
                task("dropped", &[true, true, true]),
            ],
        );
        let b = run(
            "bbb",
            "m2",
            vec![
                task("fixed-later", &[true, true, false]),
                task("broken-later", &[true, false, false]),
                task("stable", &[true, true, true]),
                task("new", &[true, true, true]),
            ],
        );
        let flips = Flips::between(&a, &b);
        assert_eq!(flips.fixed, vec!["fixed-later"]);
        assert_eq!(flips.regressed, vec!["broken-later"]);
        assert_eq!(flips.missing, vec!["dropped"]);
        let comparison = compare(&a, &b);
        assert_eq!(comparison.rows.len(), 5);
        assert_eq!(comparison.rows[0].task, "broken-later");
        assert_eq!(
            comparison
                .rows
                .iter()
                .find(|r| r.task == "new")
                .map(|r| r.a.clone()),
            Some(None)
        );
    }

    #[test]
    fn large_tagged_tasks_get_their_own_group() {
        let small = || task("py-small", &[true, true, true]);
        let a = run("aaa", "m1", vec![small()]);
        assert!(compare(&a, &a).groups.is_empty());

        let mut large = task("large-rename", &[true, false, false]);
        large.tags.push(LARGE_TAG.to_owned());
        let b = run("bbb", "m1", vec![small(), large]);
        let comparison = compare(&a, &b);
        let names: Vec<&str> = comparison.groups.iter().map(|g| g.name).collect();
        assert_eq!(names, ["small", "large"]);
        assert_eq!(comparison.groups[0].b.attempts, 3);
        assert_eq!(comparison.groups[1].a.attempts, 0);
        assert!((comparison.groups[1].b.pass_rate - 1.0 / 3.0).abs() < 1e-9);
        assert!(comparison.to_string().contains("large tasks"));
    }

    #[test]
    fn a_schema_one_and_a_schema_two_result_compare() {
        let mut old = run("aaa", "m-old", vec![task("t", &[true, true, true])]);
        old.schema = 1;
        let mut new = run("bbb", "m-new", vec![task("t", &[true, false, true])]);
        let served = |model: &str| crate::results::RoleModel {
            provider: "p".to_owned(),
            model: model.to_owned(),
        };
        new.roles = [
            ("planner".to_owned(), served("dear")),
            ("coder".to_owned(), served("m-new")),
        ]
        .into();
        let text = compare(&old, &new).to_string();
        assert!(text.contains("roles"), "{text}");
        assert!(text.contains("coder m-new, planner dear"), "{text}");
        let plain = compare(&old, &old).to_string();
        assert!(!plain.contains("roles"), "{plain}");
    }

    #[test]
    fn table_is_stable() {
        let a = run(
            "fe92bc2",
            "claude-opus-5-5",
            vec![
                task("md-fix-readme-links", &[true, true, true]),
                task("py-fix-failing-pytest", &[true, true, true]),
                task("rs-thiserror-enum", &[false, false, false]),
            ],
        );
        let mut b = run(
            "a1b2c3d",
            "gpt-6-astra",
            vec![
                task("md-fix-readme-links", &[true, true, true]),
                task("py-fix-failing-pytest", &[true, false, false]),
                task("rs-thiserror-enum", &[true, true, false]),
            ],
        );
        b.totals.cost_usd = None;
        b.totals.cost_per_passed_task = None;
        let mut build = crate::results::StateTurns::default();
        build.count("build", 1, 900);
        build.count("build", 2, 1100);
        let mut review = crate::results::StateTurns::default();
        review.count("review", 0, 300);
        for attempt in b.tasks.iter_mut().flat_map(|t| &mut t.attempts) {
            attempt.turns_by_state =
                [("review".to_owned(), review), ("build".to_owned(), build)].into();
            attempt.overheads = Some(crate::results::Overheads {
                lone_tick_turns: 1,
                review_rounds: 2,
                review_cap_hit: attempt.index == 3,
                verify_model_turns: 0,
                check_source: if attempt.index == 1 {
                    crate::results::CheckSource::Plan
                } else {
                    crate::results::CheckSource::Detected
                },
                memory_ran: attempt.index == 1,
                memory_skip_reason: (attempt.index != 1)
                    .then(|| "uneventful_small_diff".to_owned()),
            });
        }
        insta::assert_snapshot!(compare(&a, &b).to_string());
    }
}
