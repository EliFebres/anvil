//! The workspace jail: which paths anvil's own file tools may open.
//!
//! Every file tool (`read_file`, `write_file`, `edit_file`, `glob`, `grep`,
//! `list_dir`, `outline`) resolves its path, symlinks and `..` included,
//! before it opens anything. The resolved path must lie inside the project
//! root, the session folder, or a folder the user listed in
//! `[tools] extra_dirs`; installed skill folders may also be read. On top
//! of that, files that hold secrets or credentials are refused wherever
//! they are, on every operating system.
//!
//! This confines anvil's file tools only. A `shell` command the user
//! approves runs with the user's own rights and is not confined here.

use std::path::{Component, Path, PathBuf};

/// Folders outside the project root that the tools may also open.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Workspace {
    /// Folders the tools may read and write, from `[tools] extra_dirs`.
    pub extra_dirs: Vec<PathBuf>,
    /// Folders the tools may only read, such as installed skills.
    pub read_only_dirs: Vec<PathBuf>,
}

/// Path pieces that mark a credential, key or browser store, as consecutive
/// path components compared without regard to case. Any path that contains
/// one of these sequences is refused.
const SECRET_DIRS: &[&[&str]] = &[
    // SSH, cloud and cluster credentials.
    &[".ssh"],
    &[".aws"],
    &[".azure"],
    &[".config", "gcloud"],
    &[".kube"],
    &[".docker", "config.json"],
    &[".gnupg"],
    &[".password-store"],
    &[".anvil", ".env"],
    // Browser profiles: Linux, macOS, Windows.
    &[".mozilla"],
    &[".config", "google-chrome"],
    &[".config", "chromium"],
    &[".config", "microsoft-edge"],
    &["library", "application support", "google", "chrome"],
    &["library", "application support", "firefox"],
    &["library", "application support", "microsoft edge"],
    &["library", "cookies"],
    &["appdata", "local", "google", "chrome", "user data"],
    &["appdata", "local", "microsoft", "edge", "user data"],
    &["appdata", "roaming", "mozilla", "firefox"],
    // Keychains and credential stores: macOS, Linux, Windows.
    &["library", "keychains"],
    &[".local", "share", "keyrings"],
    &["appdata", "roaming", "microsoft", "credentials"],
    &["appdata", "local", "microsoft", "credentials"],
    &["appdata", "roaming", "microsoft", "protect"],
];

/// File names that hold credentials wherever they are.
const SECRET_FILES: &[&str] = &[".netrc", "_netrc", ".git-credentials", ".pgpass"];

/// Suffixes of `.env.*` files that hold examples rather than secrets.
const ENV_EXAMPLE_SUFFIXES: &[&str] = &[".example", ".sample", ".template"];

/// Whether `path` is, or lies inside, a place that holds secrets: a `.env`
/// file, an SSH, cloud or GPG folder, a browser profile, or a keychain.
///
/// # Arguments
///
/// * `path` - The path, ideally already resolved.
///
/// # Returns
///
/// `true` when the file tools must not open it.
pub fn holds_secrets(path: &Path) -> bool {
    let parts: Vec<String> = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().to_lowercase()),
            _ => None,
        })
        .collect();
    if let Some(name) = parts.last()
        && (is_env_file(name) || SECRET_FILES.contains(&name.as_str()))
    {
        return true;
    }
    SECRET_DIRS.iter().any(|pattern| {
        parts
            .windows(pattern.len())
            .any(|window| window.iter().zip(pattern.iter()).all(|(a, b)| a == b))
    })
}

