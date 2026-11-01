//! The merge policy's protected paths: globs from `[protected]` in
//! `factory.toml`, plus new dependencies in any `Cargo.toml`.

use anvil_types::ProtectedConfig;
use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::error::{FactoryError, Result};

/// A change that needs a human.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Held {
    /// The changed path.
    pub path: String,
    /// The glob that matched, or the dependency rule.
    pub rule: String,
}

impl std::fmt::Display for Held {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.path, self.rule)
    }
}

/// The compiled protected list.
#[derive(Debug)]
pub struct ProtectedPaths {
    globs: Vec<String>,
    set: GlobSet,
    cargo_dependencies: bool,
}

/// The tables whose new keys are new dependencies.
const DEPENDENCY_TABLES: [&str; 3] = ["dependencies", "dev-dependencies", "build-dependencies"];

impl ProtectedPaths {
    /// Compiles the list.
    ///
    /// # Arguments
    ///
    /// * `config` - `[protected]` from `factory.toml`.
    ///
    /// # Returns
    ///
    /// The compiled list.
    ///
    /// # Errors
    ///
    /// [`FactoryError::Config`] when a glob does not compile.
    pub fn from_config(config: &ProtectedConfig) -> Result<Self> {
        let mut builder = GlobSetBuilder::new();
        for glob in &config.paths {
            let compiled = Glob::new(glob).map_err(|error| {
                FactoryError::Config(format!("protected path `{glob}` is not a glob: {error}"))
            })?;
            builder.add(compiled);
        }
        let set = builder
            .build()
            .map_err(|error| FactoryError::Config(format!("protected paths: {error}")))?;
        Ok(Self {
            globs: config.paths.clone(),
            set,
            cargo_dependencies: config.cargo_dependencies,
        })
    }

    /// Whether new dependencies hold a pull request.
    pub fn checks_cargo_dependencies(&self) -> bool {
        self.cargo_dependencies
    }

    /// The changed paths that match a protected glob.
    ///
    /// # Arguments
    ///
    /// * `changed` - Repository-relative paths with `/` separators.
    ///
    /// # Returns
    ///
    /// One entry per protected path, in input order.
    pub fn held(&self, changed: &[String]) -> Vec<Held> {
        changed
            .iter()
            .filter_map(|path| {
                self.set.matches(path).first().map(|index| Held {
                    path: path.clone(),
                    rule: self.globs[*index].clone(),
                })
            })
            .collect()
    }

    /// New dependencies in a changed `Cargo.toml`, when the rule is on.
    ///
    /// # Arguments
    ///
    /// * `path` - The manifest path, for the result.
    /// * `base` - The manifest on `main`; empty for a new file.
    /// * `head` - The manifest on the branch.
    ///
    /// # Returns
    ///
    /// One entry per added key under a dependency table.
    pub fn dependency_additions(&self, path: &str, base: &str, head: &str) -> Vec<Held> {
        if !self.cargo_dependencies {
            return Vec::new();
        }
        dependency_additions(base, head)
            .into_iter()
            .map(|(table, name)| Held {
                path: path.to_owned(),
                rule: format!("new dependency `{name}` in [{table}]"),
            })
            .collect()
    }
}

/// Keys present under a dependency table in `head` and absent in `base`:
/// `[dependencies]`, `[dev-dependencies]`, `[build-dependencies]`,
/// `[workspace.dependencies]` and `[target.<triple>.<kind>]`.
///
/// # Arguments
///
/// * `base` - The manifest before.
/// * `head` - The manifest after.
///
/// # Returns
///
/// `(table, key)` pairs; a manifest that does not parse adds nothing.
pub fn dependency_additions(base: &str, head: &str) -> Vec<(String, String)> {
    let base: toml::Table = toml::from_str(base).unwrap_or_default();
    let Ok(head): std::result::Result<toml::Table, _> = toml::from_str(head) else {
        return Vec::new();
    };
    let mut added = Vec::new();
    for (table_name, keys) in dependency_tables(&head) {
        let before = dependency_tables(&base)
            .into_iter()
            .find(|(name, _)| *name == table_name)
            .map(|(_, keys)| keys)
            .unwrap_or_default();
        for key in keys {
            if !before.contains(&key) {
                added.push((table_name.clone(), key));
            }
        }
    }
    added
}

