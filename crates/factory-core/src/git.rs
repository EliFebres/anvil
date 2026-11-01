//! The git commands the factory needs, run in one repository or worktree.
//!
//! Every command carries the factory's identity and never signs. There is no
//! way to force-push, reset hard or delete `main` through this module; the
//! factory's safety rules are enforced where the commands are built.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::process::Command;

use crate::error::{FactoryError, Result};

/// The committer every factory commit carries.
const IDENTITY: [&str; 6] = [
    "-c",
    "user.name=factory",
    "-c",
    "user.email=factory@anvil.invalid",
    "-c",
    "commit.gpgsign=false",
];

/// Arguments the factory refuses to pass to `git push`.
const FORBIDDEN_PUSH_ARGS: [&str; 4] = ["--force", "-f", "--force-with-lease", "--delete"];

/// One repository or worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Git {
    /// The folder commands run in.
    dir: PathBuf,
}

impl Git {
    /// A handle on a repository or worktree folder.
    ///
    /// # Arguments
    ///
    /// * `dir` - The folder.
    ///
    /// # Returns
    ///
    /// The handle; nothing is checked until a command runs.
    pub fn new(dir: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
        }
    }

    /// The folder commands run in.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Runs one git command and returns its trimmed stdout.
    ///
    /// # Arguments
    ///
    /// * `args` - The arguments after `git`.
    ///
    /// # Returns
    ///
    /// Standard output with surrounding whitespace removed.
    ///
    /// # Errors
    ///
    /// [`FactoryError::Spawn`] when git cannot start, [`FactoryError::Git`]
    /// with stderr when it exits non-zero.
    pub async fn run(&self, args: &[&str]) -> Result<String> {
        let output = Command::new("git")
            .args(IDENTITY)
            .args(args)
            .current_dir(&self.dir)
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(|source| FactoryError::Spawn {
                program: "git".to_owned(),
                source,
            })?;
        if !output.status.success() {
            return Err(FactoryError::Git {
                command: args.join(" "),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    /// `git fetch --prune <remote>`.
    pub async fn fetch_prune(&self, remote: &str) -> Result<()> {
        self.run(&["fetch", "--prune", "--quiet", remote]).await?;
        Ok(())
    }

    /// The full sha of a revision.
    pub async fn rev_parse(&self, rev: &str) -> Result<String> {
        self.run(&[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{rev}^{{commit}}"),
        ])
        .await
    }

    /// The full sha of a revision, or `None` when it does not exist.
    pub async fn try_rev_parse(&self, rev: &str) -> Option<String> {
        self.rev_parse(rev).await.ok()
    }

    /// The short sha of `HEAD`.
    pub async fn short_head(&self) -> Result<String> {
        self.run(&["rev-parse", "--short", "HEAD"]).await
    }

    /// The remote branches whose names start with `prefix`, without the
    /// remote name.
    ///
    /// # Arguments
    ///
    /// * `remote` - The remote.
    /// * `prefix` - A branch name prefix such as `tickets/`.
    ///
    /// # Returns
    ///
    /// Branch names, sorted.
    pub async fn remote_branches(&self, remote: &str, prefix: &str) -> Result<Vec<String>> {
        let pattern = format!("refs/remotes/{remote}/{prefix}");
        let out = self
            .run(&["for-each-ref", "--format=%(refname)", &pattern])
            .await?;
        let strip = format!("refs/remotes/{remote}/");
        let mut names: Vec<String> = out
            .lines()
            .filter_map(|line| line.strip_prefix(&strip))
            .filter(|name| *name != "HEAD")
            .map(str::to_owned)
            .collect();
        names.sort();
        Ok(names)
    }

    /// The merge base of two revisions.
    pub async fn merge_base(&self, a: &str, b: &str) -> Result<String> {
        self.run(&["merge-base", a, b]).await
    }

    /// The paths that differ between two revisions, with `/` separators.
    pub async fn diff_names(&self, a: &str, b: &str) -> Result<Vec<String>> {
        let out = self.run(&["diff", "--name-only", a, b]).await?;
        Ok(out.lines().map(str::to_owned).collect())
    }

    /// The content of a file at a revision.
    pub async fn show(&self, rev: &str, path: &str) -> Result<String> {
        let output = Command::new("git")
            .args(IDENTITY)
            .args(["show", &format!("{rev}:{path}")])
            .current_dir(&self.dir)
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(|source| FactoryError::Spawn {
                program: "git".to_owned(),
                source,
            })?;
        if !output.status.success() {
            return Err(FactoryError::Git {
                command: format!("show {rev}:{path}"),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// The paths of every file in a folder at a revision, with `/` separators.
    pub async fn ls_tree(&self, rev: &str, dir: &str) -> Result<Vec<String>> {
        let out = self
            .run(&["ls-tree", "-r", "--name-only", rev, "--", dir])
            .await?;
        Ok(out.lines().map(str::to_owned).collect())
    }

    /// Copies files from a revision into the working tree and index.
    pub async fn checkout_paths(&self, rev: &str, paths: &[String]) -> Result<()> {
        let mut args = vec!["checkout", rev, "--"];
        args.extend(paths.iter().map(String::as_str));
        self.run(&args).await?;
        Ok(())
    }

    /// Stages everything and commits; `false` when there was nothing to commit.
    pub async fn commit_all(&self, message: &str) -> Result<bool> {
        self.run(&["add", "-A"]).await?;
        let staged = self.run(&["diff", "--cached", "--name-only"]).await?;
        if staged.is_empty() {
            return Ok(false);
        }
        self.run(&["commit", "-q", "-m", message]).await?;
        Ok(true)
    }

    /// Stages the given paths (adds, changes and deletions) and commits them.
    pub async fn commit_paths(&self, paths: &[String], message: &str) -> Result<()> {
        let mut args = vec!["add", "-A", "--"];
        args.extend(paths.iter().map(String::as_str));
        self.run(&args).await?;
        let mut args = vec!["commit", "-q", "-m", message, "--"];
        args.extend(paths.iter().map(String::as_str));
        self.run(&args).await?;
        Ok(())
    }

    /// `git push <remote> <refspec>`, never forced.
    ///
    /// # Errors
    ///
    /// [`FactoryError::Config`] when the refspec would force or delete.
    pub async fn push(&self, remote: &str, refspec: &str) -> Result<()> {
        if refspec.starts_with('+') || FORBIDDEN_PUSH_ARGS.contains(&refspec) {
            return Err(FactoryError::Config(format!(
                "the factory never force-pushes or deletes through push: refused `{refspec}`"
            )));
        }
        self.run(&["push", "--quiet", remote, refspec]).await?;
        Ok(())
    }

    /// Deletes one remote branch. `main` and the state branch are refused.
    pub async fn delete_remote_branch(&self, remote: &str, branch: &str) -> Result<()> {
        if branch == "main" || branch == anvil_types::ticket::dirs::STATE_BRANCH {
            return Err(FactoryError::Config(format!(
                "the factory never deletes `{branch}`"
            )));
        }
        self.run(&["push", "--quiet", remote, "--delete", branch])
            .await?;
        Ok(())
    }

    /// `git pull --ff-only <remote> <branch>`.
    pub async fn pull_ff_only(&self, remote: &str, branch: &str) -> Result<()> {
        self.run(&["pull", "--quiet", "--ff-only", remote, branch])
            .await?;
        Ok(())
    }

    /// Adds a worktree on a new branch, replacing a stale one of the same name.
    ///
    /// # Arguments
    ///
    /// * `path` - Where the worktree goes.
    /// * `branch` - The branch to create.
    /// * `start` - The revision it starts from.
    pub async fn worktree_add(&self, path: &Path, branch: &str, start: &str) -> Result<()> {
        self.run(&["worktree", "prune"]).await?;
        if path.exists() {
            self.worktree_remove(path).await?;
        }
        if self.try_rev_parse(branch).await.is_some() {
            self.run(&["branch", "-D", "--quiet", branch]).await?;
        }
        let path_text = path.to_string_lossy().into_owned();
        self.run(&[
            "worktree", "add", "--quiet", "-b", branch, &path_text, start,
        ])
        .await?;
        Ok(())
    }

    /// Adds a worktree that checks out an existing branch, tracking its remote.
    pub async fn worktree_add_tracking(
        &self,
        path: &Path,
        branch: &str,
        remote: &str,
    ) -> Result<()> {
        self.run(&["worktree", "prune"]).await?;
        let path_text = path.to_string_lossy().into_owned();
        let upstream = format!("{remote}/{branch}");
        self.run(&[
            "worktree", "add", "--quiet", "-B", branch, &path_text, &upstream,
        ])
        .await?;
        let worktree = Git::new(path);
        worktree
            .run(&[
                "branch",
                "--quiet",
                &format!("--set-upstream-to={upstream}"),
                branch,
            ])
            .await?;
        Ok(())
    }

    /// Removes a worktree and its folder.
    pub async fn worktree_remove(&self, path: &Path) -> Result<()> {
        let path_text = path.to_string_lossy().into_owned();
        self.run(&["worktree", "remove", "--force", &path_text])
            .await?;
        Ok(())
    }

    /// Deletes a local branch, if it exists.
    pub async fn delete_local_branch(&self, branch: &str) -> Result<()> {
        if self.try_rev_parse(branch).await.is_some() {
            self.run(&["branch", "-D", "--quiet", branch]).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn push_refuses_force_and_delete() {
        let git = Git::new(Path::new("."));
        for bad in ["--force", "-f", "+refs/heads/main:main", "--delete"] {
            let error = git.push("origin", bad).await.expect_err("refused");
            assert!(matches!(error, FactoryError::Config(_)), "{bad}: {error}");
        }
        for protected in ["main", "factory/state"] {
            let error = git
                .delete_remote_branch("origin", protected)
                .await
                .expect_err("refused");
            assert!(matches!(error, FactoryError::Config(_)), "{protected}");
        }
    }

    #[tokio::test]
    async fn a_failing_command_reports_its_arguments() {
        let temp = tempfile::tempdir().expect("tempdir");
        let git = Git::new(temp.path());
        match git.rev_parse("HEAD").await {
            Err(FactoryError::Git { command, .. }) => assert!(command.starts_with("rev-parse")),
            Err(FactoryError::Spawn { .. }) => eprintln!("git not available; skipping"),
            other => panic!("unexpected: {other:?}"),
        }
    }
}
