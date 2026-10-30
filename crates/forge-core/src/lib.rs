//! Agent loop, tool registry, provider adapters and context management for forge.
//!
//! Depends on forge-types only. Never depends on forge-compat, forge-memory,
//! forge-telemetry, any factory crate, or either CLI; those plug in from the
//! outside through the CLI.

pub mod provider;
pub mod tools;
