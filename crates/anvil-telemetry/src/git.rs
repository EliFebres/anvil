//! Publishing a submitted ticket on its own branch without touching the
//! working tree.
//!
//! The commit is built with plumbing: the ticket file becomes a blob, the
//! blob is added to a temporary index read from the base commit, and the
//! resulting tree is committed on top of that base. The current branch, the
//! real index and the working tree are never checked out or changed.

use std::path::Path;
use std::process::Command;

use anvil_types::ticket::dirs::STATE_BRANCH;

use crate::error::TelemetryError;
use crate::paths::OPEN_DIR;

/// Builds the branch `tickets/<id>` holding one submitted ticket and pushes it.
///
/// The base is `refs/remotes/<remote>/factory/state` when that ref exists
/// (the branch the factory reads tickets from), else `refs/remotes/<remote>/main`,
/// else `HEAD`. The push is the only network operation; nothing is fetched.
///
/// # Arguments
///
/// * `root` - The repository root; the ticket is at `tickets/open/<id>.toml`.
/// * `remote` - The git remote to push to.
/// * `id` - The ticket id.
/// * `title` - The ticket title, for the commit message.
///
/// # Returns
///
/// The branch name.
///
/// # Errors
///
/// [`TelemetryError::Git`] naming the step that failed, or
/// [`TelemetryError::Io`] when the temporary index cannot be removed.
pub fn publish_ticket(
    root: &Path,
    remote: &str,
    id: &str,
    title: &str,
) -> Result<String, TelemetryError> {
    let file = format!("{OPEN_DIR}/{id}.toml");
    let branch = format!("tickets/{id}");
    let index = std::env::temp_dir().join(format!("anvil-ticket-{id}.index"));
    let index_text = index.to_string_lossy().into_owned();
    let result = build_and_push(root, remote, &file, &branch, title, &index_text);
    if index.exists() {
        std::fs::remove_file(&index).map_err(|e| TelemetryError::io(&index, &e))?;
    }
    result?;
    Ok(branch)
}

/// The plumbing steps, separated so the temporary index is removed either way.
fn build_and_push(
    root: &Path,
    remote: &str,
    file: &str,
    branch: &str,
    title: &str,
    index: &str,
) -> Result<(), TelemetryError> {
    let blob = git(root, None, &["hash-object", "-w", file])?;
    let base = base_commit(root, remote)?;
    git(root, Some(index), &["read-tree", &base])?;
    let cacheinfo = format!("100644,{blob},{file}");
    git(
        root,
        Some(index),
        &["update-index", "--add", "--cacheinfo", &cacheinfo],
    )?;
    let tree = git(root, Some(index), &["write-tree"])?;
    let message = format!(
        "Add ticket {id}: {title}",
        id = branch.trim_start_matches("tickets/")
    );
    let commit = git(
        root,
        None,
        &["commit-tree", &tree, "-p", &base, "-m", &message],
    )?;
    let reference = format!("refs/heads/{branch}");
    git(root, None, &["update-ref", &reference, &commit])?;
    git(root, None, &["push", remote, &reference])?;
    Ok(())
}

/// The commit the ticket branch starts from: the remote state branch, else
/// the remote main, else `HEAD`.
fn base_commit(root: &Path, remote: &str) -> Result<String, TelemetryError> {
    for branch in [STATE_BRANCH, "main"] {
        let reference = format!("refs/remotes/{remote}/{branch}");
        if let Ok(commit) = git(
            root,
            None,
            &["rev-parse", "--verify", "--quiet", &reference],
        ) {
            return Ok(commit);
        }
    }
    git(root, None, &["rev-parse", "--verify", "HEAD"])
}

