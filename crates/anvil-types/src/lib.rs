//! Shared types for anvil.
//!
//! This crate holds provider and role configuration, the skill manifest and
//! the session status file, plus a validator for each. Besides validation,
//! the only logic here is loading: the user's anvil configuration, the
//! tightening rules a repository may add, the user's trust decisions, the
//! `.env` file and the managed policy. [`redact`] holds the secret shapes
//! and the redactor every tool result passes through. [`shell`] also holds the
//! conservative command parser that the shell's allowlist mode validates
//! commands with. The crate depends on no other
//! workspace crate, and never on tokio or reqwest.

pub mod child_env;
pub mod config;
pub mod dotenv;
pub mod error;
pub mod policy;
pub mod project;
pub mod redact;
pub mod shell;
pub mod skill;
pub mod status;
pub mod trust;

pub use config::{
    AnvilConfig, LimitsConfig, LoadedConfig, NetworkConfig, PermissionRules, Price, PromptConfig,
    Protocol, ProviderConfig, Roles, StateBudget, ToolsConfig, Unmatched,
};
pub use error::{ConfigError, DotenvError, ValidationError};
pub use project::ProjectRules;
pub use skill::SkillManifest;
pub use status::{LoopState, SessionSpend, SessionState, SessionStatus};
