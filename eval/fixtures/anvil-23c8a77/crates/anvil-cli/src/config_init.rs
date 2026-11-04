//! `anvil config init`: writes a template configuration file.

use std::path::PathBuf;

use anvil_types::config::CONFIG_TEMPLATE;
use anyhow::{Context, bail};

use crate::wiring::home_dir;
use anvil_types::config::{CONFIG_DIR, CONFIG_FILE};

/// Writes the template config and returns its path.
///
/// # Arguments
///
/// * `global` - When true, write `~/.anvil/config.toml`; otherwise write
///   `.anvil/config.toml` under the current directory.
///
/// # Returns
///
/// The path of the file written.
///
/// # Errors
///
/// When the home directory cannot be determined, the file already exists, or
/// the directory or file cannot be created.
pub fn run(global: bool) -> anyhow::Result<PathBuf> {
    let path = config_path(global)?;
    if path.exists() {
        bail!(
            "{} already exists; edit it or delete it first",
            path.display()
        );
    }
    let dir = path
        .parent()
        .context("config path has no parent directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    std::fs::write(&path, CONFIG_TEMPLATE)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Resolves the global or project config path.
fn config_path(global: bool) -> anyhow::Result<PathBuf> {
    let base = if global {
        home_dir().context("cannot determine the home directory")?
    } else {
        PathBuf::new()
    };
    Ok(base.join(CONFIG_DIR).join(CONFIG_FILE))
}