/// Runs one git command and returns its trimmed stdout.
fn git(root: &Path, index: Option<&str>, args: &[&str]) -> Result<String, TelemetryError> {
    let mut command = Command::new("git");
    command
        .args(["-c", "core.quotepath=false"])
        .args(args)
        .current_dir(root);
    if let Some(index) = index {
        command.env("GIT_INDEX_FILE", index);
    }
    let output = command.output().map_err(|error| TelemetryError::Git {
        command: args.join(" "),
        detail: error.to_string(),
    })?;
    if !output.status.success() {
        return Err(TelemetryError::Git {
            command: args.join(" "),
            detail: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs git in a folder for test setup; `false` when git is unavailable.
    fn setup(dir: &Path, args: &[&str]) -> bool {
        Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .is_ok_and(|output| output.status.success())
    }

    #[test]
    fn publishes_the_ticket_on_its_own_branch_without_checkout() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        let bare = temp.path().join("remote.git");
        std::fs::create_dir_all(&repo).expect("mkdir");
        std::fs::create_dir_all(&bare).expect("mkdir");
        if !setup(&repo, &["init", "-q", "-b", "main"]) {
            eprintln!("git not available; skipping");
            return;
        }
        assert!(setup(&bare, &["init", "-q", "--bare", "-b", "main"]));
        assert!(setup(&repo, &["config", "user.email", "t@example.com"]));
        assert!(setup(&repo, &["config", "user.name", "t"]));
        std::fs::write(repo.join("README.md"), "hello\n").expect("write");
        assert!(setup(&repo, &["add", "README.md"]));
        assert!(setup(&repo, &["commit", "-q", "-m", "init"]));
        assert!(setup(
            &repo,
            &["remote", "add", "origin", &bare.to_string_lossy()]
        ));
        assert!(setup(&repo, &["push", "-q", "origin", "main"]));
        std::fs::write(repo.join("state.txt"), "state\n").expect("write");
        assert!(setup(&repo, &["add", "state.txt"]));
        assert!(setup(&repo, &["commit", "-q", "-m", "state"]));
        assert!(setup(
            &repo,
            &["push", "-q", "origin", "main:factory/state"]
        ));
        assert!(setup(&repo, &["reset", "-q", "--hard", "HEAD~1"]));

        let id = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
        let open = repo.join(OPEN_DIR);
        std::fs::create_dir_all(&open).expect("mkdir");
        std::fs::write(open.join(format!("{id}.toml")), "title = \"t\"\n").expect("write");
        std::fs::write(repo.join("dirty.txt"), "uncommitted\n").expect("write");

        let branch = publish_ticket(&repo, "origin", id, "Missing tool `x`").expect("publish");
        assert_eq!(branch, format!("tickets/{id}"));
        let listed = git(
            &bare,
            None,
            &[
                "ls-tree",
                "--name-only",
                "-r",
                &format!("refs/heads/{branch}"),
            ],
        )
        .expect("ls-tree");
        assert!(listed.contains(&format!("{OPEN_DIR}/{id}.toml")));
        assert!(listed.contains("README.md"));
        assert!(
            listed.contains("state.txt"),
            "the branch starts from origin/factory/state, not main: {listed}"
        );
        assert!(!listed.contains("dirty.txt"));
        let subject = git(
            &bare,
            None,
            &["log", "-1", "--format=%s", &format!("refs/heads/{branch}")],
        )
        .expect("log");
        assert_eq!(subject, format!("Add ticket {id}: Missing tool `x`"));
        assert_eq!(
            git(&repo, None, &["branch", "--show-current"]).expect("branch"),
            "main"
        );
        assert!(repo.join("dirty.txt").exists());
        let status = git(&repo, None, &["status", "--porcelain"]).expect("status");
        assert!(status.contains("dirty.txt"), "{status}");
        assert!(
            !std::env::temp_dir()
                .join(format!("anvil-ticket-{id}.index"))
                .exists()
        );
    }

    #[test]
    fn missing_file_is_a_git_error() {
        let temp = tempfile::tempdir().expect("tempdir");
        if !setup(temp.path(), &["init", "-q"]) {
            eprintln!("git not available; skipping");
            return;
        }
        assert!(matches!(
            publish_ticket(temp.path(), "origin", "NOPE", "t"),
            Err(TelemetryError::Git { command, .. }) if command.starts_with("hash-object")
        ));
    }
}
