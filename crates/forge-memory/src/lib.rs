//! Cross-project memory and coding-style rules.
//!
//! Two hand-editable stores under `~/.forge/`:
//!
//! - `rules.toml`: standing coding rules ([`RuleSet`]). They go into every
//!   system prompt, the reviewer cites them, and the ones with a `check`
//!   run on the session's changed files ([`run_checks`]).
//! - `memory/`: one Markdown file per topic, one fact per line
//!   ([`FactStore`]). [`load_memory`] renders them for the prompt under a
//!   token cap, and [`facts::extract`] turns a session transcript into
//!   proposals the user keeps or drops.
//!
//! Everything here is plain TOML or Markdown: no database, no embeddings,
//! no background process. The crate may depend on forge-types only and in
//! fact needs nothing from it; never on forge-core, forge-compat or any
//! factory crate. The CLI wires it into forge-core from the outside.

pub mod error;
pub mod facts;
pub mod paths;
pub mod rules;

pub use error::MemoryError;
pub use facts::extract::{Decision, Proposal};
pub use facts::load::{LoadedMemory, MEMORY_TOKEN_CAP, load_memory};
pub use facts::{FactStore, Topic};
pub use rules::check::{CheckFailure, run_checks};
pub use rules::{Check, Rule, RuleSet};
