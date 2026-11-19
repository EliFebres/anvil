//! The user's decisions about which repositories to trust.
//!
//! A repository can ship hooks, MCP servers, skills, slash commands, agents
//! and instruction files. Hooks and MCP servers run programs; the others
//! steer the model. anvil loads them only from a repository the user has
//! trusted. The decision is stored in the user's own `~/.anvil/trust.toml`,
//! keyed by the repository's full path, and never in the repository.
//!
//! A decision to trust is pinned to a digest of what the repository ships
//! (computed by the CLI). When the digest changes, the decision no longer
//! counts and the user is asked again. A decision not to trust stays,
//! whatever changes, because it loosens nothing.

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

/// One repository's entry in `~/.anvil/trust.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TrustRecord {
    /// A decision with the digest of what the repository shipped when it
    /// was made.
    Pinned {
        /// What the user decided.
        decision: TrustDecision,
        /// The SHA-256 digest, in hex, of what the repository shipped.
        digest: String,
    },
    /// A decision from before digests were kept. Trust recorded this way
    /// no longer counts; a refusal still does.
    Unpinned(TrustDecision),
}

/// The contents of `~/.anvil/trust.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustStore {
    /// Records keyed by [`repo_key`].
    #[serde(default)]
    pub repositories: BTreeMap<String, TrustRecord>,
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

    /// The decision that counts for a repository as it is now.
    ///
    /// # Arguments
    ///
    /// * `repo` - The repository root.
    /// * `digest` - The digest of what the repository ships now.
    ///
    /// # Returns
    ///
    /// `Untrusted` when the user refused it, `Trusted` when the user
    /// trusted exactly this content, and `None` when the user was never
    /// asked or trusted content that has since changed.
    pub fn decision(&self, repo: &Path, digest: &str) -> Option<TrustDecision> {
        match self.repositories.get(&repo_key(repo))? {
            TrustRecord::Pinned {
                decision: TrustDecision::Trusted,
                digest: pinned,
            } if pinned == digest => Some(TrustDecision::Trusted),
            TrustRecord::Pinned {
                decision: TrustDecision::Untrusted,
                ..
            }
            | TrustRecord::Unpinned(TrustDecision::Untrusted) => Some(TrustDecision::Untrusted),
            TrustRecord::Pinned { .. } | TrustRecord::Unpinned(TrustDecision::Trusted) => None,
        }
    }

    /// Whether the user once trusted the repository but what it ships has
    /// changed since, so trust no longer counts.
    ///
    /// # Arguments
    ///
    /// * `repo` - The repository root.
    /// * `digest` - The digest of what the repository ships now.
    ///
    /// # Returns
    ///
    /// `true` for a trusted record whose digest differs or is missing.
    pub fn changed_since_trusted(&self, repo: &Path, digest: &str) -> bool {
        match self.repositories.get(&repo_key(repo)) {
            Some(TrustRecord::Pinned {
                decision: TrustDecision::Trusted,
                digest: pinned,
            }) => pinned != digest,
            Some(TrustRecord::Unpinned(TrustDecision::Trusted)) => true,
            _ => false,
        }
    }

    /// Records a decision, replacing any earlier one.
    ///
    /// # Arguments
    ///
    /// * `repo` - The repository root.
    /// * `decision` - What the user decided.
    /// * `digest` - The digest of what the repository ships, as shown to the user.
    pub fn set(&mut self, repo: &Path, decision: TrustDecision, digest: &str) {
        self.repositories.insert(
            repo_key(repo),
            TrustRecord::Pinned {
                decision,
                digest: digest.to_owned(),
            },
        );
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
        assert_eq!(store.decision(repo.path(), "d1"), None);
        store.set(repo.path(), TrustDecision::Trusted, "d1");
        store.save(home.path()).expect("saves");
        let again = TrustStore::load(home.path()).expect("loads");
        assert_eq!(
            again.decision(repo.path(), "d1"),
            Some(TrustDecision::Trusted)
        );
    }

    #[test]
    fn trust_no_longer_counts_once_the_content_changes() {
        let repo = tempfile::tempdir().expect("repo");
        let mut store = TrustStore::default();
        store.set(repo.path(), TrustDecision::Trusted, "before");
        assert_eq!(store.decision(repo.path(), "after"), None);
        assert!(store.changed_since_trusted(repo.path(), "after"));
        assert!(!store.changed_since_trusted(repo.path(), "before"));
        store.set(repo.path(), TrustDecision::Untrusted, "before");
        assert_eq!(
            store.decision(repo.path(), "after"),
            Some(TrustDecision::Untrusted),
            "a refusal stays whatever changes"
        );
    }

    #[test]
    fn trust_recorded_without_a_digest_no_longer_counts() {
        let home = tempfile::tempdir().expect("home");
        let repo = tempfile::tempdir().expect("repo");
        let key = repo_key(repo.path()).replace('\\', "\\\\");
        std::fs::create_dir_all(home.path().join(CONFIG_DIR)).expect("mkdir");
        std::fs::write(
            home.path().join(TRUST_FILE),
            format!("[repositories]\n\"{key}\" = \"trusted\"\n"),
        )
        .expect("write");
        let store = TrustStore::load(home.path()).expect("the old format still loads");
        assert_eq!(store.decision(repo.path(), "any"), None);
        assert!(store.changed_since_trusted(repo.path(), "any"));
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