/// Every dependency table in a manifest with its keys.
fn dependency_tables(manifest: &toml::Table) -> Vec<(String, Vec<String>)> {
    let mut tables = Vec::new();
    let keys_of = |value: &toml::Value| -> Vec<String> {
        value
            .as_table()
            .map(|table| table.keys().cloned().collect())
            .unwrap_or_default()
    };
    for name in DEPENDENCY_TABLES {
        if let Some(value) = manifest.get(name) {
            tables.push((name.to_owned(), keys_of(value)));
        }
    }
    if let Some(workspace) = manifest.get("workspace").and_then(toml::Value::as_table)
        && let Some(value) = workspace.get("dependencies")
    {
        tables.push(("workspace.dependencies".to_owned(), keys_of(value)));
    }
    if let Some(targets) = manifest.get("target").and_then(toml::Value::as_table) {
        for (triple, target) in targets {
            for name in DEPENDENCY_TABLES {
                if let Some(value) = target.get(name) {
                    tables.push((format!("target.{triple}.{name}"), keys_of(value)));
                }
            }
        }
    }
    tables
}

#[cfg(test)]
mod tests {
    use super::*;

    fn protected(paths: &[&str], cargo: bool) -> ProtectedPaths {
        ProtectedPaths::from_config(&ProtectedConfig {
            paths: paths.iter().map(|p| (*p).to_owned()).collect(),
            cargo_dependencies: cargo,
        })
        .expect("compiles")
    }

    #[test]
    fn globs_match_repository_paths() {
        let p = protected(&["deploy/**", "CLAUDE.md", "**/permissions*.toml"], false);
        let held = p.held(&[
            "crates/anvil-core/src/lib.rs".to_owned(),
            "deploy/nuc/Containerfile".to_owned(),
            "CLAUDE.md".to_owned(),
            ".anvil/permissions.toml".to_owned(),
            "docs/CLAUDE.md".to_owned(),
        ]);
        let paths: Vec<&str> = held.iter().map(|h| h.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "deploy/nuc/Containerfile",
                "CLAUDE.md",
                ".anvil/permissions.toml"
            ]
        );
        assert_eq!(held[0].rule, "deploy/**");
        assert_eq!(held[0].to_string(), "deploy/nuc/Containerfile (deploy/**)");
    }

    #[test]
    fn a_bad_glob_is_a_config_error() {
        let error = ProtectedPaths::from_config(&ProtectedConfig {
            paths: vec!["crates/[".to_owned()],
            cargo_dependencies: true,
        })
        .expect_err("rejected");
        assert!(matches!(error, FactoryError::Config(_)), "{error}");
    }

    #[test]
    fn new_dependency_keys_are_found_in_every_dependency_table() {
        let base = r#"
[package]
name = "x"
[dependencies]
serde = "1"
[dev-dependencies]
insta = "1"
[workspace.dependencies]
tokio = "1"
[target.'cfg(unix)'.dependencies]
libc = "0.2"
"#;
        let head = r#"
[package]
name = "x"
version = "0.2.0"
[dependencies]
serde = { version = "1", features = ["derive"] }
sha2 = "0.10"
[dev-dependencies]
insta = "1"
wiremock = "0.6"
[build-dependencies]
cc = "1"
[workspace.dependencies]
tokio = "1"
[target.'cfg(unix)'.dependencies]
libc = "0.2"
nix = "0.29"
"#;
        let added = dependency_additions(base, head);
        assert_eq!(
            added,
            vec![
                ("dependencies".to_owned(), "sha2".to_owned()),
                ("dev-dependencies".to_owned(), "wiremock".to_owned()),
                ("build-dependencies".to_owned(), "cc".to_owned()),
                ("target.cfg(unix).dependencies".to_owned(), "nix".to_owned()),
            ]
        );
        assert!(
            dependency_additions(head, base).is_empty(),
            "removals are fine"
        );
        assert!(dependency_additions(base, base).is_empty());
        let bumped = base.replace("serde = \"1\"", "serde = \"1.0.200\"");
        assert!(
            dependency_additions(base, &bumped).is_empty(),
            "a version bump is not an addition"
        );
        assert_eq!(
            dependency_additions("", "[dependencies]\nnew = \"1\"\n").len(),
            1,
            "a new manifest"
        );
        assert!(dependency_additions(base, "not = [toml").is_empty());
    }

    #[test]
    fn the_rule_can_be_switched_off() {
        let on = protected(&["x"], true);
        let off = protected(&["x"], false);
        let base = "[dependencies]\n";
        let head = "[dependencies]\nsha2 = \"0.10\"\n";
        let held = on.dependency_additions("crates/a/Cargo.toml", base, head);
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].rule, "new dependency `sha2` in [dependencies]");
        assert!(
            off.dependency_additions("crates/a/Cargo.toml", base, head)
                .is_empty()
        );
        assert!(on.checks_cargo_dependencies());
    }
}
