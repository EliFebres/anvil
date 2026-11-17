//! The factory's `.env` files.
//!
//! The factory loads the same files anvil does, in the same first-wins
//! order, with its own `~/.factory/env` in front of them: the real
//! environment, then `~/.factory/env`, then `~/.anvil/.env`, then the
//! project's `.anvil/.env`.

use std::path::{Path, PathBuf};

/// Directory under the home directory that holds factory settings.
pub const FACTORY_DIR: &str = ".factory";

/// File name of the factory's env file inside [`FACTORY_DIR`].
pub const FACTORY_ENV_FILE: &str = "env";

/// The `.env` files the factory loads, in loading order.
///
/// # Arguments
///
/// * `home` - The home directory, if known.
/// * `cwd` - The project root.
///
/// # Returns
///
/// `~/.factory/env` followed by anvil's files; a missing file is skipped by
/// [`anvil_types::dotenv::load`].
pub fn env_files(home: Option<&Path>, cwd: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Some(home) = home {
        files.push(home.join(FACTORY_DIR).join(FACTORY_ENV_FILE));
    }
    files.extend(anvil_types::dotenv::env_files(home, cwd));
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_factory_file_comes_before_anvil_files() {
        let home = Path::new("/home/u");
        let cwd = Path::new("/proj");
        assert_eq!(
            env_files(Some(home), cwd),
            vec![
                home.join(".factory").join("env"),
                home.join(".anvil").join(".env"),
                cwd.join(".anvil").join(".env")
            ]
        );
        assert_eq!(env_files(None, cwd), vec![cwd.join(".anvil").join(".env")]);
    }
}
