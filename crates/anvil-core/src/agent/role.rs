//! The jobs a model does in a run, for attributing spend to a provider.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Consecutive failed project checks after which the next build round runs on the planner.
pub const ESCALATE_AFTER_CHECK_FAILURES: u32 = 2;

/// Consecutive review change requests after which the next build round runs on the planner.
pub const ESCALATE_AFTER_REVIEW_CHANGES: u32 = 2;

/// The job a completion was made for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Writes the plan, and takes escalated build rounds.
    Planner,
    /// Builds, and plans when there is no planner.
    Coder,
    /// Reviews the diff.
    Reviewer,
    /// Writes compaction summaries.
    Summarizer,
}

impl Role {
    /// The lowercase name used in summaries and logs.
    ///
    /// # Returns
    ///
    /// The name.
    pub fn name(self) -> &'static str {
        match self {
            Role::Planner => "planner",
            Role::Coder => "coder",
            Role::Reviewer => "reviewer",
            Role::Summarizer => "summarizer",
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Why a build round ran on the planner instead of the coder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscalationReason {
    /// The project check failed this many times in a row.
    CheckFailures,
    /// The reviewer asked for changes this many times in a row.
    ReviewChanges,
}

impl fmt::Display for EscalationReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EscalationReason::CheckFailures => write!(
                f,
                "the project check failed {ESCALATE_AFTER_CHECK_FAILURES} times in a row"
            ),
            EscalationReason::ReviewChanges => write!(
                f,
                "the reviewer asked for changes {ESCALATE_AFTER_REVIEW_CHANGES} times in a row"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_serialize_lowercase_and_order_by_job() {
        assert_eq!(
            serde_json::to_string(&Role::Planner).expect("json"),
            "\"planner\""
        );
        assert_eq!(Role::Summarizer.to_string(), "summarizer");
        assert!(Role::Planner < Role::Coder && Role::Reviewer < Role::Summarizer);
    }
}
