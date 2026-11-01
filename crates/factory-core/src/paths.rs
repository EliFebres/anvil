//! The factory's own folder, `~/.factory/`.
//!
//! ```text
//! ~/.factory/
//!   env          the secrets, mode 600, loaded by the service only
//!   pause        present while the loop must not take a ticket; holds the reason
//!   state.json   the ticket in progress, the last outcomes and the digest counters
//!   logs/        factory.jsonl, rolled daily
//!   state/       a worktree of the factory/state branch
//!   work/<id>/   one worktree per ticket being worked, removed afterwards
//! ```

use std::path::{Path, PathBuf};

use anvil_types::dotenv::{FACTORY_DIR, FACTORY_ENV_FILE};

/// Environment variable that overrides the home directory, so tests can
/// point `~/.factory` at a temp folder.
pub const HOME_ENV: &str = "FACTORY_HOME";

/// The folder structure under `~/.factory`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FactoryHome {
    /// `~/.factory`.
    root: PathBuf,
}

impl FactoryHome {
    /// The factory folder under a home directory.
    ///
    /// # Arguments
    ///
    /// * `home` - The user's home directory.
    ///
    /// # Returns
    ///
    /// `<home>/.factory`.
    pub fn under(home: &Path) -> Self {
        Self {
            root: home.join(FACTORY_DIR),
        }
    }

    /// The factory folder for this process: `$FACTORY_HOME/.factory`, else
    /// `~/.factory`.
    ///
    /// # Returns
    ///
    /// The folder, or `None` when no home directory is known.
    pub fn from_env() -> Option<Self> {
        std::env::var_os(HOME_ENV)
            .map(PathBuf::from)
            .or_else(std::env::home_dir)
            .map(|home| Self::under(&home))
    }

    /// `~/.factory`.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The secrets file.
    pub fn env_file(&self) -> PathBuf {
        self.root.join(FACTORY_ENV_FILE)
    }

    /// The pause flag; its content is the reason.
    pub fn pause_file(&self) -> PathBuf {
        self.root.join("pause")
    }

    /// The service state.
    pub fn state_file(&self) -> PathBuf {
        self.root.join("state.json")
    }

    /// The log folder.
    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }

    /// The worktree of the `factory/state` branch.
    pub fn state_dir(&self) -> PathBuf {
        self.root.join("state")
    }

    /// The folder that holds every ticket worktree.
    pub fn work_root(&self) -> PathBuf {
        self.root.join("work")
    }

    /// The worktree folder of one ticket.
    ///
    /// # Arguments
    ///
    /// * `id` - The ticket id.
    pub fn work_dir(&self, id: &str) -> PathBuf {
        self.work_root().join(id)
    }

    /// The anvil home the sandbox mounts for one ticket, next to its worktree
    /// so it is never part of the tree.
    ///
    /// # Arguments
    ///
    /// * `id` - The ticket id.
    pub fn ticket_home(&self, id: &str) -> PathBuf {
        self.work_root().join(format!("{id}-home"))
    }

    /// The egress proxy's unix socket for one ticket.
    ///
    /// # Arguments
    ///
    /// * `id` - The ticket id.
    pub fn proxy_socket(&self, id: &str) -> PathBuf {
        self.work_root().join(format!("{id}-proxy.sock"))
    }

    /// The eval wrapper script for one ticket.
    ///
    /// # Arguments
    ///
    /// * `id` - The ticket id.
    pub fn eval_wrapper(&self, id: &str) -> PathBuf {
        self.work_root().join(format!("{id}-eval.sh"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders_hang_off_dot_factory() {
        let home = FactoryHome::under(Path::new("/h"));
        assert_eq!(home.root(), Path::new("/h/.factory"));
        assert_eq!(home.env_file(), Path::new("/h/.factory/env"));
        assert_eq!(home.pause_file(), Path::new("/h/.factory/pause"));
        assert_eq!(home.state_file(), Path::new("/h/.factory/state.json"));
        assert_eq!(home.logs_dir(), Path::new("/h/.factory/logs"));
        assert_eq!(home.state_dir(), Path::new("/h/.factory/state"));
        assert_eq!(home.work_dir("X"), Path::new("/h/.factory/work/X"));
        assert_eq!(home.ticket_home("X"), Path::new("/h/.factory/work/X-home"));
        assert_eq!(
            home.proxy_socket("X"),
            Path::new("/h/.factory/work/X-proxy.sock")
        );
        assert_eq!(
            home.eval_wrapper("X"),
            Path::new("/h/.factory/work/X-eval.sh")
        );
    }
}
