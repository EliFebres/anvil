//! The facts store: `~/.anvil/memory/`, one Markdown file per topic.
//!
//! Each non-empty line of a topic file is one fact, written as a `- `
//! bullet. Headings and blank lines are ignored on read, so the files stay
//! pleasant to edit by hand.

pub mod extract;
pub mod load;

use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::MemoryError;
use crate::paths::PROPOSED_FILE;

/// Where a fact belongs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Topic {
    /// The user's machine, editors, languages and tools: `tooling.md`.
    Tooling,
    /// How the user likes to work: `workflow.md`.
    Workflow,
    /// One repository: `projects/<name>.md`.
    Project(String),
}

impl Topic {
    /// The file the topic lives in, relative to the memory directory.
    ///
    /// # Returns
    ///
    /// `tooling.md`, `workflow.md` or `projects/<name>.md`.
    pub fn file_name(&self) -> PathBuf {
        match self {
            Topic::Tooling => PathBuf::from("tooling.md"),
            Topic::Workflow => PathBuf::from("workflow.md"),
            Topic::Project(name) => PathBuf::from("projects").join(format!("{name}.md")),
        }
    }

    /// The label the extractor uses: `tooling`, `workflow` or `project`.
    ///
    /// # Returns
    ///
    /// The label.
    pub fn label(&self) -> &'static str {
        match self {
            Topic::Tooling => "tooling",
            Topic::Workflow => "workflow",
            Topic::Project(_) => "project",
        }
    }

    /// The topic for a label.
    ///
    /// # Arguments
    ///
    /// * `label` - `tooling`, `workflow` or `project`, any case.
    /// * `repo` - The repository name a `project` label refers to.
    ///
    /// # Returns
    ///
    /// The topic, or `None` for any other label.
    pub fn parse(label: &str, repo: &str) -> Option<Self> {
        match label.trim().to_ascii_lowercase().as_str() {
            "tooling" => Some(Topic::Tooling),
            "workflow" => Some(Topic::Workflow),
            "project" | "projects" | "repo" => Some(Topic::Project(repo.to_owned())),
            _ => None,
        }
    }
}

impl fmt::Display for Topic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// The topic files under one memory directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FactStore {
    root: PathBuf,
}

impl FactStore {
    /// A store over a memory directory, which need not exist yet.
    ///
    /// # Arguments
    ///
    /// * `root` - `~/.anvil/memory`.
    ///
    /// # Returns
    ///
    /// The store.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The memory directory.
    ///
    /// # Returns
    ///
    /// The path.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The file of a topic.
    ///
    /// # Arguments
    ///
    /// * `topic` - The topic.
    ///
    /// # Returns
    ///
    /// The path.
    pub fn path(&self, topic: &Topic) -> PathBuf {
        self.root.join(topic.file_name())
    }

    /// The facts of a topic, oldest first.
    ///
    /// # Arguments
    ///
    /// * `topic` - The topic.
    ///
    /// # Returns
    ///
    /// One entry per non-empty, non-heading line with its bullet removed.
    /// A missing or unreadable file yields nothing.
    pub fn read(&self, topic: &Topic) -> Vec<String> {
        let Ok(text) = std::fs::read_to_string(self.path(topic)) else {
            return Vec::new();
        };
        text.lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(|line| strip_bullet(line).to_owned())
            .collect()
    }

    /// Whether a topic already holds a fact, up to normalization.
    ///
    /// # Arguments
    ///
    /// * `topic` - The topic.
    /// * `fact` - The fact.
    ///
    /// # Returns
    ///
    /// `true` when a line restates it.
    pub fn contains(&self, topic: &Topic, fact: &str) -> bool {
        let wanted = normalize(fact);
        self.read(topic)
            .iter()
            .any(|existing| normalize(existing) == wanted)
    }

