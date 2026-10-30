//! Loads `~/.forge/config.toml` and `.forge/config.toml` and merges them.
//!
//! Tables merge key by key, so a project file can override one provider
//! field or one limit without repeating the rest. Scalars and arrays from
//! the project file replace the global ones.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use forge_types::ForgeConfig;

/// Directory that holds forge configuration, both globally and per project.
pub const CONFIG_DIR: &str = ".forge";

/// File name of the configuration file inside [`CONFIG_DIR`].
pub const CONFIG_FILE: &str = "config.toml";

/// Where the merged configuration came from.
#[derive(Debug, Clone)]
pub struct LoadedConfig {
    /// The merged, validated configuration.
    pub config: ForgeConfig,
    /// Files that contributed, global first.
    pub sources: Vec<PathBuf>,
    /// The merged TOML, for `forge config show`.
    pub merged: toml::Table,
}

/// Loads and merges the configuration.
///
/// # Arguments
///
/// * `home` - The home directory, if known.
/// * `cwd` - The project root.
///
/// # Returns
///
/// The merged configuration.
///
/// # Errors
///
/// When neither file exists, a file does not parse, or validation fails.
pub fn load(home: Option<&Path>, cwd: &Path) -> anyhow::Result<LoadedConfig> {
    let mut merged = toml::Table::new();
    let mut sources = Vec::new();
    let candidates = [
        home.map(|home| home.join(CONFIG_DIR).join(CONFIG_FILE)),
        Some(cwd.join(CONFIG_DIR).join(CONFIG_FILE)),
    ];
    for path in candidates.into_iter().flatten() {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let table: toml::Table =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        merge_tables(&mut merged, table);
        sources.push(path);
    }
    if sources.is_empty() {
        bail!(
            "no configuration found; run `forge config init` (project) or `forge config init --global`"
        );
    }
    let config: ForgeConfig = merged
        .clone()
        .try_into()
        .with_context(|| format!("reading {}", describe(&sources)))?;
    config
        .validate()
        .with_context(|| format!("validating {}", describe(&sources)))?;
    Ok(LoadedConfig {
        config,
        sources,
        merged,
    })
}

/// Merges `overlay` into `base`: nested tables recurse, everything else replaces.
pub fn merge_tables(base: &mut toml::Table, overlay: toml::Table) {
    for (key, value) in overlay {
        match (base.get_mut(&key), value) {
            (Some(toml::Value::Table(existing)), toml::Value::Table(incoming)) => {
                merge_tables(existing, incoming);
            }
            (_, value) => {
                base.insert(key, value);
            }
        }
    }
}

/// Lists the source paths for error messages.
fn describe(sources: &[PathBuf]) -> String {
    sources
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(" + ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_types::config::CONFIG_TEMPLATE;

    fn write(dir: &Path, text: &str) {
        let path = dir.join(CONFIG_DIR).join(CONFIG_FILE);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, text).expect("write");
    }

    #[test]
    fn project_overrides_global_key_by_key() {
        let home = tempfile::tempdir().expect("home");
        let cwd = tempfile::tempdir().expect("cwd");
        write(home.path(), CONFIG_TEMPLATE);
        write(
            cwd.path(),
            "[providers.runpod]\nmodel = \"local-model\"\n\n[limits]\nmax_retries = 1\n",
        );
        let loaded = load(Some(home.path()), cwd.path()).expect("loads");
        assert_eq!(loaded.sources.len(), 2);
        let runpod = &loaded.config.providers["runpod"];
        assert_eq!(runpod.model, "local-model");
        assert_eq!(runpod.context_window, 131_072, "global field kept");
        assert_eq!(loaded.config.limits.max_retries, 1);
        assert_eq!(loaded.config.limits.request_timeout_s, 600);
        assert_eq!(loaded.config.roles.coder, "runpod");
    }

    #[test]
    fn missing_everything_and_invalid_files_fail_clearly() {
        let cwd = tempfile::tempdir().expect("cwd");
        let error = load(None, cwd.path()).expect_err("no config");
        assert!(error.to_string().contains("forge config init"));

        write(
            cwd.path(),
            "[roles]\ncoder = \"nope\"\nsummarizer = \"nope\"\n",
        );
        let error = load(None, cwd.path()).expect_err("unknown provider");
        assert!(
            format!("{error:#}").contains("unknown provider"),
            "{error:#}"
        );

        write(cwd.path(), "not = [toml");
        assert!(load(None, cwd.path()).is_err());
    }

    #[test]
    fn merge_replaces_arrays_and_scalars() {
        let mut base: toml::Table =
            toml::from_str("a = [1]\nb = 1\n[t]\nx = 1\ny = 2").expect("toml");
        let overlay: toml::Table = toml::from_str("a = [2, 3]\n[t]\ny = 9\nz = 3").expect("toml");
        merge_tables(&mut base, overlay);
        assert_eq!(base["a"], toml::Value::Array(vec![2.into(), 3.into()]));
        assert_eq!(base["b"], toml::Value::Integer(1));
        assert_eq!(base["t"]["x"], toml::Value::Integer(1));
        assert_eq!(base["t"]["y"], toml::Value::Integer(9));
        assert_eq!(base["t"]["z"], toml::Value::Integer(3));
    }
}
