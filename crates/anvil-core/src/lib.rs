//! Agent loop, tool registry, provider adapters and context management for anvil.
//!
//! Depends on anvil-types only. Never depends on anvil-compat, anvil-memory,
//! anvil-telemetry or the CLI; those plug in from the outside through the
//! CLI.

pub mod agent;
pub mod context;
pub mod git;
pub mod guard;
pub mod net;
pub mod outline;
pub mod provider;
pub mod tools;
