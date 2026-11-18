//! The user's decisions about which repositories to trust.
//!
//! A repository can ship hooks, MCP servers, skills, slash commands, agents
//! and instruction files. Hooks and MCP servers run programs; the others
//! steer the model. anvil loads them only from a repository the user has
//! trusted. The decision is stored in the user's own `~/.anvil/trust.toml`,
//! keyed by the repository's full path, and never in the repository.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::CONFIG_DIR;
use crate::error::ConfigError;

/// The trust file, relative to the home directory.
pub const TRUST_FILE: &str = ".anvil/trust.toml";

/// Folder, relative to the home directory, of the permission rules a user
/// saved for one repository with the `always` answer.
pub const USER_PERMISSIONS_DIR: &str = ".anvil/permissions";

/// What the user decided about one repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrustDecision {
    /// Load what the repository ships.
    Trusted,
    /// Load nothing the repository ships, and do not ask again.
    Untrusted,
}

/// The contents of `~/.anvil/trust.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustStore {
    /// Decisions keyed by [`repo_key`].
    #[serde(default)]
    pub repositories: BTreeMap<String, TrustDecision>,
}

impl TrustStore {
    /// Reads `<home>/.anvil/trust.toml`.
    ///
    /// # Arguments
    ///
    /// * `home` - The home directory.
    ///
    /// # Returns
    ///
    /// The stored decisions; empty when the file does not exist.
    ///
    /// # Errors
    ///
    /// [`ConfigError::Read`] or [`ConfigError::Parse`] when the file exists
    /// but cannot be read or parsed. A broken trust file is an error rather
    /// than "nothing trusted", so the user notices it.
    pub fn load(home: &Path) -> Result<Self, ConfigError> {
        let path = home.join(TRUST_FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(source) => return Err(ConfigError::Read { path, source }),
        };
        toml::from_str(&text).map_err(|source| ConfigError::Parse { path, source })
    }

    /// The stored decision for a repository.
    ///
    /// # Arguments
    ///
    /// * `repo` - The repository root.
    ///
    /// # Returns
    ///
    /// The decision, or `None` when the user was never asked.
    pub fn decision(&self, repo: &Path) -> Option<TrustDecision> {
        self.repositories.get(&repo_key(repo)).copied()
    }

    /// Records a decision, replacing any earlier one.
    ///
    /// # Arguments
    ///
    /// * `repo` - The repository root.
    /// * `decision` - What the user decided.
    pub fn set(&mut self, repo: &Path, decision: TrustDecision) {
        self.repositories.insert(repo_key(repo), decision);
    }

    /// Writes `<home>/.anvil/trust.toml`, creating `~/.anvil` if needed.
    ///
    /// # Arguments
    ///
    /// * `home` - The home directory.
    ///
    /// # Returns
    ///
    /// The path written.
    ///
    /// # Errors
    ///
    /// When the folder or the file cannot be written.
    pub fn save(&self, home: &Path) -> std::io::Result<PathBuf> {
        let path = home.join(TRUST_FILE);
        std::fs::create_dir_all(home.join(CONFIG_DIR))?;
        let text = toml::to_string(self).map_err(std::io::Error::other)?;
        std::fs::write(&path, text)?;
        Ok(path)
    }
}

/// The key a repository is stored under: its full path with symlinks
/// resolved, so two spellings of one folder share a decision.
///
/// # Arguments
///
/// * `repo` - The repository root.
///
/// # Returns
///
/// The path as text, without the `\\?\` prefix Windows adds.
pub fn repo_key(repo: &Path) -> String {
    let resolved = std::fs::canonicalize(repo).unwrap_or_else(|_| repo.to_path_buf());
    let text = resolved.display().to_string();
    text.strip_prefix(r"\\?\").unwrap_or(&text).to_owned()
}

/// Where the user's own permission rules for one repository live.
///
/// The `always` answer to a prompt writes here rather than into the
/// repository, so the repository cannot plant `allow` rules.
///
/// # Arguments
///
/// * `home` - The home directory.
/// * `repo` - The repository root.
///
/// # Returns
///
/// `<home>/.anvil/permissions/<key>.toml`, where `<key>` is [`repo_key`]
/// with every character other than a letter or digit replaced by `-`.
pub fn user_permissions_path(home: &Path, repo: &Path) -> PathBuf {
    let name: String = repo_key(repo)
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    home.join(USER_PERMISSIONS_DIR)
        .join(format!("{}.toml", name.trim_matches('-')))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decisions_round_trip_through_the_file() {
        let home = tempfile::tempdir().expect("home");
        let repo = tempfile::tempdir().expect("repo");
        let mut store = TrustStore::load(home.path()).expect("missing file is empty");
        assert_eq!(store.decision(repo.path()), None);
        store.set(repo.path(), TrustDecision::Trusted);
        store.save(home.path()).expect("saves");
        let again = TrustStore::load(home.path()).expect("loads");
        assert_eq!(again.decision(repo.path()), Some(TrustDecision::Trusted));
    }

    #[test]
    fn two_spellings_of_one_folder_share_a_decision() {
        let repo = tempfile::tempdir().expect("repo");
        std::fs::create_dir(repo.path().join("sub")).expect("mkdir");
        let dotted = repo.path().join("sub").join("..");
        assert_eq!(repo_key(&dotted), repo_key(repo.path()));
    }

    #[test]
    fn a_broken_trust_file_is_an_error() {
        let home = tempfile::tempdir().expect("home");
        std::fs::create_dir_all(home.path().join(CONFIG_DIR)).expect("mkdir");
        std::fs::write(home.path().join(TRUST_FILE), "repositories = [").expect("write");
        assert!(TrustStore::load(home.path()).is_err());
    }

    #[test]
    fn user_permissions_live_under_the_home() {
        let home = Path::new("home");
        let path = user_permissions_path(home, Path::new("/work/my repo"));
        assert!(
            path.starts_with(home.join(USER_PERMISSIONS_DIR)),
            "{path:?}"
        );
        let name = path
            .file_name()
            .expect("name")
            .to_string_lossy()
            .into_owned();
        assert!(name.ends_with("work-my-repo.toml"), "{name}");
    }
}
