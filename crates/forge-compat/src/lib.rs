//! Claude Code compatibility: SKILL.md, CLAUDE.md, .claude/commands,
//! .claude/agents, hooks and the MCP client.
//!
//! Everything here reads Claude Code's on-disk formats and plugs the result
//! into `forge-core` from the outside: skills become a `skill` tool and a
//! short list in the system prompt, subagents a `delegate` tool, nothing in
//! the agent loop knows they exist. Depends on forge-types and forge-core
//! only; never on any factory crate, forge-memory or forge-telemetry.

pub mod agents;
pub mod commands;
pub mod delegate;
pub mod error;
pub mod frontmatter;
pub mod instructions;
pub mod names;
pub mod paths;
pub mod skills;

pub use agents::{AgentSet, SubagentDef};
pub use commands::{CommandSet, SlashCommand};
pub use delegate::{DelegateTool, SubagentEnv};
pub use error::{CompatError, LoadError};
pub use paths::Origin;
pub use skills::{Skill, SkillRoots, SkillSet, SkillTool};
