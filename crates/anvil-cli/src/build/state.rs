//! The state file of a build: one `state.json` per build folder, written
//! atomically after every change, and the plain-text report of it.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::plan::WorkItem;
use crate::status::write_atomic;

/// Where builds live, relative to the project root.
pub const BUILDS_DIR: &str = ".anvil/builds";

/// The state file inside a build folder.
pub const STATE_FILE: &str = "state.json";

/// The only schema version this code reads and writes.
pub const SCHEMA_VERSION: u32 = 1;

/// Where one work item stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemStatus {
    /// Not started, or a failed attempt that will be retried.
    Pending,
    /// A session is working on it, or was when the process stopped.
    InProgress,
    /// Its checks passed and its changes are committed.
    Done,
    /// It used all its attempts in one run.
    Failed,
}

impl ItemStatus {
    /// The snake_case name, as written in the file.
    ///
    /// # Returns
    ///
    /// The name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }
}

/// Where a whole build stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildStatus {
    /// Items remain and the build is running or can be resumed.
    Running,
    /// A budget cap was reached.
    Blocked,
    /// An item failed and the owner must look.
    NeedsInput,
    /// Every item is done.
    Done,
}

impl BuildStatus {
    /// The snake_case name, as written in the file.
    ///
    /// # Returns
    ///
    /// The name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Blocked => "blocked",
            Self::NeedsInput => "needs_input",
            Self::Done => "done",
        }
    }
}

/// One work item and what has happened to it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ItemState {
    /// The item as planned.
    #[serde(flatten)]
    pub item: WorkItem,
    /// The item's Markdown file in the build folder.
    pub file: String,
    /// Where it stands.
    pub status: ItemStatus,
    /// Attempts started so far, over every run of the build.
    pub attempts: u32,
    /// Weighted tokens its sessions used.
    pub tokens: u64,
    /// Estimated cost of its sessions; `None` when no price is configured.
    pub cost_usd: Option<f64>,
    /// The commit that holds its changes, once done.
    pub commit: Option<String>,
}

/// The content of `state.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BuildState {
    /// The format version, [`SCHEMA_VERSION`].
    pub schema: u32,
    /// The build id, a ULID.
    pub id: String,
    /// When the build was planned.
    pub created_at: DateTime<Utc>,
    /// The specification as given on the command line.
    pub spec_path: String,
    /// The branch the build works on and commits to.
    pub branch: String,
    /// Cap on the whole build's cost, if any.
    pub budget_usd: Option<f64>,
    /// Cap on the whole build's weighted tokens, if any.
    pub budget_tokens: Option<u64>,
    /// Weighted tokens the planner used.
    pub planning_tokens: u64,
    /// Where the build stands.
    pub status: BuildStatus,
    /// One line saying more, such as why the build stopped.
    pub detail: String,
    /// The work items in order.
    pub items: Vec<ItemState>,
}

impl BuildState {
    /// Weighted tokens spent so far, planning included.
    ///
    /// # Returns
    ///
    /// The total.
    pub fn spent_tokens(&self) -> u64 {
        self.items.iter().fold(self.planning_tokens, |sum, item| {
            sum.saturating_add(item.tokens)
        })
    }

    /// Estimated cost so far, for the items that have a price.
    ///
    /// # Returns
    ///
    /// The total, or `None` when no item has a cost.
    pub fn spent_usd(&self) -> Option<f64> {
        self.items
            .iter()
            .filter_map(|item| item.cost_usd)
            .reduce(|a, b| a + b)
    }

    /// Items that are not done.
    ///
    /// # Returns
    ///
    /// The count.
    pub fn remaining(&self) -> usize {
        self.items
            .iter()
            .filter(|item| item.status != ItemStatus::Done)
            .count()
    }

    /// Writes `state.json` atomically.
    ///
    /// # Arguments
    ///
    /// * `dir` - The build folder.
    ///
    /// # Errors
    ///
    /// When the file cannot be written; the previous one is then intact.
    pub fn save(&self, dir: &Path) -> anyhow::Result<()> {
        let mut text = serde_json::to_string_pretty(self).context("serializing the build state")?;
        text.push('\n');
        write_atomic(dir, STATE_FILE, text.as_bytes())
            .with_context(|| format!("writing {}", dir.join(STATE_FILE).display()))
    }

