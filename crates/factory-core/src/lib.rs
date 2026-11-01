//! Ticket queue, worker loop, git operations and merge policy for the factory.
//!
//! The factory is a long-lived service on one machine. Every few minutes it
//! fast-forwards its worktree of the `factory/state` branch, copies tickets
//! from `tickets/*` branches into `tickets/open/`, claims the highest-priority
//! oldest one and hands it to a [`Worker`]. The worker's outcome moves the
//! ticket on to `merged/`, `waiting/` or `rejected/`; every move is one commit
//! on the state branch, which the factory alone writes. Code reaches `main`
//! only through pull requests.
//!
//! This crate depends on `anvil-types` only and drives anvil as a subprocess.
//! The eval harness, the review swarm and GitHub are reached through the
//! ports in [`ports`]; `factory-cli` wires the real implementations and the
//! tests use [`fakes`].

pub mod error;
pub mod fakes;
pub mod git;
pub mod github;
pub mod notify;
pub mod paths;
pub mod ports;
pub mod service;
pub mod status;
pub mod tickets;

pub use error::{FactoryError, Result};
pub use git::Git;
pub use github::{GitHub, InlineComment, Pull};
pub use notify::{Event, LogNotifier, Notifier, WebhookNotifier};
pub use paths::FactoryHome;
pub use ports::{BoxFuture, CiStatus, CodeHost, Outcome, Progress, Worker};
pub use service::{Clock, ERRORS_BEFORE_PAUSE, Service, ServiceDeps, Tick, hostname, system_clock};
pub use status::{InProgress, KEPT_OUTCOMES, OutcomeKind, OutcomeRecord, ServiceState};
pub use tickets::{
    Ingested, MODEL_NOTE, OpenTickets, Refused, TicketQueue, Transition, open_ticket_id, order,
};
