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

pub mod budget;
pub mod error;
pub mod fakes;
pub mod git;
pub mod github;
pub mod notify;
pub mod paths;
pub mod ports;
pub mod protected;
pub mod proxy;
pub mod render;
pub mod sandbox;
pub mod secrets;
pub mod service;
pub mod status;
pub mod tickets;
pub mod usage;
pub mod worker;

pub use budget::{Budget, BudgetExceeded, MAX_GATE_A_ATTEMPTS, MAX_REVIEW_ROUNDS};
pub use error::{FactoryError, Result};
pub use git::Git;
pub use github::{GitHub, Pull};
pub use notify::{Event, LogNotifier, Notifier, WebhookNotifier};
pub use paths::FactoryHome;
pub use ports::{
    BoxFuture, CheckOutcome, CiStatus, CodeHost, EvalGate, EvalGateReport, GateB, Implementer,
    ImplementerOutcome, ImplementerStatus, InlineComment, MinorFinding, NEEDS_HUMAN_LABEL, Outcome,
    Progress, PullRequest, ReviewInput, ReviewReport, ReviewSwarm, Tokens, Worker,
};
pub use protected::{Held, ProtectedPaths, dependency_additions};
pub use sandbox::{Sandbox, SandboxConfig};
pub use service::{Clock, ERRORS_BEFORE_PAUSE, Service, ServiceDeps, Tick, hostname, system_clock};
pub use status::{InProgress, KEPT_OUTCOMES, OutcomeKind, OutcomeRecord, ServiceState};
pub use tickets::{
    Ingested, MODEL_NOTE, OpenTickets, Refused, TicketQueue, Transition, open_ticket_id, order,
};
pub use worker::{TicketWorker, WorkerDeps};
