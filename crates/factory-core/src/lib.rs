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

/// The dashboard snapshot in `~/.factory/status.json`.
pub mod board;
/// Per-ticket budgets: wall clock, tokens, Gate A attempts and review rounds.
pub mod budget;
/// The error type of the factory.
pub mod error;
/// Fakes for every port, for this crate's tests and factory-cli's.
pub mod fakes;
/// The git commands the factory needs, run in one repository or worktree.
pub mod git;
/// GitHub through octocrab: the only module that knows the REST routes.
pub mod github;
/// Every outcome ever recorded, in `~/.factory/outcomes.jsonl`.
pub mod history;
/// Notifications: one webhook URL that is a Discord, Slack or ntfy endpoint.
pub mod notify;
/// The factory's own folder, `~/.factory/`.
pub mod paths;
/// The ports the service and the worker drive.
pub mod ports;
/// The merge policy's protected paths.
pub mod protected;
/// The egress proxy that lets the container reach allowlisted hosts only.
pub mod proxy;
/// Text the worker writes: the task, the pull request body and rejection notes.
pub mod render;
/// The sandbox: anvil and the build tools in a container, or on the host.
pub mod sandbox;
/// The secret scan of a worktree about to be pushed.
pub mod secrets;
/// The service loop: poll, take the next ticket, work it, record the outcome.
pub mod service;
/// The service state in `~/.factory/state.json` and the `factory status` text.
pub mod status;
/// Reads the implementer output log into the lines the dashboard shows.
pub mod tail;
/// The ticket queue on a worktree of the `factory/state` branch.
pub mod tickets;
/// Token usage from anvil's telemetry files, priced with the providers.
pub mod usage;
/// The worker loop: one claimed ticket from a fresh worktree to an outcome.
pub mod worker;

pub use board::{Board, Live, Stage, Status, StatusReader, write_atomic};
pub use budget::{Budget, BudgetExceeded, MAX_GATE_A_ATTEMPTS};
pub use error::{FactoryError, Result};
pub use git::Git;
pub use github::{GitHub, Pull};
pub use notify::{Event, LogNotifier, Notifier, WebhookNotifier};
pub use paths::FactoryHome;
pub use ports::{
    BoxFuture, CheckOutcome, CiStatus, CodeHost, EvalGate, EvalGateReport, GateB, GateCost,
    Implementer, ImplementerOutcome, ImplementerStatus, InlineComment, MinorFinding,
    NEEDS_HUMAN_LABEL, NoProbe, Outcome, Probe, Progress, ProgressEvent, PullRequest, ReviewInput,
    ReviewReport, ReviewSwarm, Tokens, Worker,
};
pub use protected::{Held, ProtectedPaths, dependency_additions};
pub use sandbox::{
    CommandRunner, Exit, Invocation, Sandbox, SandboxConfig, SandboxOptions, SystemRunner,
    build_command, check_image, ensure_cache_volume, ensure_volume, preflight, resolve_anvil_bin,
};
pub use service::{Clock, ERRORS_BEFORE_PAUSE, Service, ServiceDeps, Tick, hostname, system_clock};
pub use status::{InProgress, KEPT_OUTCOMES, Liveness, OutcomeKind, OutcomeRecord, ServiceState};
pub use tickets::{
    Ingested, MODEL_NOTE, OpenTickets, Refused, TicketQueue, Transition, open_ticket_id, order,
};
pub use worker::{TicketWorker, WorkerDeps};
