//! Usage recording, failure classification and ticket drafting.
//!
//! The crate consumes telemetry values that the CLI derives from loop events
//! and never calls into the agent loop, so it depends on `forge-types` only.
//! It must never depend on `forge-core` or any `factory-*` crate.
//!
//! Three things live here:
//!
//! - [`Recorder`] appends content-free [`Record`]s as JSONL under
//!   `.forge/telemetry/<date>.jsonl` and a copy under `~/.forge/telemetry/`.
//!   Tool arguments and results are recorded as sizes and digests, model text
//!   is not recorded at all.
//! - [`classify`] turns one session's records (plus cross-session history)
//!   into [`Signal`]s: repeated tool failures, exhausted loops, slow models.
//! - [`draft_all`] turns signals into [`forge_types::Ticket`] drafts through
//!   a [`Drafter`] (the summarizer, supplied by the CLI) and the ticket
//!   validator, which is the data boundary toward the factory.
//!
//! Subagents run with a discarded event sink and are not recorded.

pub mod classify;
pub mod draft;
pub mod error;
pub mod git;
pub mod paths;
pub mod record;
pub mod recorder;
pub mod signal;
pub mod stats;
pub mod tickets;

pub use classify::classify;
pub use draft::{
    DraftFields, DraftFuture, DraftReport, Drafter, SYSTEM_PROMPT, assemble, draft_all,
    parse_draft, relevant_records, user_prompt,
};
pub use error::TelemetryError;
pub use git::publish_ticket;
pub use record::{
    PermissionAnswer, Record, RecordKind, SessionMode, SessionOutcome, StopKind, Tokens,
    ToolOutcomeKind, WarningClass, digest, hex,
};
pub use recorder::{Recorder, last_sessions, load_records, session_records};
pub use signal::{
    Signal, SignalKind, append_signals, load_signals, normalized_title, save_signals,
};
pub use stats::{ProviderStats, Stats, ToolStats, parse_since};
pub use tickets::{StoredTicket, TicketStore, load_ticket, write_ticket};
