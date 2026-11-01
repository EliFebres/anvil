//! The states of the agent loop and the pure transition table between them.
//!
//! ```text
//! Plan -> Build -> Review -> Optimize -> Verify -> Done
//!           ^         |                    |
//!           +---------+ (review requests changes, max 3 rounds)
//!           ^                              |
//!           +------------------------------+ (verify fails, max 2 rounds)
//! ```

use std::fmt;

use anvil_types::{LimitsConfig, StateBudget};
use serde::{Deserialize, Serialize};

use crate::provider::ProviderError;

/// How many times the reviewer may send the work back.
pub const MAX_REVIEW_ROUNDS: u32 = 3;

/// How many times a failed verification may send the work back.
pub const MAX_VERIFY_ROUNDS: u32 = 2;

/// A state of the loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Explore and write the plan.
    Plan,
    /// Implement the plan.
    Build,
    /// A second model reviews the diff.
    Review,
    /// One simplification pass.
    Optimize,
    /// Run the project's checks.
    Verify,
    /// Terminal.
    Done,
}

impl State {
    /// The budget config applies to this state.
    ///
    /// # Arguments
    ///
    /// * `limits` - The `[limits]` table.
    ///
    /// # Returns
    ///
    /// The state's budget; `Done` has none and gets the default.
    pub fn budget(self, limits: &LimitsConfig) -> StateBudget {
        match self {
            State::Plan => limits.plan,
            State::Build => limits.build,
            State::Review => limits.review,
            State::Optimize => limits.optimize,
            State::Verify => limits.verify,
            State::Done => StateBudget::default(),
        }
    }

    /// The lowercase name used in prompts and logs.
    ///
    /// # Returns
    ///
    /// The name.
    pub fn name(self) -> &'static str {
        match self {
            State::Plan => "plan",
            State::Build => "build",
            State::Review => "review",
            State::Optimize => "optimize",
            State::Verify => "verify",
            State::Done => "done",
        }
    }
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Why a run ended without finishing the task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "reason")]
pub enum IncompleteReason {
    /// A state used all of its turns.
    TurnBudget {
        /// The state.
        state: State,
    },
    /// A state used all of its tokens.
    TokenBudget {
        /// The state.
        state: State,
    },
    /// The whole session used its token budget.
    SessionBudget,
    /// The reviewer sent the work back too many times.
    ReviewRoundsExhausted,
    /// Verification failed too many times.
    VerifyRoundsExhausted,
    /// The user rejected the plan.
    PlanRejected,
    /// The last turn in a state produced no usable result.
    Unexpected {
        /// What was wrong.
        detail: String,
    },
}

impl fmt::Display for IncompleteReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IncompleteReason::TurnBudget { state } => {
                write!(f, "the {state} state used its turn budget")
            }
            IncompleteReason::TokenBudget { state } => {
                write!(f, "the {state} state used its token budget")
            }
            IncompleteReason::SessionBudget => f.write_str("the session used its token budget"),
            IncompleteReason::ReviewRoundsExhausted => write!(
                f,
                "the reviewer requested changes {MAX_REVIEW_ROUNDS} times; giving up"
            ),
            IncompleteReason::VerifyRoundsExhausted => write!(
                f,
                "verification failed {} times; giving up",
                MAX_VERIFY_ROUNDS + 1
            ),
            IncompleteReason::PlanRejected => f.write_str("the plan was rejected"),
            IncompleteReason::Unexpected { detail } => f.write_str(detail),
        }
    }
}

/// How a run ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum Status {
    /// The task was built, reviewed, simplified and verified.
    Done,
    /// The task was not finished.
    Incomplete {
        /// Why.
        reason: IncompleteReason,
    },
    /// The task was not finished and a tool call had been denied along the way.
    Denied {
        /// Why the run stopped.
        reason: IncompleteReason,
    },
    /// A provider failed and the run stopped.
    ProviderError {
        /// The failure.
        error: ProviderError,
    },
}

