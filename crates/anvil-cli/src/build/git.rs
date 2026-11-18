//! The git operations of a build, through the `git` process.
//!
//! `.anvil/` is left out of every dirty check and every commit: a build
//! keeps its own files under `.anvil/builds/`, and a build's commits hold
//! the project's changes only. The owner may commit or ignore `.anvil/`
//! as they like.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, bail};

/// The pathspec that leaves anvil's own folder out of status and `add`.
const NOT_ANVIL: &str = ":(exclude).anvil";

/// Runs `git` in `cwd` and returns its trimmed stdout.
fn git(cwd: &Path, args: &[&str]) -> anyhow::Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("cannot run `git {}`", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "`git {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Whether `cwd` is inside a git work tree.
///
/// # Arguments
///
/// * `cwd` - The project root.
///
/// # Returns
///
/// True when git says so; false for no repository or no `git`.
pub fn is_repo(cwd: &Path) -> bool {
    git(cwd, &["rev-parse", "--is-inside-work-tree"]).is_ok_and(|out| out == "true")
}

/// The checked-out branch.
///
/// # Arguments
///
/// * `cwd` - The project root.
///
/// # Errors
///
/// When HEAD is detached or the repository has no commit yet.
pub fn current_branch(cwd: &Path) -> anyhow::Result<String> {
    let branch = git(cwd, &["symbolic-ref", "--short", "-q", "HEAD"])
        .context("a build needs a checked-out branch, not a detached HEAD")?;
    git(cwd, &["rev-parse", "--verify", "-q", "HEAD"])
        .context("a build needs a repository with at least one commit")?;
    Ok(branch)
}

/// The commit HEAD points at.
///
/// # Arguments
///
/// * `cwd` - The project root.
///
/// # Errors
///
/// When git cannot resolve HEAD.
pub fn head(cwd: &Path) -> anyhow::Result<String> {
    git(cwd, &["rev-parse", "HEAD"])
}

/// The changes that make the work tree dirty: tracked modifications and
/// untracked files that are not ignored, outside `.anvil/`.
///
/// # Arguments
///
/// * `cwd` - The project root.
///
/// # Returns
///
/// One `git status --porcelain` line per change; empty when the tree is clean.
///
/// # Errors
///
/// When `git status` fails.
pub fn dirty_paths(cwd: &Path) -> anyhow::Result<Vec<String>> {
    let out = git(
        cwd,
        &[
            "status",
            "--porcelain",
            "--untracked-files=all",
            "--",
            ".",
            NOT_ANVIL,
        ],
    )?;
    Ok(out.lines().map(str::to_owned).collect())
}

/// Commits every change outside `.anvil/`, even when there is none, so
/// each finished item has a commit of its own.
///
/// # Arguments
///
/// * `cwd` - The project root.
/// * `message` - The commit message.
///
/// # Returns
///
/// The new commit's hash.
///
/// # Errors
///
/// When staging or committing fails, for instance without a git identity.
pub fn commit_all(cwd: &Path, message: &str) -> anyhow::Result<String> {
    git(cwd, &["add", "-A", "--", ".", NOT_ANVIL])?;
    git(cwd, &["commit", "--allow-empty", "-q", "-m", message])?;
    head(cwd)
}

/// Moves the work tree's changes to a side branch and returns to `branch`
/// as it was: the changes are committed on the side branch, which leaves
/// `branch` untouched at its last good commit and the tree clean.
///
/// # Arguments
///
/// * `cwd` - The project root.
/// * `side` - The new branch's name.
/// * `branch` - The branch to return to.
/// * `message` - The side branch's commit message.
///
/// # Errors
///
/// When the side branch exists already or any git step fails.
pub fn save_attempt(cwd: &Path, side: &str, branch: &str, message: &str) -> anyhow::Result<()> {
    git(cwd, &["checkout", "-q", "-b", side])?;
    commit_all(cwd, message)?;
    git(cwd, &["checkout", "-q", branch])?;
    Ok(())
}
