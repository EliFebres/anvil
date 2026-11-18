//! Shared types for anvil.
//!
//! This crate holds the ticket schema, provider and role configuration,
//! telemetry references, the skill manifest and the session status file,
//! plus a validator for each. Besides validation, the only logic here is
//! loading: the user's anvil configuration, the tightening rules a
//! repository may add, the user's trust decisions and the `.env` file. The crate
//! depends on no other workspace crate, and never on tokio or reqwest.

pub mod boundary;
pub mod config;
pub mod dotenv;
pub mod error;
pub mod project;
pub mod schema;
pub mod skill;
pub mod status;
pub mod ticket;
pub mod trust;

pub use config::{
    AnvilConfig, IntegrationsConfig, LimitsConfig, LoadedConfig, PermissionRules, Price,
    PromptConfig, Protocol, ProviderConfig, Roles, StateBudget, TelemetryConfig, TicketsConfig,
    ToolsConfig, Unmatched,
};
pub use error::{ConfigError, DotenvError, ValidationError};
pub use project::ProjectRules;
pub use skill::SkillManifest;
pub use status::{LoopState, SessionSpend, SessionState, SessionStatus};
pub use ticket::{Priority, TelemetryRef, Ticket, TicketKind, TicketOrigin, TicketStatus};
