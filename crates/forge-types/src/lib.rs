//! Shared types for `forge` and `factory`.
//!
//! This crate is the only code both binaries share. It holds the ticket
//! schema, provider and role configuration, telemetry references and the
//! skill manifest, plus a validator for each. There is no logic here beyond
//! validation, and the crate depends on no other workspace crate.

pub mod boundary;
pub mod config;
pub mod error;
pub mod schema;
pub mod skill;
pub mod ticket;

pub use config::{ForgeConfig, LimitsConfig, Protocol, ProviderConfig, Roles, ToolsConfig};
pub use error::ValidationError;
pub use skill::SkillManifest;
pub use ticket::{Priority, TelemetryRef, Ticket, TicketKind, TicketOrigin, TicketStatus};
