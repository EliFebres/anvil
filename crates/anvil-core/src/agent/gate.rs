//! The plan approval hook. The CLI shows the plan and asks; `--yes` auto-approves.

use std::collections::VecDeque;
use std::sync::Mutex;

use crate::provider::lock_or_recover;

/// What the user said about the plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanDecision {
    /// Go ahead.
    Approve,
    /// Plan again with this feedback.
    Revise(String),
    /// Stop the run.
    Abort,
}

/// Decides whether a plan may be built.
pub trait PlanGate: Send + Sync {
    /// Reviews one plan.
    ///
    /// # Arguments
    ///
    /// * `plan` - The plan text.
    ///
    /// # Returns
    ///
    /// The decision.
    fn review(&self, plan: &str) -> PlanDecision;
}

/// Approves every plan, as `--yes` does.
#[derive(Debug, Clone, Copy, Default)]
pub struct AutoApprove;

impl PlanGate for AutoApprove {
    fn review(&self, _plan: &str) -> PlanDecision {
        PlanDecision::Approve
    }
}

/// Replays scripted decisions and records the plans it saw. Approves once
/// the script is exhausted.
#[derive(Debug)]
pub struct ScriptedGate {
    /// Decisions not yet given.
    decisions: Mutex<VecDeque<PlanDecision>>,
    /// Every plan reviewed.
    plans: Mutex<Vec<String>>,
}

impl ScriptedGate {
    /// Creates the gate.
    ///
    /// # Arguments
    ///
    /// * `decisions` - Decisions in the order they will be given.
    ///
    /// # Returns
    ///
    /// The gate.
    pub fn new(decisions: Vec<PlanDecision>) -> Self {
        Self {
            decisions: Mutex::new(decisions.into()),
            plans: Mutex::new(Vec::new()),
        }
    }

    /// Every plan reviewed so far.
    ///
    /// # Returns
    ///
    /// Clones, oldest first.
    pub fn plans(&self) -> Vec<String> {
        lock_or_recover(&self.plans).clone()
    }
}

impl PlanGate for ScriptedGate {
    fn review(&self, plan: &str) -> PlanDecision {
        lock_or_recover(&self.plans).push(plan.to_owned());
        lock_or_recover(&self.decisions)
            .pop_front()
            .unwrap_or(PlanDecision::Approve)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripted_gate_replays_then_approves() {
        let gate = ScriptedGate::new(vec![PlanDecision::Revise("more".to_owned())]);
        assert_eq!(gate.review("v1"), PlanDecision::Revise("more".to_owned()));
        assert_eq!(gate.review("v2"), PlanDecision::Approve);
        assert_eq!(gate.plans(), vec!["v1".to_owned(), "v2".to_owned()]);
        assert_eq!(AutoApprove.review("x"), PlanDecision::Approve);
    }
}
