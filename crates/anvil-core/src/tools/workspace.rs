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
    /// Folders the tools never write, wherever they lie: the user's own
    /// `~/.anvil`, which holds the configuration, saved permission rules
    /// and trust decisions.
    pub protected_dirs: Vec<PathBuf>,
    /// Files the tools never write, in canonical form: the scripts and
    /// programs inside the workspace that a loaded hook or MCP server
    /// runs, found with [`command_files`]. A model that could rewrite one
    /// would run code later without a prompt.
    pub protected_files: Vec<PathBuf>,
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
    // A repository's own git config can hold remote URLs with tokens.
    &[".git", "config"],
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
const SECRET_FILES: &[&str] = &[
    ".netrc",
    "_netrc",
    ".git-credentials",
    ".pgpass",
    ".npmrc",
    ".pypirc",
];

/// Starts of file names that hold private keys or credentials: SSH keys
/// (`id_rsa`, `id_ed25519` and their `.pub` and backup copies) and any
/// `credentials*` file.
const SECRET_PREFIXES: &[&str] = &["id_rsa", "id_ed25519", "credentials"];

/// Extensions of key and certificate stores.
const SECRET_EXTENSIONS: &[&str] = &[".pem", ".key", ".pfx", ".p12"];

/// Suffixes of `.env.*` files that hold examples rather than secrets.
const ENV_EXAMPLE_SUFFIXES: &[&str] = &[".example", ".sample", ".template"];

/// Whether `path` is, or lies inside, a place that holds secrets: a `.env`
/// file, a key or certificate file (`*.pem`, `*.key`, `*.pfx`, `*.p12`,
/// `id_rsa*`, `id_ed25519*`), a credentials file (`credentials*`,
/// `.netrc`, `.npmrc`, `.pypirc`, `.git-credentials`), a repository's
/// `.git/config`, an SSH, cloud or GPG folder, a browser profile, or a
/// keychain.
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
        && (is_env_file(name)
            || SECRET_FILES.contains(&name.as_str())
            || SECRET_PREFIXES
                .iter()
                .any(|prefix| name.starts_with(prefix))
            || SECRET_EXTENSIONS
                .iter()
                .any(|extension| name.ends_with(extension)))
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

/// Files in a `.anvil` folder that grant power, lower-cased: the
/// repository's rules and the settings that define hooks and MCP servers.
const ANVIL_POWER_FILES: &[&str] = &["config.toml", "permissions.toml"];

/// Whether `path` is a file that grants anvil, or a tool it runs, more
/// power: `.anvil/config.toml`, `.anvil/permissions.toml`, a
/// `settings*.json` in a `.claude` or `.anvil` folder (hooks, MCP servers,
/// permissions), or a `.mcp.json`, anywhere and in any letter case. A
/// model that could write one could raise its own permissions or plant a
/// program that runs later without a prompt, so file tools never write
/// them.
///
/// # Arguments
///
/// * `path` - The path, already resolved.
///
/// # Returns
///
/// `true` for such a file.
pub fn grants_power(path: &Path) -> bool {
    let lower = |part: Option<&std::ffi::OsStr>| {
        part.map(|part| part.to_string_lossy().to_lowercase())
            .unwrap_or_default()
    };
    let name = lower(path.file_name());
    let folder = lower(path.parent().and_then(Path::file_name));
    let settings = name.starts_with("settings") && name.ends_with(".json");
    name == ".mcp.json"
        || (folder == ".anvil" && (settings || ANVIL_POWER_FILES.contains(&name.as_str())))
        || (folder == ".claude" && settings)
}

/// The path Windows opens for `path`: trailing dots and spaces stripped
/// from every component, as the Win32 layer does before it reaches the
/// file system. The same folding runs on every system so the check is
/// testable everywhere.
///
/// # Arguments
///
/// * `path` - The path as the model wrote it.
///
/// # Returns
///
/// The folded path, or `None` when a component contains `:` (an
/// alternate data stream such as `::$DATA`) or folds to nothing.
pub fn win32_fold(path: &Path) -> Option<PathBuf> {
    let mut folded = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(name) => {
                let name = name.to_string_lossy();
                let trimmed = name.trim_end_matches(['.', ' ']);
                if trimmed.is_empty() || name.contains(':') {
                    return None;
                }
                folded.push(trimmed);
            }
            other => folded.push(other),
        }
    }
    Some(folded)
}