impl Status {
    /// The process exit code the CLI uses for this status.
    ///
    /// # Returns
    ///
    /// 0 done, 2 incomplete, 3 denied, 4 provider error.
    pub fn exit_code(&self) -> i32 {
        match self {
            Status::Done => 0,
            Status::Incomplete { .. } => 2,
            Status::Denied { .. } => 3,
            Status::ProviderError { .. } => 4,
        }
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Status::Done => f.write_str("done"),
            Status::Incomplete { reason } => write!(f, "incomplete: {reason}"),
            Status::Denied { reason } => {
                write!(f, "incomplete after a denied tool call: {reason}")
            }
            Status::ProviderError { error } => write!(f, "provider error: {error}"),
        }
    }
}

/// What a state produced, which decides the next state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateOutcome {
    /// The state finished its work.
    Finished,
    /// The reviewer approved the diff.
    ReviewApproved,
    /// The reviewer wants changes.
    ReviewChanges(Vec<String>),
    /// Verification passed.
    VerifyPassed,
    /// Verification failed with this output.
    VerifyFailed(String),
    /// The state ran out of budget, or the run must stop for another reason.
    Stop(IncompleteReason),
    /// A provider failed.
    ProviderFailed(ProviderError),
}

/// How many times each retry edge has been taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Rounds {
    /// Review -> Build edges taken.
    pub review: u32,
    /// Verify -> Build edges taken.
    pub verify: u32,
}

/// Where the loop goes next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Enter this state.
    Next(State),
    /// Stop with this status.
    Finish(Status),
}

