//! Claude Code compatibility: SKILL.md, CLAUDE.md, .claude/commands,
//! .claude/agents, hooks and the MCP client.
//!
//! Everything here reads Claude Code's on-disk formats and plugs the result
//! into `anvil-core` from the outside: skills become a `skill` tool and a
//! short list in the system prompt, subagents a `delegate` tool, nothing in
//! the agent loop knows they exist. Depends on anvil-types and anvil-core
//! only; never on any factory crate, anvil-memory or anvil-telemetry.

pub mod agents;
pub mod check;
pub mod commands;
pub mod delegate;
pub mod error;
pub mod frontmatter;
pub mod hooks;
pub mod instructions;
pub mod mcp;
pub mod names;
pub mod paths;
pub mod skills;

pub use agents::{AgentSet, SubagentDef};
pub use commands::{CommandSet, SlashCommand};
pub use delegate::{DelegateTool, SubagentEnv};
pub use error::{CompatError, LoadError};
pub use hooks::{HookConfig, HookEvent, HookRunner};
pub use mcp::{McpError, McpServer, McpTools, ServerSet};
pub use paths::Origin;
pub use skills::{Skill, SkillRoots, SkillSet, SkillTool};
