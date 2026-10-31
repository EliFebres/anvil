//! `.env` loading shared by `forge` and `factory`.
//!
//! Keys never live in configuration files. Each provider names an
//! environment variable, and both binaries fill the environment from `.env`
//! files at startup, in a fixed order where the first definition wins: the
//! real environment, then `~/.factory/env` (factory only), then
//! `~/.forge/.env`, then the project's `.forge/.env`.

use std::path::{Path, PathBuf};

use crate::error::DotenvError;

/// Directory under the home directory that holds factory settings.
pub const FACTORY_DIR: &str = ".factory";

/// File name of the factory's env file inside [`FACTORY_DIR`].
pub const FACTORY_ENV_FILE: &str = "env";

/// File name of the forge env file inside `.forge`.
pub const FORGE_ENV_FILE: &str = ".env";

/// The `.env` files a binary loads, in loading order.
///
/// # Arguments
///
/// * `home` - The home directory, if known.
/// * `cwd` - The project root.
/// * `factory` - Whether to include `~/.factory/env` (first, since it is the
///   service-specific file).
///
/// # Returns
///
/// The candidate paths; a missing file is skipped by [`load`].
pub fn env_files(home: Option<&Path>, cwd: &Path, factory: bool) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Some(home) = home {
        if factory {
            files.push(home.join(FACTORY_DIR).join(FACTORY_ENV_FILE));
        }
        files.push(home.join(crate::config::CONFIG_DIR).join(FORGE_ENV_FILE));
    }
    files.push(cwd.join(crate::config::CONFIG_DIR).join(FORGE_ENV_FILE));
    files
}

/// Loads each file into the process environment without overriding anything.
///
/// A variable already in the environment, or set by an earlier file, keeps
/// its value. Call this before spawning threads or a runtime: setting
/// environment variables is only sound while the process is single-threaded.
///
/// # Arguments
///
/// * `paths` - The files, in priority order.
///
/// # Returns
///
/// The files that existed and were loaded, in order.
///
/// # Errors
///
/// When a file exists but cannot be read or parsed.
pub fn load(paths: &[PathBuf]) -> Result<Vec<PathBuf>, DotenvError> {
    let mut loaded = Vec::new();
    for path in paths {
        match dotenvy::from_path(path) {
            Ok(()) => loaded.push(path.clone()),
            Err(dotenvy::Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(DotenvError::Load {
                    path: path.clone(),
                    source,
                });
            }
        }
    }
    Ok(loaded)
}

/// Reads the variables a file defines without touching the environment.
///
/// # Arguments
///
/// * `path` - The `.env` file.
///
/// # Returns
///
/// The `(name, value)` pairs in file order; empty when the file is missing.
///
/// # Errors
///
/// When the file exists but cannot be read or parsed.
pub fn read_values(path: &Path) -> Result<Vec<(String, String)>, DotenvError> {
    let iter = match dotenvy::from_path_iter(path) {
        Ok(iter) => iter,
        Err(dotenvy::Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Vec::new());
        }
        Err(source) => {
            return Err(DotenvError::Load {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    iter.collect::<Result<Vec<_>, _>>()
        .map_err(|source| DotenvError::Load {
            path: path.to_path_buf(),
            source,
        })
}

/// Whether a value is a template placeholder rather than a real setting.
///
/// Templates write placeholders in angle brackets, such as
/// `<console.anthropic.com -> API keys>`; an empty value counts too.
///
/// # Arguments
///
/// * `value` - The value as read from the environment or a file.
///
/// # Returns
///
/// True for an empty value or one wrapped in `<` and `>`.
pub fn is_placeholder(value: &str) -> bool {
    let value = value.trim();
    value.is_empty() || (value.starts_with('<') && value.ends_with('>'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_file(dir: &Path, name: &str, text: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, text).expect("write");
        path
    }

    #[test]
    fn earlier_file_wins_and_missing_files_are_skipped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let first = env_file(dir.path(), "first.env", "FORGE_TEST_DOTENV_ORDER=first\n");
        let second = env_file(
            dir.path(),
            "second.env",
            "FORGE_TEST_DOTENV_ORDER=second\nFORGE_TEST_DOTENV_ONLY_SECOND=yes\n",
        );
        let missing = dir.path().join("missing.env");
        let loaded = load(&[first.clone(), missing, second.clone()]).expect("loads");
        assert_eq!(loaded, vec![first, second]);
        assert_eq!(
            std::env::var("FORGE_TEST_DOTENV_ORDER").as_deref(),
            Ok("first")
        );
        assert_eq!(
            std::env::var("FORGE_TEST_DOTENV_ONLY_SECOND").as_deref(),
            Ok("yes")
        );
    }

    #[test]
    fn the_real_environment_beats_a_file() {
        // Cargo sets CARGO_MANIFEST_DIR for every test binary.
        let before = std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets it");
        let dir = tempfile::tempdir().expect("tempdir");
        let file = env_file(dir.path(), "a.env", "CARGO_MANIFEST_DIR=/overridden\n");
        load(&[file]).expect("loads");
        assert_eq!(
            std::env::var("CARGO_MANIFEST_DIR").expect("still set"),
            before
        );
    }

    #[test]
    fn a_malformed_file_is_an_error_naming_the_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = env_file(dir.path(), "bad.env", "NOT VALID LINE WITH SPACES=x y\n");
        let error = load(std::slice::from_ref(&file)).expect_err("malformed");
        assert!(error.to_string().contains("bad.env"), "{error}");
    }

    #[test]
    fn read_values_does_not_touch_the_environment() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = env_file(
            dir.path(),
            "v.env",
            "# comment\nFORGE_TEST_DOTENV_READ_ONLY=\"<fill me>\"\nOTHER=x\n",
        );
        let values = read_values(&file).expect("reads");
        assert_eq!(
            values,
            vec![
                (
                    "FORGE_TEST_DOTENV_READ_ONLY".to_owned(),
                    "<fill me>".to_owned()
                ),
                ("OTHER".to_owned(), "x".to_owned()),
            ]
        );
        assert!(std::env::var("FORGE_TEST_DOTENV_READ_ONLY").is_err());
        assert!(
            read_values(&dir.path().join("none.env"))
                .expect("missing is empty")
                .is_empty()
        );
    }

    #[test]
    fn env_files_order_for_forge_and_factory() {
        let home = Path::new("/home/u");
        let cwd = Path::new("/proj");
        assert_eq!(
            env_files(Some(home), cwd, false),
            vec![
                home.join(".forge").join(".env"),
                cwd.join(".forge").join(".env")
            ]
        );
        assert_eq!(
            env_files(Some(home), cwd, true),
            vec![
                home.join(".factory").join("env"),
                home.join(".forge").join(".env"),
                cwd.join(".forge").join(".env")
            ]
        );
        assert_eq!(
            env_files(None, cwd, true),
            vec![cwd.join(".forge").join(".env")]
        );
    }

    #[test]
    fn placeholders_are_angle_bracketed_or_empty() {
        assert!(is_placeholder(""));
        assert!(is_placeholder("  "));
        assert!(is_placeholder("<console.anthropic.com -> API keys>"));
        assert!(is_placeholder(" <x> "));
        assert!(!is_placeholder("sk-real"));
        assert!(!is_placeholder("<unclosed"));
    }
}