/// The transition table.
///
/// # Arguments
///
/// * `state` - The state that just ran.
/// * `outcome` - What it produced.
/// * `rounds` - Retry counters, incremented when a retry edge is taken.
///
/// # Returns
///
/// The next step.
pub fn transition(state: State, outcome: StateOutcome, rounds: &mut Rounds) -> Step {
    match (state, outcome) {
        (_, StateOutcome::Stop(reason)) => Step::Finish(Status::Incomplete { reason }),
        (_, StateOutcome::ProviderFailed(error)) => Step::Finish(Status::ProviderError { error }),
        (State::Plan, StateOutcome::Finished) => Step::Next(State::Build),
        (State::Build, StateOutcome::Finished) => Step::Next(State::Review),
        (State::Review, StateOutcome::ReviewApproved) => Step::Next(State::Optimize),
        (State::Review, StateOutcome::ReviewChanges(_)) => {
            if rounds.review < MAX_REVIEW_ROUNDS {
                rounds.review += 1;
                Step::Next(State::Build)
            } else {
                Step::Finish(Status::Incomplete {
                    reason: IncompleteReason::ReviewRoundsExhausted,
                })
            }
        }
        (State::Optimize, StateOutcome::Finished) => Step::Next(State::Verify),
        (State::Verify, StateOutcome::VerifyPassed) => Step::Finish(Status::Done),
        (State::Verify, StateOutcome::VerifyFailed(_)) => {
            if rounds.verify < MAX_VERIFY_ROUNDS {
                rounds.verify += 1;
                Step::Next(State::Build)
            } else {
                Step::Finish(Status::Incomplete {
                    reason: IncompleteReason::VerifyRoundsExhausted,
                })
            }
        }
        (state, outcome) => {
            debug_assert!(false, "no transition from {state} on {outcome:?}");
            Step::Finish(Status::Incomplete {
                reason: IncompleteReason::Unexpected {
                    detail: format!("no transition from {state} on {outcome:?}"),
                },
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn incomplete(reason: IncompleteReason) -> Step {
        Step::Finish(Status::Incomplete { reason })
    }

    #[test]
    fn happy_path() {
        let mut rounds = Rounds::default();
        assert_eq!(
            transition(State::Plan, StateOutcome::Finished, &mut rounds),
            Step::Next(State::Build)
        );
        assert_eq!(
            transition(State::Build, StateOutcome::Finished, &mut rounds),
            Step::Next(State::Review)
        );
        assert_eq!(
            transition(State::Review, StateOutcome::ReviewApproved, &mut rounds),
            Step::Next(State::Optimize)
        );
        assert_eq!(
            transition(State::Optimize, StateOutcome::Finished, &mut rounds),
            Step::Next(State::Verify)
        );
        assert_eq!(
            transition(State::Verify, StateOutcome::VerifyPassed, &mut rounds),
            Step::Finish(Status::Done)
        );
        assert_eq!(rounds, Rounds::default());
    }

    #[test]
    fn review_changes_loop_back_three_times_then_stop() {
        let mut rounds = Rounds::default();
        for expected in 1..=MAX_REVIEW_ROUNDS {
            assert_eq!(
                transition(
                    State::Review,
                    StateOutcome::ReviewChanges(vec!["x".to_owned()]),
                    &mut rounds
                ),
                Step::Next(State::Build)
            );
            assert_eq!(rounds.review, expected);
        }
        assert_eq!(
            transition(
                State::Review,
                StateOutcome::ReviewChanges(vec![]),
                &mut rounds
            ),
            incomplete(IncompleteReason::ReviewRoundsExhausted)
        );
    }

    #[test]
    fn verify_failures_loop_back_twice_then_stop() {
        let mut rounds = Rounds::default();
        for expected in 1..=MAX_VERIFY_ROUNDS {
            assert_eq!(
                transition(
                    State::Verify,
                    StateOutcome::VerifyFailed("boom".to_owned()),
                    &mut rounds
                ),
                Step::Next(State::Build)
            );
            assert_eq!(rounds.verify, expected);
        }
        assert_eq!(
            transition(
                State::Verify,
                StateOutcome::VerifyFailed(String::new()),
                &mut rounds
            ),
            incomplete(IncompleteReason::VerifyRoundsExhausted)
        );
    }

    #[test]
    fn stops_and_provider_failures_end_from_any_state() {
        for state in [
            State::Plan,
            State::Build,
            State::Review,
            State::Optimize,
            State::Verify,
        ] {
            let mut rounds = Rounds::default();
            assert_eq!(
                transition(
                    state,
                    StateOutcome::Stop(IncompleteReason::TurnBudget { state }),
                    &mut rounds
                ),
                incomplete(IncompleteReason::TurnBudget { state })
            );
            assert_eq!(
                transition(
                    state,
                    StateOutcome::ProviderFailed(ProviderError::Timeout { after_s: 1 }),
                    &mut rounds
                ),
                Step::Finish(Status::ProviderError {
                    error: ProviderError::Timeout { after_s: 1 }
                })
            );
        }
    }

    #[test]
    fn exit_codes_and_messages() {
        assert_eq!(Status::Done.exit_code(), 0);
        let reason = IncompleteReason::SessionBudget;
        assert_eq!(
            Status::Incomplete {
                reason: reason.clone()
            }
            .exit_code(),
            2
        );
        assert_eq!(Status::Denied { reason }.exit_code(), 3);
        assert_eq!(
            Status::ProviderError {
                error: ProviderError::Timeout { after_s: 1 }
            }
            .exit_code(),
            4
        );
        assert_eq!(
            Status::Incomplete {
                reason: IncompleteReason::TurnBudget {
                    state: State::Build
                }
            }
            .to_string(),
            "incomplete: the build state used its turn budget"
        );
    }

    #[test]
    fn budgets_come_from_config() {
        let mut limits = LimitsConfig::default();
        limits.build.max_turns = 3;
        assert_eq!(State::Build.budget(&limits).max_turns, 3);
        assert_eq!(State::Plan.budget(&limits), StateBudget::default());
        assert_eq!(State::Verify.to_string(), "verify");
    }
}
