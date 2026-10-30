//! Usage recording, failure classification and ticket drafting.
//!
//! Consumes telemetry event values and never calls into the agent loop, so it
//! depends on forge-types only. Never depends on forge-core or any factory
//! crate. Nothing is implemented yet.
