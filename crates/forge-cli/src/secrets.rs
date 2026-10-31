//! `forge scan-keys` and the `secrets` doctor row: no API key in a tracked file.
//!
//! A hit is reported by path, line and kind, never by value.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};
use forge_types::dotenv::{is_placeholder, read_values};
use regex::Regex;

/// Shortest `.env` value that is treated as a secret worth scanning for.
const MIN_VALUE_LEN: usize = 12;

/// Key shapes to look for: a label and a pattern. The patterns are written so
/// that this source file never matches itself.
const PATTERNS: &[(&str, &str)] = &[
    ("sk- key", "sk-[A-Za-z0-9_-]{20,}"),
    ("AIza key", "AIza[0-9A-Za-z_-]{35}"),
    ("ghp_ token", "ghp_[A-Za-z0-9]{36}"),
    ("github_pat_ token", "github_pat_[A-Za-z0-9_]{22,}"),
    ("hf_ token", "hf_[A-Za-z0-9]{20,}"),
];

/// One place a key-looking string was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    /// The file, relative to the repository root with forward slashes.
    pub path: String,
    /// The 1-based line number.
    pub line: usize,
    /// What matched: a pattern label or `value of <NAME>`.
    pub kind: String,
}

impl std::fmt::Display for Hit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}: {}", self.path, self.line, self.kind)
    }
}

