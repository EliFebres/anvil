//! Usage recording: content-free records of what each session did.
//!
//! The crate consumes usage values that the CLI derives from loop events
//! and never calls into the agent loop, so it depends on `anvil-types` only.
//! It must never depend on `anvil-core`.
//!
//! [`Recorder`] appends content-free [`Record`]s as JSONL under
//! `.anvil/usage/<date>.jsonl` and a copy under `~/.anvil/usage/`.
//! Tool arguments and results are recorded as sizes and digests, model text
//! is not recorded at all. [`Stats`] sums them up for `anvil stats`. Nothing
//! here sends a record anywhere.
//!
//! Subagents run with a discarded event sink and are not recorded.

pub mod error;
pub mod paths;
pub mod record;
pub mod recorder;
pub mod stats;

pub use error::UsageError;
pub use record::{
    CheckOrigin, MemorySkipReason, PermissionAnswer, PermissionScope, Record, RecordKind,
    SessionMode, SessionOutcome, StopKind, Tokens, ToolOutcomeKind, WarningClass, digest, hex,
};
pub use recorder::{Recorder, last_sessions, load_records, session_records};
pub use stats::{ProviderStats, Stats, ToolStats, parse_since};