/// The files inside `root` that command lines name: every word, and the
/// value after `=` in a `--key=value` word, that resolves (relative to
/// `root`) to an existing file inside `root`.
///
/// Words are split at white space, quotes and the shell's `;`, `|`, `&`,
/// `(`, `)`, `<` and `>`, so `sh "scripts/fmt.sh" && ./x` names both
/// scripts. It errs towards naming too much: a word that only happens to
/// match a file still protects that file.
///
/// # Arguments
///
/// * `root` - The project root.
/// * `lines` - Command lines: hook commands, and an MCP server's program
///   and arguments.
///
/// # Returns
///
/// The files, canonical, sorted and without duplicates.
pub fn command_files<'a>(root: &Path, lines: impl IntoIterator<Item = &'a str>) -> Vec<PathBuf> {
    let base = canonical(root);
    let mut files = std::collections::BTreeSet::new();
    for line in lines {
        let words = line.split(|c: char| c.is_whitespace() || "\"'`;|&()<>".contains(c));
        for word in words.filter(|word| !word.is_empty()) {
            let value = word.split_once('=').map(|(_, value)| value);
            for candidate in std::iter::once(word).chain(value) {
                let path = base.join(candidate);
                if path.is_file()
                    && let Ok(path) = std::fs::canonicalize(&path)
                    && path.starts_with(&base)
                {
                    files.insert(path);
                }
            }
        }
    }
    files.into_iter().collect()
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
    fn keys_certificates_and_credential_files_hold_secrets_anywhere() {
        for path in [
            "certs/server.pem",
            "deploy/tls.KEY",
            "win/cert.pfx",
            "win/cert.p12",
            "keys/id_rsa",
            "keys/id_rsa.pub",
            "backup/id_ed25519.old",
            "credentials",
            "config/credentials.json",
            "home/.npmrc",
            "home/.pypirc",
            "repo/.git/config",
            "repo/.GIT/Config",
        ] {
            let path = path.replace('/', std::path::MAIN_SEPARATOR_STR);
            assert!(holds_secrets(Path::new(&path)), "{path}");
        }
        for path in [
            "src/keyboard.rs",
            "docs/pem-format.md",
            "src/credential_store.rs",
            ".git/HEAD",
            "config/git.toml",
        ] {
            assert!(!holds_secrets(Path::new(path)), "{path}");
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
    fn files_that_grant_power_are_recognised_in_any_case_and_folder() {
        for path in [
            ".anvil/config.toml",
            ".anvil/permissions.toml",
            ".anvil/settings.json",
            ".anvil/settings.local.json",
            ".claude/settings.json",
            ".claude/settings.local.json",
            ".claude/settings.dev.json",
            ".mcp.json",
            "sub/.mcp.json",
            "sub/.ANVIL/Permissions.TOML",
            ".Claude/Settings.Local.JSON",
        ] {
            let path = path.replace('/', std::path::MAIN_SEPARATOR_STR);
            assert!(grants_power(Path::new(&path)), "{path}");
        }
        for path in [
            "config.toml",
            "permissions.toml",
            "settings.json",
            ".claude/agents/a.md",
            ".anvil/rules.toml",
            "mcp.json",
        ] {
            assert!(!grants_power(Path::new(path)), "{path}");
        }
    }

    #[test]
    fn win32_fold_strips_trailing_dots_and_spaces_and_refuses_streams() {
        let fold = |text: &str| win32_fold(Path::new(text));
        assert_eq!(
            fold(".anvil/config.toml."),
            Some(PathBuf::from(".anvil/config.toml"))
        );
        assert_eq!(
            fold(".anvil/permissions.toml "),
            Some(PathBuf::from(".anvil/permissions.toml"))
        );
        assert_eq!(
            fold(".anvil./config.toml"),
            Some(PathBuf::from(".anvil/config.toml"))
        );
        assert_eq!(fold(".mcp.json. . "), Some(PathBuf::from(".mcp.json")));
        assert_eq!(fold("../a/./b.txt"), Some(PathBuf::from("../a/b.txt")));
        assert_eq!(fold("src/main.rs"), Some(PathBuf::from("src/main.rs")));
        assert_eq!(fold(".claude/settings.json::$DATA"), None);
        assert_eq!(fold("notes.txt:hidden"), None);
        assert_eq!(
            fold("a/.../b"),
            None,
            "a name of only dots folds to nothing"
        );
    }

    #[test]
    fn command_files_names_existing_files_inside_the_root_only() {
        let dir = tempfile::tempdir().expect("dir");
        let root = canonical(dir.path());
        std::fs::create_dir_all(root.join("scripts")).expect("mkdir");
        std::fs::write(root.join("scripts").join("fmt.sh"), "x").expect("write");
        std::fs::write(root.join("server.toml"), "x").expect("write");
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(outside.path().join("o.sh"), "x").expect("write");
        let far = outside.path().join("o.sh").display().to_string();
        let files = command_files(
            &root,
            [
                "sh 'scripts/fmt.sh' && ./scripts/../missing.sh",
                "--config=server.toml",
                far.as_str(),
                "node",
            ],
        );
        assert_eq!(
            files,
            vec![
                canonical(&root.join("scripts").join("fmt.sh")),
                canonical(&root.join("server.toml")),
            ]
        );
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
