//! Subagent definitions: Markdown files in `agents/` directories.
//!
//! The frontmatter names the agent and describes when to use it; the body
//! is its system prompt. `tools` narrows what it may call and `model` picks
//! a role or provider from the forge config.

use std::path::{Path, PathBuf};

use forge_core::agent::Instruction;
use serde_json::{Map, Value};

use crate::error::{CompatError, LoadError};
use crate::frontmatter;
use crate::names::to_forge_name;
use crate::paths::{Origin, config_dirs};

/// The subdirectory agents live in.
pub const AGENTS_DIR: &str = "agents";

/// One subagent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentDef {
    /// The name the model passes to `delegate`.
    pub name: String,
    /// When to use it; shown in the system prompt.
    pub description: String,
    /// Tools it may call, as forge names. `None` means every tool.
    pub tools: Option<Vec<String>>,
    /// A role or provider name from the config, if the file names one.
    pub model: Option<String>,
    /// Its system prompt.
    pub body: String,
    /// The file it was read from.
    pub path: PathBuf,
    /// Which directory it came from.
    pub origin: Origin,
    /// Every other frontmatter key.
    pub extra: Map<String, Value>,
}

impl SubagentDef {
    /// Whether this agent may call a tool.
    ///
    /// # Arguments
    ///
    /// * `tool` - A forge tool name.
    ///
    /// # Returns
    ///
    /// `true` when there is no allow list or the tool is on it.
    pub fn allows(&self, tool: &str) -> bool {
        self.tools
            .as_ref()
            .is_none_or(|tools| tools.iter().any(|allowed| allowed == tool))
    }
}

/// Every agent found, deduplicated by name in directory priority order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentSet {
    /// Loaded agents, sorted by name.
    agents: Vec<SubagentDef>,
    /// Markdown files that did not parse.
    errors: Vec<LoadError>,
}

impl AgentSet {
    /// Walks the `agents/` directories and loads every `.md` file.
    ///
    /// # Arguments
    ///
    /// * `cwd` - The project root.
    /// * `home` - The home directory, if known.
    ///
    /// # Returns
    ///
    /// The set. Broken files are listed in `errors`, never fatal.
    pub fn discover(cwd: &Path, home: Option<&Path>) -> Self {
        let mut set = Self::default();
        for (dir, origin) in config_dirs(cwd, home, AGENTS_DIR) {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            let mut files: Vec<PathBuf> = entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.is_file() && path.extension().is_some_and(|ext| ext == "md"))
                .collect();
            files.sort();
            for path in files {
                match load_agent(&path, origin) {
                    Ok(agent) => {
                        if set.get(&agent.name).is_none() {
                            set.agents.push(agent);
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%error, "skipping subagent");
                        set.errors.push(LoadError { path, error });
                    }
                }
            }
        }
        set.agents.sort_by(|a, b| a.name.cmp(&b.name));
        set
    }

    /// A set holding the given agents, for tests and callers that load
    /// their own.
    ///
    /// # Arguments
    ///
    /// * `agents` - The agents; later duplicates by name are dropped.
    ///
    /// # Returns
    ///
    /// The set, sorted by name.
    pub fn from_agents(agents: Vec<SubagentDef>) -> Self {
        let mut set = Self::default();
        for agent in agents {
            if set.get(&agent.name).is_none() {
                set.agents.push(agent);
            }
        }
        set.agents.sort_by(|a, b| a.name.cmp(&b.name));
        set
    }

    /// The loaded agents, sorted by name.
    ///
    /// # Returns
    ///
    /// The agents.
    pub fn agents(&self) -> &[SubagentDef] {
        &self.agents
    }

    /// Files that did not parse.
    ///
    /// # Returns
    ///
    /// The errors.
    pub fn errors(&self) -> &[LoadError] {
        &self.errors
    }

    /// Whether nothing loaded.
    ///
    /// # Returns
    ///
    /// `true` when there are no agents.
    pub fn is_empty(&self) -> bool {
        self.agents.is_empty()
    }

    /// Looks an agent up by name.
    ///
    /// # Arguments
    ///
    /// * `name` - The agent name.
    ///
    /// # Returns
    ///
    /// The agent, if loaded.
    pub fn get(&self, name: &str) -> Option<&SubagentDef> {
        self.agents.iter().find(|agent| agent.name == name)
    }

    /// The names of every loaded agent.
    ///
    /// # Returns
    ///
    /// Sorted names.
    pub fn names(&self) -> Vec<&str> {
        self.agents
            .iter()
            .map(|agent| agent.name.as_str())
            .collect()
    }

    /// The system prompt section listing the agents.
    ///
    /// # Returns
    ///
    /// An instruction block, or `None` when there are no agents.
    pub fn prompt_section(&self) -> Option<Instruction> {
        if self.agents.is_empty() {
            return None;
        }
        let mut text = String::from(
            "Subagents are specialists you can hand a self-contained task to with the \
             `delegate` tool. Each runs in its own fresh context with its own tools and \
             returns only its final message, so use one when a task would otherwise fill \
             your context with intermediate output.\n",
        );
        for agent in &self.agents {
            let description = agent
                .description
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            text.push_str(&format!("\n- {}: {description}", agent.name));
        }
        text.push('\n');
        Some(Instruction {
            source: "subagents".to_owned(),
            text,
        })
    }
}