/// Whether `path` is, or lies inside, a git directory: a `.git` folder or
/// file at any depth (which covers worktrees and submodules, whose git
/// directories live under the main `.git`), or a folder git itself would
/// take for one because it holds `HEAD`, `objects` and `refs` (a bare
/// repository or a separate git directory).
///
/// Writing there could plant a hook or change the config that git, and
/// anvil's own git calls, read.
///
/// # Arguments
///
/// * `path` - The path, already resolved.
///
/// # Returns
///
/// `true` when the file tools must not write to it.
pub fn inside_git_dir(path: &Path) -> bool {
    let named_git = path.components().any(
        |component| matches!(component, Component::Normal(part) if part.eq_ignore_ascii_case(".git")),
    );
    named_git || path.ancestors().any(is_git_directory)
}

/// Whether `dir` has the three entries git checks to recognise a git directory.
fn is_git_directory(dir: &Path) -> bool {
    dir.join("HEAD").is_file() && dir.join("objects").is_dir() && dir.join("refs").is_dir()
}

/// Whether a lower-cased file name is `.env` or a `.env.*` that is not an example.
fn is_env_file(name: &str) -> bool {
    name == ".env"
        || (name.starts_with(".env.")
            && !ENV_EXAMPLE_SUFFIXES
                .iter()
                .any(|suffix| name.ends_with(suffix)))
}

/// Whether `path` lies inside `dir`, comparing both in their canonical form.
///
/// # Arguments
///
/// * `dir` - The folder.
/// * `path` - A path already resolved against the file system.
///
/// # Returns
///
/// `true` when `path` is `dir` or below it.
pub fn is_within(dir: &Path, path: &Path) -> bool {
    path.starts_with(canonical(dir))
}

/// The canonical form of `path`, or `path` itself when it cannot be resolved.
pub(crate) fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_files_hold_secrets_and_examples_do_not() {
        for path in [".env", "a/.env", "a/.env.local", "a/.ENV.Production"] {
            assert!(holds_secrets(Path::new(path)), "{path}");
        }
        for path in [
            ".env.example",
            "a/.env.sample",
            "env.rs",
            "src/environment.md",
        ] {
            assert!(!holds_secrets(Path::new(path)), "{path}");
        }
    }

    #[test]
    fn credential_and_browser_stores_hold_secrets_on_every_system() {
        for path in [
            "/home/u/.ssh/id_ed25519",
            "/home/u/.aws/credentials",
            "/home/u/.config/gcloud/application_default_credentials.json",
            "/home/u/.kube/config",
            "/home/u/.docker/config.json",
            "/home/u/.netrc",
            "/home/u/.git-credentials",
            "/home/u/.anvil/.env",
            "/home/u/.mozilla/firefox/x.default/logins.json",
            "/home/u/.local/share/keyrings/login.keyring",
            "/Users/u/Library/Keychains/login.keychain-db",
            "/Users/u/Library/Application Support/Google/Chrome/Default/Cookies",
            r"C:\Users\u\AppData\Local\Google\Chrome\User Data\Default\Login Data",
            r"C:\Users\u\AppData\Local\Microsoft\Edge\User Data\Default\Login Data",
            r"C:\Users\u\AppData\Roaming\Microsoft\Credentials\ABC",
            r"C:\Users\u\AppData\Roaming\Microsoft\Protect\S-1-5",
        ] {
            let path = path.replace(['\\', '/'], std::path::MAIN_SEPARATOR_STR);
            assert!(holds_secrets(Path::new(&path)), "{path}");
        }
    }

    #[test]
    fn ordinary_project_files_do_not_hold_secrets() {
        for path in [
            "src/main.rs",
            "docs/ssh.md",
            ".github/workflows/ci.yml",
            "config/docker.json",
            "library/cookies.rs",
        ] {
            assert!(!holds_secrets(Path::new(path)), "{path}");
        }
    }

    #[test]
    fn within_compares_canonical_paths() {
        let dir = tempfile::tempdir().expect("dir");
        std::fs::create_dir(dir.path().join("sub")).expect("mkdir");
        let inside = canonical(&dir.path().join("sub"));
        assert!(is_within(dir.path(), &inside));
        assert!(!is_within(&dir.path().join("sub"), &canonical(dir.path())));
    }
}
