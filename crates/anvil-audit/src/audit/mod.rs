//! The audit log: one append-only, hash-chained `audit.jsonl` per session,
//! separate from the usage records, mirrored to the policy's
//! `[audit] mirror_dir` when one is set.
//!
//! [`chain`] defines the record, its canonical form and [`chain::verify`];
//! [`log`] appends records; [`show`] renders a file for people.

pub mod chain;
pub mod event;
pub mod log;
pub mod show;

pub use chain::{AuditError, GENESIS, Identity, RecordBody, SealedRecord, Verified, verify};
pub use event::{AuditEvent, Content, ProviderEntry, RecordMode, sha256_hex};
pub use log::{AUDIT_FILE, AuditLog, AuditTarget, Sink, mirror_file};