/// Reads one agent file.
///
/// # Arguments
///
/// * `path` - The Markdown file.
/// * `origin` - Which directory it came from.
///
/// # Returns
///
/// The definition. `name` defaults to the file stem; `tools` entries are
/// translated from Claude Code names to forge names.
///
/// # Errors
///
/// [`CompatError::Io`], a frontmatter error, or
/// [`CompatError::MissingField`] when `description` is absent.
pub fn load_agent(path: &Path, origin: Origin) -> Result<SubagentDef, CompatError> {
    let text = std::fs::read_to_string(path).map_err(|error| CompatError::io(path, &error))?;
    let parsed = frontmatter::parse(&text, path)?;
    let description = parsed.require_str("description", path)?.to_owned();
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name = parsed.get_str("name").map_or(stem, str::to_owned);
    let tools = parsed
        .get_list("tools")
        .map(|tools| tools.iter().map(|tool| to_forge_name(tool)).collect());
    let model = parsed.get_str("model").map(str::to_owned);
    let mut extra = parsed.fields.clone();
    for key in ["name", "description", "tools", "model"] {
        extra.remove(key);
    }
    Ok(SubagentDef {
        name,
        description,
        tools,
        model,
        body: parsed.body,
        path: path.to_path_buf(),
        origin,
        extra,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        std::fs::write(path, text).expect("write");
    }

    #[test]
    fn loads_agents_with_tool_lists_in_either_spelling() {
        let project = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        write(
            &project.path().join(".claude/agents/reviewer.md"),
            "---\nname: reviewer\ndescription: Reviews diffs.\ntools: Read, Grep, Bash(git:*)\nmodel: haiku\ncolor: blue\n---\nFindings only.\n",
        );
        write(
            &project.path().join(".forge/agents/reviewer.md"),
            "---\ndescription: forge reviewer\ntools: [glob, Edit]\n---\nForge body.\n",
        );
        write(
            &home.path().join(".claude/agents/explorer.md"),
            "---\ndescription: Explores.\n---\nLook around.\n",
        );
        write(&home.path().join(".claude/agents/notes.txt"), "ignored");
        write(
            &home.path().join(".claude/agents/broken.md"),
            "---\nname: broken\n---\nno description\n",
        );

        let set = AgentSet::discover(project.path(), Some(home.path()));
        assert_eq!(set.names(), ["explorer", "reviewer"]);
        let reviewer = set.get("reviewer").expect("reviewer");
        assert_eq!(reviewer.origin, Origin::ProjectForge);
        assert_eq!(
            reviewer.tools.as_deref(),
            Some(&["glob".to_owned(), "edit_file".to_owned()][..])
        );
        assert!(reviewer.allows("glob") && !reviewer.allows("shell"));
        let explorer = set.get("explorer").expect("explorer");
        assert!(explorer.tools.is_none() && explorer.allows("shell"));
        assert_eq!(set.errors().len(), 1);
        assert!(matches!(
            set.errors()[0].error,
            CompatError::MissingField {
                field: "description",
                ..
            }
        ));

        let claude = load_agent(
            &project.path().join(".claude/agents/reviewer.md"),
            Origin::ProjectClaude,
        )
        .expect("loads");
        assert_eq!(
            claude.tools.as_deref(),
            Some(
                &[
                    "read_file".to_owned(),
                    "grep".to_owned(),
                    "shell".to_owned()
                ][..]
            )
        );
        assert_eq!(claude.model.as_deref(), Some("haiku"));
        assert_eq!(claude.extra["color"], "blue");
        assert_eq!(claude.body, "Findings only.\n");
    }

    #[test]
    fn prompt_section_lists_names_and_descriptions() {
        let project = tempfile::tempdir().expect("tempdir");
        write(
            &project.path().join(".claude/agents/reviewer.md"),
            "---\ndescription: Reviews\n  diffs.\n---\nSECRET BODY\n",
        );
        let set = AgentSet::discover(project.path(), None);
        let section = set.prompt_section().expect("section");
        assert_eq!(section.source, "subagents");
        assert!(section.text.contains("- reviewer: Reviews diffs."));
        assert!(!section.text.contains("SECRET BODY"));
        assert!(AgentSet::default().prompt_section().is_none());
    }
}
