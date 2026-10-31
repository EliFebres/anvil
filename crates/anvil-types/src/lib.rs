//! Shared types for `anvil` and `factory`.
//!
//! This crate is the only code both binaries share. It holds the ticket
//! schema, provider and role configuration, telemetry references and the
//! skill manifest, plus a validator for each. Besides validation, the only
//! logic here is loading: the merged anvil configuration, the `.env` files
//! and `factory.toml`, which both binaries read the same way. The crate
//! depends on no other workspace crate, and never on tokio or reqwest.

pub mod boundary;
pub mod config;
pub mod dotenv;
pub mod error;
pub mod factory_config;
pub mod schema;
pub mod skill;
pub mod ticket;

pub use config::{
    AnvilConfig, LimitsConfig, LoadedConfig, Price, Protocol, ProviderConfig, Roles, StateBudget,
    TelemetryConfig, TicketsConfig, ToolsConfig,
};
pub use error::{ConfigError, DotenvError, ValidationError};
pub use factory_config::{FactoryConfig, FactoryRoles, GithubConfig, NotifyConfig, Reviewer};
pub use skill::SkillManifest;
pub use ticket::{Priority, TelemetryRef, Ticket, TicketKind, TicketOrigin, TicketStatus};