    /// Reads a build's `state.json`.
    ///
    /// # Arguments
    ///
    /// * `dir` - The build folder.
    ///
    /// # Errors
    ///
    /// When the file is missing, malformed or of another schema version.
    pub fn load(dir: &Path) -> anyhow::Result<Self> {
        let path = dir.join(STATE_FILE);
        let text =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let state: Self =
            serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        anyhow::ensure!(
            state.schema == SCHEMA_VERSION,
            "{} has schema {}, this anvil reads {SCHEMA_VERSION}",
            path.display(),
            state.schema
        );
        Ok(state)
    }

    /// The build as `anvil build status` prints it.
    ///
    /// # Returns
    ///
    /// A header and one line per item.
    pub fn report(&self) -> String {
        let mut text = format!(
            "build {}  {}  branch {}\nspec {}\n",
            self.id,
            self.status.name(),
            self.branch,
            self.spec_path
        );
        if !self.detail.is_empty() {
            text.push_str(&format!("{}\n", self.detail));
        }
        let cost = self
            .spent_usd()
            .map_or_else(String::new, |usd| format!(", ${usd:.2}"));
        text.push_str(&format!(
            "spent {} tokens{cost}; {} of {} item(s) left\n\n",
            self.spent_tokens(),
            self.remaining(),
            self.items.len()
        ));
        for (index, item) in self.items.iter().enumerate() {
            let commit = item.commit.as_deref().map_or_else(String::new, |hash| {
                format!("  {}", &hash[..hash.len().min(8)])
            });
            text.push_str(&format!(
                "{:>2}. [{}] {}  (attempts {}, {} tokens){commit}\n",
                index + 1,
                item.status.name(),
                item.item.title,
                item.attempts,
                item.tokens
            ));
        }
        text
    }
}

/// The folder that holds every build of a project.
///
/// # Arguments
///
/// * `cwd` - The project root.
///
/// # Returns
///
/// `<cwd>/.anvil/builds`.
pub fn builds_root(cwd: &Path) -> PathBuf {
    cwd.join(BUILDS_DIR)
}

/// The folder of one build.
///
/// # Arguments
///
/// * `cwd` - The project root.
/// * `id` - The build id.
///
/// # Returns
///
/// `<cwd>/.anvil/builds/<id>`.
pub fn build_dir(cwd: &Path, id: &str) -> PathBuf {
    builds_root(cwd).join(id)
}

/// The ids of the project's builds, oldest first.
///
/// # Arguments
///
/// * `cwd` - The project root.
///
/// # Returns
///
/// The ids of folders that hold a `state.json`; empty when there are none.
pub fn build_ids(cwd: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(builds_root(cwd)) else {
        return Vec::new();
    };
    let mut ids: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.path().join(STATE_FILE).is_file())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    ids.sort();
    ids
}

/// Picks the build a command works on.
///
/// # Arguments
///
/// * `cwd` - The project root.
/// * `id` - The id the owner gave, if any.
/// * `unfinished_only` - Without an id, take the newest build that is not
///   done (resume) rather than the newest one (status).
///
/// # Returns
///
/// The id and the loaded state.
///
/// # Errors
///
/// When the id has no build, or there is no build to default to.
pub fn pick_build(
    cwd: &Path,
    id: Option<&str>,
    unfinished_only: bool,
) -> anyhow::Result<(String, BuildState)> {
    if let Some(id) = id {
        let state = BuildState::load(&build_dir(cwd, id))
            .with_context(|| format!("no build `{id}` under {}", builds_root(cwd).display()))?;
        return Ok((id.to_owned(), state));
    }
    for candidate in build_ids(cwd).into_iter().rev() {
        let state = BuildState::load(&build_dir(cwd, &candidate))?;
        if !unfinished_only || state.status != BuildStatus::Done {
            return Ok((candidate, state));
        }
    }
    anyhow::bail!(
        "no {}build under {}",
        if unfinished_only { "unfinished " } else { "" },
        builds_root(cwd).display()
    )
}