    /// Appends facts to a topic file as bullet lines.
    ///
    /// # Arguments
    ///
    /// * `topic` - The topic.
    /// * `facts` - The lines to add, in order.
    ///
    /// # Returns
    ///
    /// How many lines were written.
    ///
    /// # Errors
    ///
    /// [`MemoryError::Io`] when the directory or file cannot be written.
    pub fn append(&self, topic: &Topic, facts: &[String]) -> Result<usize, MemoryError> {
        if facts.is_empty() {
            return Ok(0);
        }
        let path = self.path(topic);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| MemoryError::io(parent, &error))?;
        }
        let needs_newline = std::fs::read(&path)
            .map(|bytes| !bytes.is_empty() && !bytes.ends_with(b"\n"))
            .unwrap_or(false);
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|error| MemoryError::io(&path, &error))?;
        let mut text = String::new();
        if needs_newline {
            text.push('\n');
        }
        for fact in facts {
            text.push_str("- ");
            text.push_str(strip_bullet(fact.trim()));
            text.push('\n');
        }
        file.write_all(text.as_bytes())
            .map_err(|error| MemoryError::io(&path, &error))?;
        Ok(facts.len())
    }

    /// The file proposals are written to before review.
    ///
    /// # Returns
    ///
    /// `<memory dir>/proposed.md`.
    pub fn proposed_path(&self) -> PathBuf {
        self.root.join(PROPOSED_FILE)
    }

    /// Deletes the proposals file if it exists.
    ///
    /// # Errors
    ///
    /// [`MemoryError::Io`] when it exists but cannot be removed.
    pub fn remove_proposed(&self) -> Result<(), MemoryError> {
        let path = self.proposed_path();
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(MemoryError::io(path, &error)),
        }
    }
}

/// The comparison form of a fact: lowercase, single spaces, no bullet, no
/// trailing period.
///
/// # Arguments
///
/// * `fact` - The fact.
///
/// # Returns
///
/// The normalized text.
pub fn normalize(fact: &str) -> String {
    strip_bullet(fact.trim())
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
        .trim_end_matches('.')
        .to_owned()
}

/// Removes a leading `- ` or `* ` bullet.
fn strip_bullet(line: &str) -> &str {
    line.strip_prefix("- ")
        .or_else(|| line.strip_prefix("* "))
        .unwrap_or(line)
        .trim_start()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topics_map_to_files_and_labels() {
        assert_eq!(Topic::Tooling.file_name(), PathBuf::from("tooling.md"));
        assert_eq!(
            Topic::Project("anvil".to_owned()).file_name(),
            PathBuf::from("projects").join("anvil.md")
        );
        assert_eq!(
            Topic::parse("Project", "anvil"),
            Some(Topic::Project("anvil".to_owned()))
        );
        assert_eq!(Topic::parse("workflow", "anvil"), Some(Topic::Workflow));
        assert_eq!(Topic::parse("other", "anvil"), None);
        assert_eq!(Topic::Workflow.to_string(), "workflow");
    }

    #[test]
    fn read_append_contains_and_normalize() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FactStore::new(dir.path().join("memory"));
        let topic = Topic::Project("anvil".to_owned());
        assert!(store.read(&topic).is_empty());
        std::fs::create_dir_all(store.path(&topic).parent().expect("parent")).expect("mkdir");
        std::fs::write(
            store.path(&topic),
            "# anvil\n\n- Tests run with just check.\n* Config lives in .anvil/config.toml\nplain line",
        )
        .expect("write");
        assert_eq!(
            store.read(&topic),
            vec![
                "Tests run with just check.",
                "Config lives in .anvil/config.toml",
                "plain line"
            ]
        );
        assert!(store.contains(&topic, "  tests run  with JUST check "));
        assert!(!store.contains(&topic, "tests run with cargo"));
        let written = store
            .append(&topic, &["- New fact.".to_owned(), "Another".to_owned()])
            .expect("appends");
        assert_eq!(written, 2);
        let text = std::fs::read_to_string(store.path(&topic)).expect("read");
        assert!(
            text.ends_with("plain line\n- New fact.\n- Another\n"),
            "{text}"
        );
        assert_eq!(store.append(&Topic::Tooling, &[]).expect("nothing"), 0);
        assert!(!store.path(&Topic::Tooling).exists());
        assert_eq!(normalize("- The  Cat sat."), "the cat sat");
        store.remove_proposed().expect("absent is fine");
        std::fs::write(store.proposed_path(), "x").expect("write");
        store.remove_proposed().expect("removes");
        assert!(!store.proposed_path().exists());
    }
}
