//! Cross-project memory and coding-style rules.
//!
//! Two hand-editable stores under `~/.anvil/`:
//!
//! - `rules.toml`: standing coding rules ([`RuleSet`]). They go into the
//!   system prompt of a project in a matching language, the reviewer cites them, and the ones with a `check`
//!   run on the session's changed files ([`run_checks`]).
//! - `memory/`: one Markdown file per topic, one fact per line
//!   ([`FactStore`]). [`load_memory`] renders them for the prompt under a
//!   token cap, and [`facts::extract`] turns a session transcript into
//!   proposals the user keeps or drops.
//!
//! Three more things live beside them:
//!
//! - [`languages`]: language tags by extension and a bounded project scan,
//!   so a prompt carries only the rules for the project's languages.
//! - [`corrections`] and [`rule_proposals`]: the files anvil left behind,
//!   what the owner changed in them, and the rule proposals drawn from it.
//! - [`templates`]: `~/.anvil/templates/<name>/`, project skeletons.
//!
//! Everything here is plain TOML or Markdown: no database, no embeddings,
//! no background process. The crate may depend on anvil-types only and in
//! fact needs nothing from it; never on anvil-core, anvil-compat or any
//! factory crate. The CLI wires it into anvil-core from the outside.

pub mod corrections;
pub mod error;
pub mod facts;
pub mod languages;
pub mod paths;
pub mod rule_proposals;
pub mod rules;
pub mod templates;

pub use error::MemoryError;
pub use facts::extract::{Decision, Proposal};
pub use facts::load::{LoadedMemory, MEMORY_TOKEN_CAP, load_memory};
pub use facts::{FactStore, Topic};
pub use rules::check::{CheckFailure, run_checks};
pub use rules::{Check, Rule, RuleSet};
