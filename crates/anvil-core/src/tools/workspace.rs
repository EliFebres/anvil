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

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

/// Compiled path globs the file tools refuse; see [`deny_globs`].
pub type DeniedPaths = GlobSet;

/// Folders outside the project root that the tools may also open, and
/// paths they refuse on top of the built-in secret list.
#[derive(Debug, Clone, Default)]
pub struct Workspace {
    /// Folders the tools may read and write, from `[tools] extra_dirs`.
    pub extra_dirs: Vec<PathBuf>,
    /// Folders the tools may only read, such as installed skills and a
    /// managed policy's `[files] extra_roots`.
    pub read_only_dirs: Vec<PathBuf>,
    /// Path globs the tools refuse and searches skip, from a managed
    /// policy's `[files] deny`; see [`deny_globs`].
    pub denied: Option<DeniedPaths>,
}

impl Workspace {
    /// Whether the file tools must not open `path`: it holds secrets
    /// ([`holds_secrets`]) or matches a denied glob.
    ///
    /// # Arguments
    ///
    /// * `path` - The path, ideally already resolved.
    ///
    /// # Returns
    ///
    /// `true` when the tools refuse it and searches skip it.
    pub fn refuses(&self, path: &Path) -> bool {
        holds_secrets(path)
            || self
                .denied
                .as_ref()
                .is_some_and(|globs| globs.is_match(path))
    }
}

/// Compiles path globs for [`Workspace::denied`], without regard to case.
///
/// # Arguments
///
/// * `patterns` - Globs such as `**/*.kdbx`, matched against the whole
///   resolved path with `/` as the separator on every system.
///
/// # Returns
///
/// The set, or `None` when there are no patterns.
///
/// # Errors
///
/// A message naming the first pattern that does not compile.
pub fn deny_globs(patterns: &[String]) -> Result<Option<DeniedPaths>, String> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = GlobBuilder::new(pattern)
            .case_insensitive(true)
            .literal_separator(true)
            .build()
            .map_err(|error| format!("`{pattern}` is not a valid glob: {error}"))?;
        builder.add(glob);
    }
    builder
        .build()
        .map(Some)
        .map_err(|error| format!("the deny globs do not compile: {error}"))
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

/// Folders whose settings files can define hooks, lower-cased.
const HOOK_SETTINGS_DIRS: &[&str] = &[".claude", ".anvil"];

/// Settings file names that can define hooks, lower-cased.
const HOOK_SETTINGS_FILES: &[&str] = &["settings.json", "settings.local.json"];

/// Whether `path` is a settings file that can define hooks:
/// `settings.json` or `settings.local.json` in a `.claude` or `.anvil`
/// folder, anywhere. A hook written there runs in a later session without
/// a prompt, so a write to one always needs a person's answer.
///
/// # Arguments
///
/// * `path` - The path, already resolved.
///
/// # Returns
///
/// `true` for such a file.
pub fn is_hook_settings(path: &Path) -> bool {
    let lower = |part: Option<&std::ffi::OsStr>| {
        part.map(|part| part.to_string_lossy().to_lowercase())
            .unwrap_or_default()
    };
    let name = lower(path.file_name());
    let folder = lower(path.parent().and_then(Path::file_name));
    HOOK_SETTINGS_FILES.contains(&name.as_str()) && HOOK_SETTINGS_DIRS.contains(&folder.as_str())
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

    #[test]
    fn denied_globs_match_whole_paths_without_regard_to_case() {
        let workspace = Workspace {
            denied: deny_globs(&["**/*.kdbx".to_owned(), "**/secrets/**".to_owned()])
                .expect("compiles"),
            ..Workspace::default()
        };
        assert!(workspace.refuses(Path::new("/work/repo/Vault.KDBX")));
        assert!(workspace.refuses(Path::new("/work/repo/secrets/a.txt")));
        assert!(!workspace.refuses(Path::new("/work/repo/src/main.rs")));
        assert!(
            Workspace::default().refuses(Path::new("/work/.env")),
            "the built-in list"
        );
        assert!(deny_globs(&["a[".to_owned()]).is_err());
        assert!(deny_globs(&[]).expect("empty").is_none());
    }
}
