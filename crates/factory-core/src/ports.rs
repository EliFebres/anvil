//! The ports the service drives: the worker, the code host and the notifier.
//!
//! `factory-core` depends on `anvil-types` only, so the eval harness and the
//! review swarm are reached through these traits; `factory-cli` wires the
//! real implementations and the tests use the fakes in [`crate::fakes`].

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use anvil_types::Ticket;

use crate::error::Result;
use crate::tickets::TicketQueue;

/// A boxed future, so ports can be trait objects.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

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

/// The code host, GitHub in production and a bare repository in tests.
pub trait CodeHost: Send + Sync {
    /// The CI status of the latest commit on `main`.
    fn main_ci(&self) -> BoxFuture<'_, Result<CiStatus>>;
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

/// Receives the worker's stage, for `factory status`.
pub type Progress = Arc<dyn Fn(&str) + Send + Sync>;

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
