//! Agent loop, tool registry, provider adapters and context management for anvil.
//!
//! Depends on anvil-types only. Never depends on anvil-compat, anvil-memory,
//! anvil-telemetry, any factory crate, or either CLI; those plug in from the
//! outside through the CLI.

pub mod agent;
pub mod context;
pub mod provider;
pub mod tools;
