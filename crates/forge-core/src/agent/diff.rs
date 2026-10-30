//! The diff of everything the session changed, via git tree snapshots.
//!
//! At session start the working tree (tracked and untracked, minus ignored
//! files and `.forge/`) is written to a tree object through a private index
//! file. The review diff is `git diff <start tree> <current tree>`, so
//! changes that were already uncommitted before the session stay out of it.

use std::path::{Path, PathBuf};

use tokio::process::Command;

/// Bytes of diff kept before the text is cut for the reviewer.
pub const MAX_DIFF_BYTES: usize = 200 * 1024;

/// Name of the private index file inside the session folder.
const INDEX_FILE: &str = "git-index";

/// Writes the current working tree to a tree object.
///
/// # Arguments
///
/// * `cwd` - The project root.
/// * `session_dir` - Where the private index file lives.
///
/// # Returns
///
/// The tree hash, or `None` when `cwd` is not inside a git repository or
/// git is unavailable.
pub async fn snapshot_tree(cwd: &Path, session_dir: &Path) -> Option<String> {
    let index = session_dir.join(INDEX_FILE);
    let _ = std::fs::remove_file(&index);
    let _ = std::fs::create_dir_all(session_dir);
    git(cwd, &index, &["add", "-A", "--", ".", ":(exclude).forge"]).await?;
    let tree = git(cwd, &index, &["write-tree"]).await?;
    let tree = tree.trim().to_owned();
    if tree.is_empty() { None } else { Some(tree) }
}

/// The diff between a snapshot and the working tree now.
///
/// # Arguments
///
/// * `cwd` - The project root.
/// * `session_dir` - Where the private index file lives.
/// * `baseline` - A tree hash from [`snapshot_tree`].
///
/// # Returns
///
/// The unified diff, cut at [`MAX_DIFF_BYTES`] with a note, or `None` when
/// git is unavailable.
pub async fn diff_since(cwd: &Path, session_dir: &Path, baseline: &str) -> Option<String> {
    let current = snapshot_tree(cwd, session_dir).await?;
    let index = session_dir.join(INDEX_FILE);
    let diff = git(cwd, &index, &["diff", "--no-color", baseline, &current]).await?;
    Some(cap(diff))
}

/// The files that differ between a snapshot and the working tree now.
///
/// # Arguments
///
/// * `cwd` - The project root.
/// * `session_dir` - Where the private index file lives.
/// * `baseline` - A tree hash from [`snapshot_tree`].
///
/// # Returns
///
/// Absolute paths of added, copied, modified and renamed files, or `None`
/// when git is unavailable. Deleted files are left out.
pub async fn changed_files(cwd: &Path, session_dir: &Path, baseline: &str) -> Option<Vec<PathBuf>> {
    let current = snapshot_tree(cwd, session_dir).await?;
    let index = session_dir.join(INDEX_FILE);
    let names = git(
        cwd,
        &index,
        &[
            "-c",
            "core.quotepath=false",
            "diff",
            "--name-only",
            "--diff-filter=ACMR",
            baseline,
            &current,
        ],
    )
    .await?;
    Some(
        names
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(|line| cwd.join(line))
            .collect(),
    )
}

/// Runs git with a private index file and returns stdout on success.
async fn git(cwd: &Path, index: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_INDEX_FILE", absolute(index))
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        tracing::debug!(
            args = ?args,
            stderr = %String::from_utf8_lossy(&output.stderr),
            "git command failed"
        );
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Makes the index path absolute so git resolves it independently of `cwd`.
fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|dir| dir.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

/// Cuts an oversized diff on a line boundary.
fn cap(mut diff: String) -> String {
    if diff.len() <= MAX_DIFF_BYTES {
        return diff;
    }
    let total = diff.len();
    let cut = diff[..MAX_DIFF_BYTES].rfind('\n').unwrap_or(MAX_DIFF_BYTES);
    diff.truncate(cut);
    diff.push_str(&format!(
        "\n[diff truncated: {cut} of {total} bytes shown]\n"
    ));
    diff
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_sync(cwd: &Path, args: &[&str]) -> bool {
        std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false)
    }

    #[tokio::test]
    async fn diff_covers_session_changes_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        if !git_sync(root, &["init", "-q"]) {
            eprintln!("git not available; skipping");
            return;
        }
        std::fs::write(root.join(".gitignore"), "ignored.txt\n").expect("write");
        std::fs::write(root.join("a.txt"), "one\n").expect("write");
        assert!(git_sync(root, &["add", "-A"]));
        assert!(git_sync(
            root,
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-qm",
                "init"
            ]
        ));
        // A change that predates the session must not appear in the diff.
        std::fs::write(root.join("a.txt"), "one\ntwo\n").expect("write");

        let session = root.join(".forge").join("sessions").join("s1");
        let baseline = snapshot_tree(root, &session).await.expect("snapshot");
        std::fs::write(root.join("a.txt"), "one\ntwo\nthree\n").expect("write");
        std::fs::write(root.join("new.txt"), "fresh\n").expect("write");
        std::fs::write(root.join("ignored.txt"), "nope\n").expect("write");
        std::fs::write(session.join("plan.md"), "plan").expect("write");

        let diff = diff_since(root, &session, &baseline).await.expect("diff");
        assert!(diff.contains("+three"), "{diff}");
        assert!(
            diff.contains("new.txt") && diff.contains("+fresh"),
            "{diff}"
        );
        assert!(!diff.contains("+two"), "pre-session change leaked: {diff}");
        assert!(!diff.contains("ignored.txt"), "{diff}");
        assert!(!diff.contains("plan.md"), "{diff}");
        assert!(root.join("a.txt").exists(), "working tree untouched");

        let changed = changed_files(root, &session, &baseline)
            .await
            .expect("changed files");
        assert_eq!(changed, vec![root.join("a.txt"), root.join("new.txt")]);
    }

    #[tokio::test]
    async fn outside_a_repo_is_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let session = dir.path().join("s");
        // A temp dir may sit inside a repo on some machines; only assert when it does not.
        if !git_sync(dir.path(), &["rev-parse", "--is-inside-work-tree"]) {
            assert_eq!(snapshot_tree(dir.path(), &session).await, None);
        }
    }

    #[test]
    fn cap_cuts_on_a_line() {
        let line = "x".repeat(1000) + "\n";
        let big = line.repeat(300);
        let cut = cap(big.clone());
        assert!(cut.len() < big.len());
        assert!(cut.contains("[diff truncated:"));
        assert_eq!(cap("small".to_owned()), "small");
    }
}
