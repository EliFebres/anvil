//! The few git commands the harness needs.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::process::Command;

use crate::error::EvalError;

/// The repository root that contains `cwd`.
///
/// # Arguments
///
/// * `cwd` - Any folder inside the repository.
///
/// # Returns
///
/// The top-level folder.
///
/// # Errors
///
/// When `cwd` is not inside a git repository.
pub async fn toplevel(cwd: &Path) -> Result<PathBuf, EvalError> {
    let out = git(cwd, &["rev-parse", "--show-toplevel"]).await?;
    Ok(PathBuf::from(out.trim()))
}

/// The short sha of `HEAD`.
///
/// # Arguments
///
/// * `repo_root` - The repository.
///
/// # Returns
///
/// The abbreviated sha.
///
/// # Errors
///
/// When the folder is not a repository or has no commits.
pub async fn short_sha(repo_root: &Path) -> Result<String, EvalError> {
    let out = git(repo_root, &["rev-parse", "--short", "HEAD"]).await?;
    Ok(out.trim().to_owned())
}

/// Turns a fixture copy into a repository with one commit.
///
/// anvil's git-aware tools and many `check.sh` scripts expect a repository,
/// and a baseline commit lets a check `git diff` the model's work.
///
/// # Arguments
///
/// * `dir` - The fixture copy.
///
/// # Errors
///
/// When git fails.
pub async fn init_fixture_repo(dir: &Path) -> Result<(), EvalError> {
    git(dir, &["init", "-q"]).await?;
    git(dir, &["add", "-A"]).await?;
    git(
        dir,
        &[
            "-c",
            "user.name=eval",
            "-c",
            "user.email=eval@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "fixture",
        ],
    )
    .await?;
    Ok(())
}

/// Stages and commits one path, leaving everything else as it was.
///
/// # Arguments
///
/// * `repo_root` - The repository.
/// * `path` - The file to commit.
/// * `message` - The commit message.
///
/// # Errors
///
/// When git fails, for example because `user.name` is unset.
pub async fn commit_path(repo_root: &Path, path: &Path, message: &str) -> Result<(), EvalError> {
    let path = path.to_string_lossy().into_owned();
    git(repo_root, &["add", "--", &path]).await?;
    git(repo_root, &["commit", "-q", "-m", message, "--", &path]).await?;
    Ok(())
}

/// Runs git and returns its stdout.
async fn git(cwd: &Path, args: &[&str]) -> Result<String, EvalError> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|source| EvalError::Spawn {
            program: PathBuf::from("git"),
            source,
        })?;
    if !output.status.success() {
        return Err(EvalError::Git {
            args: args.join(" "),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fixture_repo_gets_one_commit_and_commit_path_adds_another() {
        let dir = tempfile::tempdir().expect("tmp");
        std::fs::write(dir.path().join("a.txt"), "a\n").expect("write");
        init_fixture_repo(dir.path()).await.expect("init");
        let sha = short_sha(dir.path()).await.expect("sha");
        assert!(sha.len() >= 7);
        let top = toplevel(dir.path()).await.expect("toplevel");
        assert_eq!(
            top.file_name(),
            dir.path().canonicalize().expect("canon").file_name()
        );

        std::fs::write(dir.path().join("b.txt"), "b\n").expect("write");
        std::fs::write(dir.path().join("c.txt"), "untouched\n").expect("write");
        git(
            dir.path(),
            &[
                "-c",
                "user.name=eval",
                "-c",
                "user.email=e@example.invalid",
                "config",
                "user.name",
                "eval",
            ],
        )
        .await
        .expect("config");
        git(dir.path(), &["config", "user.email", "e@example.invalid"])
            .await
            .expect("config");
        commit_path(dir.path(), Path::new("b.txt"), "add b")
            .await
            .expect("commit");
        let log = git(dir.path(), &["log", "--oneline"]).await.expect("log");
        assert_eq!(log.lines().count(), 2);
        let status = git(dir.path(), &["status", "--porcelain"])
            .await
            .expect("status");
        assert!(
            status.contains("c.txt"),
            "c.txt stays uncommitted: {status}"
        );
        assert!(matches!(
            short_sha(&dir.path().join("missing")).await,
            Err(EvalError::Spawn { .. } | EvalError::Git { .. })
        ));
    }
}