/// Something to search for.
enum Needle {
    /// A regular expression with its label.
    Pattern(&'static str, Regex),
    /// A literal value from a `.env` file, with the variable's name.
    Value(String, String),
}

/// Scans every tracked file and prints the hits.
///
/// # Arguments
///
/// * `cwd` - A directory inside the repository.
/// * `env_files` - The `.env` files whose values count as keys.
///
/// # Returns
///
/// Zero when nothing was found, one otherwise.
///
/// # Errors
///
/// When `cwd` is not inside a git repository or a `.env` file is malformed.
pub fn run(cwd: &Path, env_files: &[PathBuf]) -> anyhow::Result<i32> {
    let Some(root) = git_toplevel(cwd) else {
        bail!("{} is not inside a git repository", cwd.display());
    };
    let values = env_values(env_files)?;
    let hits = scan_tree(&root, &values)?;
    for hit in &hits {
        println!("{hit}");
    }
    if hits.is_empty() {
        println!("scan-keys: no API keys in tracked files");
        Ok(0)
    } else {
        println!(
            "scan-keys: {} hit(s); remove the values, rotate the keys and commit again",
            hits.len()
        );
        Ok(1)
    }
}

/// The repository root that contains `cwd`, if any.
///
/// # Arguments
///
/// * `cwd` - Any directory.
///
/// # Returns
///
/// The root, or `None` outside a repository or without git.
pub fn git_toplevel(cwd: &Path) -> Option<PathBuf> {
    let out = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    let root = text.trim();
    (!root.is_empty()).then(|| PathBuf::from(root.replace('/', std::path::MAIN_SEPARATOR_STR)))
}

/// The `.env` values that count as secrets, with their variable names.
///
/// Placeholders, short values and values with whitespace are left out, so
/// settings like `FORGE_LOG=info` do not flag half the repository.
///
/// # Arguments
///
/// * `env_files` - The files, present or not.
///
/// # Returns
///
/// `(name, value)` pairs.
///
/// # Errors
///
/// When a file exists but is malformed.
pub fn env_values(env_files: &[PathBuf]) -> anyhow::Result<Vec<(String, String)>> {
    let mut values = Vec::new();
    for path in env_files {
        for (name, value) in read_values(path)? {
            let value = value.trim().to_owned();
            let usable = !is_placeholder(&value)
                && value.len() >= MIN_VALUE_LEN
                && !value.chars().any(char::is_whitespace);
            if usable {
                values.push((name, value));
            }
        }
    }
    Ok(values)
}

/// Scans every file git tracks under `root`.
///
/// # Arguments
///
/// * `root` - The repository root.
/// * `values` - `.env` values that count as keys, with their names.
///
/// # Returns
///
/// The hits, in file order. Files that are not UTF-8 are skipped.
///
/// # Errors
///
/// When `git ls-files` fails.
pub fn scan_tree(root: &Path, values: &[(String, String)]) -> anyhow::Result<Vec<Hit>> {
    let out = Command::new("git")
        .args(["ls-files", "-z"])
        .current_dir(root)
        .output()
        .context("running git ls-files")?;
    if !out.status.success() {
        bail!(
            "git ls-files failed in {}: {}",
            root.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let needles = needles(values);
    let mut hits = Vec::new();
    for relative in String::from_utf8_lossy(&out.stdout).split('\0') {
        if relative.is_empty() {
            continue;
        }
        let Ok(bytes) = std::fs::read(root.join(relative)) else {
            continue;
        };
        let Ok(text) = String::from_utf8(bytes) else {
            continue;
        };
        hits.extend(scan_text(relative, &text, &needles));
    }
    Ok(hits)
}

/// Compiles the patterns and pairs them with the literal values.
fn needles(values: &[(String, String)]) -> Vec<Needle> {
    let mut needles: Vec<Needle> = PATTERNS
        .iter()
        .filter_map(|(label, pattern)| {
            Regex::new(pattern)
                .ok()
                .map(|re| Needle::Pattern(label, re))
        })
        .collect();
    needles.extend(
        values
            .iter()
            .map(|(name, value)| Needle::Value(name.clone(), value.clone())),
    );
    needles
}

/// Finds every needle in one file's text.
fn scan_text(path: &str, text: &str, needles: &[Needle]) -> Vec<Hit> {
    let mut hits = Vec::new();
    for (index, line) in text.lines().enumerate() {
        for needle in needles {
            let kind = match needle {
                Needle::Pattern(label, re) if re.is_match(line) => (*label).to_owned(),
                Needle::Value(name, value) if line.contains(value.as_str()) => {
                    format!("value of {name}")
                }
                _ => continue,
            };
            hits.push(Hit {
                path: path.to_owned(),
                line: index + 1,
                kind,
            });
        }
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake(prefix: &str, len: usize) -> String {
        format!("{prefix}{}", "a".repeat(len))
    }

    #[test]
    fn each_prefix_is_caught_and_the_value_is_not_printed() {
        let needles = needles(&[]);
        let cases = [
            (fake("sk-", 40), "sk- key"),
            (fake("AIza", 35), "AIza key"),
            (fake("ghp_", 36), "ghp_ token"),
            (fake("github_pat_", 60), "github_pat_ token"),
            (fake("hf_", 30), "hf_ token"),
        ];
        for (value, kind) in &cases {
            let text = format!("line one\nkey = \"{value}\"\n");
            let hits = scan_text("f.toml", &text, &needles);
            assert_eq!(hits.len(), 1, "{kind}");
            assert_eq!(hits[0].line, 2);
            assert_eq!(hits[0].kind, *kind);
            assert!(!hits[0].to_string().contains(value.as_str()));
        }
        assert!(scan_text("f", "sk-short\nghp_x\nnothing here", &needles).is_empty());
    }

    #[test]
    fn env_values_are_caught_by_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let env = dir.path().join(".env");
        std::fs::write(
            &env,
            "GITHUB_TOKEN=realtokenvalue123\nFORGE_LOG=info\nEMPTY=\"<fill>\"\nWEBHOOK_URL=\"has space here\"\n",
        )
        .expect("write");
        let values = env_values(&[env, dir.path().join("missing")]).expect("reads");
        assert_eq!(
            values,
            vec![("GITHUB_TOKEN".to_owned(), "realtokenvalue123".to_owned())]
        );
        let hits = scan_text("x.md", "token: realtokenvalue123\n", &needles(&values));
        assert_eq!(hits[0].kind, "value of GITHUB_TOKEN");
    }

    #[test]
    fn scan_tree_reads_only_tracked_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(root)
                .output()
                .expect("git runs");
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init", "-q"]);
        std::fs::write(
            root.join("tracked.txt"),
            format!("x {}\n", fake("ghp_", 36)),
        )
        .expect("write");
        std::fs::write(root.join("untracked.txt"), fake("ghp_", 36)).expect("write");
        git(&["add", "tracked.txt"]);
        let hits = scan_tree(root, &[]).expect("scans");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "tracked.txt");
        assert_eq!(git_toplevel(root).map(|p| p.is_dir()), Some(true));
        assert_eq!(scan_tree(root, &[]).expect("scans").len(), 1);
    }
}
