//! `eval compare`: two results side by side, and which tasks flipped.

use std::collections::BTreeSet;
use std::fmt;

use crate::results::{RunResult, money, percent, thousands};

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
        writeln!(f)?;
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
        insta::assert_snapshot!(compare(&a, &b).to_string());
    }
}
