//! Per-ticket budgets: wall clock, tokens, Gate A attempts and review rounds.

use std::fmt;
use std::time::{Duration, Instant};

use anvil_types::BudgetConfig;

/// Implementer runs allowed to pass Gate A: the first and one retry.
pub const MAX_GATE_A_ATTEMPTS: u32 = 2;

/// Times the swarm reviews a ticket: once, and once more after `changes`.
pub const MAX_REVIEW_ROUNDS: u32 = 2;

/// Which budget ran out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BudgetExceeded {
    /// The ticket took longer than allowed.
    WallClock {
        /// The limit.
        limit: Duration,
    },
    /// The implementer used more tokens than allowed.
    Tokens {
        /// Tokens used so far.
        used: u64,
        /// The limit.
        limit: u64,
    },
    /// Gate A failed once too often.
    Attempts {
        /// The limit.
        limit: u32,
    },
    /// The swarm asked for changes once too often.
    ReviewRounds {
        /// The limit.
        limit: u32,
    },
}

impl fmt::Display for BudgetExceeded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BudgetExceeded::WallClock { limit } => write!(
                f,
                "wall clock budget of {} minutes exceeded",
                limit.as_secs() / 60
            ),
            BudgetExceeded::Tokens { used, limit } => {
                write!(f, "token budget exceeded: {used} used, {limit} allowed")
            }
            BudgetExceeded::Attempts { limit } => write!(
                f,
                "`just check` failed on every one of the {limit} implementer attempts"
            ),
            BudgetExceeded::ReviewRounds { limit } => write!(
                f,
                "the review swarm asked for changes after all {limit} review rounds"
            ),
        }
    }
}

/// What a ticket may still spend.
#[derive(Debug, Clone)]
pub struct Budget {
    started: Instant,
    wall_clock: Duration,
    token_limit: u64,
    tokens_used: u64,
    gate_a_failures: u32,
    review_rounds: u32,
}

impl Budget {
    /// A fresh budget from `[budget]` in `factory.toml`.
    ///
    /// # Arguments
    ///
    /// * `now` - When the ticket was claimed.
    /// * `config` - The limits.
    ///
    /// # Returns
    ///
    /// The budget.
    pub fn new(now: Instant, config: &BudgetConfig) -> Self {
        Self {
            started: now,
            wall_clock: Duration::from_secs(config.wall_clock_minutes * 60),
            token_limit: config.tokens,
            tokens_used: 0,
            gate_a_failures: 0,
            review_rounds: 0,
        }
    }

    /// When the wall clock runs out.
    pub fn deadline(&self) -> Instant {
        self.started + self.wall_clock
    }

    /// Tokens used so far.
    pub fn tokens_used(&self) -> u64 {
        self.tokens_used
    }

    /// Fails once the wall clock has run out.
    ///
    /// # Arguments
    ///
    /// * `now` - The current time.
    ///
    /// # Errors
    ///
    /// [`BudgetExceeded::WallClock`].
    pub fn check_time(&self, now: Instant) -> Result<(), BudgetExceeded> {
        if now >= self.deadline() {
            return Err(BudgetExceeded::WallClock {
                limit: self.wall_clock,
            });
        }
        Ok(())
    }

    /// Counts tokens and fails once the limit is passed.
    ///
    /// # Arguments
    ///
    /// * `tokens` - Tokens one run used.
    ///
    /// # Errors
    ///
    /// [`BudgetExceeded::Tokens`].
    pub fn add_tokens(&mut self, tokens: u64) -> Result<(), BudgetExceeded> {
        self.tokens_used += tokens;
        if self.tokens_used > self.token_limit {
            return Err(BudgetExceeded::Tokens {
                used: self.tokens_used,
                limit: self.token_limit,
            });
        }
        Ok(())
    }

    /// Records a Gate A failure; fails when no retry is left.
    ///
    /// # Errors
    ///
    /// [`BudgetExceeded::Attempts`] on the [`MAX_GATE_A_ATTEMPTS`]th failure.
    pub fn gate_a_failed(&mut self) -> Result<(), BudgetExceeded> {
        self.gate_a_failures += 1;
        if self.gate_a_failures >= MAX_GATE_A_ATTEMPTS {
            return Err(BudgetExceeded::Attempts {
                limit: MAX_GATE_A_ATTEMPTS,
            });
        }
        Ok(())
    }

    /// Records a review that asked for changes; fails when no round is left.
    ///
    /// # Errors
    ///
    /// [`BudgetExceeded::ReviewRounds`] on the [`MAX_REVIEW_ROUNDS`]th such review.
    pub fn review_asked_for_changes(&mut self) -> Result<(), BudgetExceeded> {
        self.review_rounds += 1;
        if self.review_rounds >= MAX_REVIEW_ROUNDS {
            return Err(BudgetExceeded::ReviewRounds {
                limit: MAX_REVIEW_ROUNDS,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> BudgetConfig {
        BudgetConfig {
            wall_clock_minutes: 90,
            tokens: 1000,
        }
    }

    #[test]
    fn wall_clock_runs_out_at_the_deadline() {
        let start = Instant::now();
        let budget = Budget::new(start, &config());
        assert!(budget.check_time(start).is_ok());
        assert!(
            budget
                .check_time(start + Duration::from_secs(89 * 60))
                .is_ok()
        );
        let error = budget
            .check_time(start + Duration::from_secs(90 * 60))
            .expect_err("over");
        assert_eq!(
            error.to_string(),
            "wall clock budget of 90 minutes exceeded"
        );
    }

    #[test]
    fn tokens_accumulate_across_runs() {
        let mut budget = Budget::new(Instant::now(), &config());
        budget.add_tokens(600).expect("under");
        budget
            .add_tokens(400)
            .expect("exactly the limit is allowed");
        let error = budget.add_tokens(1).expect_err("over");
        assert_eq!(
            error,
            BudgetExceeded::Tokens {
                used: 1001,
                limit: 1000
            }
        );
        assert_eq!(budget.tokens_used(), 1001);
    }

    #[test]
    fn one_gate_a_retry_and_one_extra_review_round() {
        let mut budget = Budget::new(Instant::now(), &config());
        budget.gate_a_failed().expect("first failure buys a retry");
        let error = budget.gate_a_failed().expect_err("second failure rejects");
        assert!(
            error.to_string().contains("2 implementer attempts"),
            "{error}"
        );

        let mut budget = Budget::new(Instant::now(), &config());
        budget
            .review_asked_for_changes()
            .expect("first changes verdict buys a round");
        let error = budget
            .review_asked_for_changes()
            .expect_err("second rejects");
        assert_eq!(
            error,
            BudgetExceeded::ReviewRounds {
                limit: MAX_REVIEW_ROUNDS
            }
        );
    }
}
