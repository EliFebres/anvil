//! `eval gate`: the regression rule the factory applies to every branch.

use std::fmt;

use crate::compare::Flips;
use crate::results::RunResult;

/// How many tasks a candidate may lose against the base and still pass.
pub const MAX_REGRESSIONS: usize = 1;

/// The gate's decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateOutcome {
    /// Whether the candidate may merge.
    pub passed: bool,
    /// Tasks passing in base and failing in the candidate.
    pub regressions: Vec<String>,
    /// Tasks failing in base and passing in the candidate.
    pub fixed: Vec<String>,
    /// Tasks in base that the candidate did not run; a warning, not a failure.
    pub not_in_candidate: Vec<String>,
}

/// Judges a candidate result against a base result.
///
/// A task passes a run when a strict majority of its attempts passed. The
/// candidate fails the gate when more than [`MAX_REGRESSIONS`] tasks that
/// passed in base fail in the candidate; tasks the candidate did not run are
/// reported but not counted.
///
/// # Arguments
///
/// * `base` - The result on trunk.
/// * `candidate` - The result on the branch.
///
/// # Returns
///
/// The outcome.
pub fn gate(base: &RunResult, candidate: &RunResult) -> GateOutcome {
    let flips = Flips::between(base, candidate);
    GateOutcome {
        passed: flips.regressed.len() <= MAX_REGRESSIONS,
        regressions: flips.regressed,
        fixed: flips.fixed,
        not_in_candidate: flips.missing,
    }
}

impl fmt::Display for GateOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let word = if self.passed { "PASS" } else { "FAIL" };
        writeln!(
            f,
            "gate: {word} ({} regression(s); up to {MAX_REGRESSIONS} allowed)",
            self.regressions.len()
        )?;
        if !self.regressions.is_empty() {
            writeln!(f, "  regressed: {}", self.regressions.join(", "))?;
        }
        if !self.fixed.is_empty() {
            writeln!(f, "  fixed:     {}", self.fixed.join(", "))?;
        }
        if !self.not_in_candidate.is_empty() {
            writeln!(
                f,
                "  warning: {} task(s) in base were not run by the candidate: {}",
                self.not_in_candidate.len(),
                self.not_in_candidate.join(", ")
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::results::samples::{run, task};

    fn base() -> RunResult {
        run(
            "base",
            "m",
            vec![
                task("a", &[true, true, true]),
                task("b", &[true, true, false]),
                task("c", &[false, false, false]),
            ],
        )
    }

    #[test]
    fn no_regressions_pass() {
        let outcome = gate(&base(), &base());
        assert!(outcome.passed);
        assert!(outcome.regressions.is_empty());
    }

    #[test]
    fn exactly_one_regression_passes() {
        let candidate = run(
            "cand",
            "m",
            vec![
                task("a", &[true, true, true]),
                task("b", &[true, false, false]),
                task("c", &[false, false, false]),
            ],
        );
        let outcome = gate(&base(), &candidate);
        assert!(outcome.passed);
        assert_eq!(outcome.regressions, vec!["b"]);
    }

    #[test]
    fn exactly_two_regressions_fail() {
        let candidate = run(
            "cand",
            "m",
            vec![
                task("a", &[false, true, false]),
                task("b", &[false, false, false]),
                task("c", &[true, true, true]),
            ],
        );
        let outcome = gate(&base(), &candidate);
        assert!(!outcome.passed);
        assert_eq!(outcome.regressions, vec!["a", "b"]);
        assert_eq!(outcome.fixed, vec!["c"]);
        assert!(
            outcome
                .to_string()
                .starts_with("gate: FAIL (2 regression(s)")
        );
    }

    #[test]
    fn missing_tasks_warn_but_do_not_count() {
        let candidate = run("cand", "m", vec![task("c", &[false, false, false])]);
        let outcome = gate(&base(), &candidate);
        assert!(outcome.passed);
        assert_eq!(outcome.not_in_candidate, vec!["a", "b"]);
        assert!(outcome.to_string().contains("warning: 2 task(s)"));
    }
}
