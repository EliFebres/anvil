//! The secret scan: no key value may sit in a worktree that is about to be pushed.

use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::git::Git;

/// Shortest value worth scanning for; shorter strings would match by accident.
const MIN_SECRET_LEN: usize = 8;

/// The files of a worktree that hold any of the secret values.
///
/// Scans what `git ls-files -co --exclude-standard` lists: tracked and
/// untracked files that are not ignored, so everything a push could carry.
/// `target/` and `.anvil/` are ignored and skipped.
///
/// # Arguments
///
/// * `worktree` - The worktree.
/// * `secrets` - The values; empty and very short ones are ignored.
///
/// # Returns
///
/// The offending files, repository-relative, sorted.
///
/// # Errors
///
/// When git fails or a listed file cannot be read.
pub async fn scan(worktree: &Path, secrets: &[String]) -> Result<Vec<PathBuf>> {
    let secrets: Vec<&[u8]> = secrets
        .iter()
        .filter(|s| s.len() >= MIN_SECRET_LEN)
        .map(|s| s.as_bytes())
        .collect();
    if secrets.is_empty() {
        return Ok(Vec::new());
    }
    let git = Git::new(worktree);
    let listing = git
        .run(&["ls-files", "-co", "--exclude-standard", "-z"])
        .await?;
    let mut hits = Vec::new();
    for name in listing.split('\0').filter(|name| !name.is_empty()) {
        let path = worktree.join(name);
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        if secrets.iter().any(|secret| contains(&bytes, secret)) {
            hits.push(PathBuf::from(name));
        }
    }
    hits.sort();
    Ok(hits)
}

/// Whether `haystack` holds `needle`.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(dir: &Path, args: &[&str]) -> bool {
        std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .is_ok_and(|out| out.status.success())
    }

    #[tokio::test]
    async fn finds_secrets_in_tracked_and_untracked_files_but_not_ignored_ones() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path();
        if !setup(dir, &["init", "-q"]) {
            eprintln!("git not available; skipping");
            return;
        }
        std::fs::write(dir.join(".gitignore"), "target/\n").expect("write");
        std::fs::write(dir.join("clean.rs"), "fn main() {}\n").expect("write");
        std::fs::write(
            dir.join("tracked.rs"),
            "const K: &str = \"sk-live-123456789\";\n",
        )
        .expect("write");
        assert!(setup(dir, &["add", "."]));
        std::fs::write(dir.join("untracked.txt"), "token ghp_abcdefghij here\n").expect("write");
        std::fs::create_dir_all(dir.join("target")).expect("mkdir");
        std::fs::write(dir.join("target/out.log"), "sk-live-123456789\n").expect("write");

        let hits = scan(
            dir,
            &[
                "sk-live-123456789".to_owned(),
                "ghp_abcdefghij".to_owned(),
                "short".to_owned(),
                String::new(),
            ],
        )
        .await
        .expect("scans");
        assert_eq!(
            hits,
            vec![PathBuf::from("tracked.rs"), PathBuf::from("untracked.txt")]
        );
        assert!(
            scan(dir, &["nothing-here-at-all".to_owned()])
                .await
                .expect("scans")
                .is_empty()
        );
        assert!(scan(dir, &[]).await.expect("scans").is_empty());
    }
}
