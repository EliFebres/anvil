//! Publishing a submitted ticket to the ticket queue.
//!
//! The queue is a git repository named by `[tickets] queue` in the anvil
//! configuration; nothing is sent while that key is unset. anvil keeps a
//! bare cache of it under `~/.anvil/queue.git`, fetched before each
//! submission, so the current project's remotes are never used.
//!
//! The commit is built with plumbing: the ticket file becomes a blob, the
//! blob is added to a temporary index read from the base commit, and the
//! resulting tree is committed on top of that base. No working tree is
//! checked out or changed.

use std::path::Path;
use std::process::Command;

use anvil_types::ticket::dirs::STATE_BRANCH;

use crate::error::TelemetryError;
use crate::paths::OPEN_DIR;

/// The remote name the cache gives the queue.
pub const QUEUE_REMOTE: &str = "queue";

/// Creates the bare cache of the queue when it is missing, points its
/// remote at `url`, and fetches it.
///
/// # Arguments
///
/// * `git_dir` - The cache, normally `~/.anvil/queue.git`.
/// * `url` - The queue repository, as `[tickets] queue` names it.
///
/// # Errors
///
/// [`TelemetryError::Git`] naming the step that failed, or
/// [`TelemetryError::Io`] when the cache folder cannot be created.
pub fn sync_queue(git_dir: &Path, url: &str) -> Result<(), TelemetryError> {
    if !git_dir.join("HEAD").exists() {
        std::fs::create_dir_all(git_dir).map_err(|e| TelemetryError::io(git_dir, &e))?;
        git(git_dir, None, &["init", "-q", "--bare"])?;
    }
    if git(git_dir, None, &["remote", "get-url", QUEUE_REMOTE]).is_ok() {
        git(git_dir, None, &["remote", "set-url", QUEUE_REMOTE, url])?;
    } else {
        git(git_dir, None, &["remote", "add", QUEUE_REMOTE, url])?;
    }
    git(
        git_dir,
        None,
        &["fetch", "--quiet", "--prune", QUEUE_REMOTE],
    )?;
    Ok(())
}

/// Builds the branch `tickets/<id>` holding one submitted ticket and pushes it.
///
/// The base is the queue's state branch, `refs/remotes/<remote>/<STATE_BRANCH>`,
/// when that ref exists (the branch the queue keeps ticket state on), else
/// `refs/remotes/<remote>/main`, else `HEAD`. The ticket lands at
/// `tickets/open/<id>.toml` in the branch.
///
/// # Arguments
///
/// * `git_dir` - The repository the branch is built in: the queue's cache.
/// * `remote` - The git remote to push to.
/// * `ticket_file` - The submitted ticket on disk.
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
    git_dir: &Path,
    remote: &str,
    ticket_file: &Path,
    id: &str,
    title: &str,
) -> Result<String, TelemetryError> {
    let file = format!("{OPEN_DIR}/{id}.toml");
    let branch = format!("tickets/{id}");
    let index = std::env::temp_dir().join(format!("anvil-ticket-{id}.index"));
    let index_text = index.to_string_lossy().into_owned();
    let source = ticket_file.to_string_lossy();
    let result = build_and_push(git_dir, remote, &source, &file, &branch, title, &index_text);
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
    source: &str,
    file: &str,
    branch: &str,
    title: &str,
    index: &str,
) -> Result<(), TelemetryError> {
    let blob = git(root, None, &["hash-object", "-w", source])?;
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
    git(root, None, &["push", "--quiet", remote, &reference])?;
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
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(args)
            .current_dir(dir)
            .output()
            .is_ok_and(|output| output.status.success())
    }

    /// A bare queue with `main` and a state branch one commit ahead.
    fn queue(temp: &Path) -> Option<std::path::PathBuf> {
        let seed = temp.join("seed");
        let bare = temp.join("queue-remote.git");
        std::fs::create_dir_all(&seed).expect("mkdir");
        std::fs::create_dir_all(&bare).expect("mkdir");
        if !setup(&seed, &["init", "-q", "-b", "main"]) {
            return None;
        }
        assert!(setup(&bare, &["init", "-q", "--bare", "-b", "main"]));
        std::fs::write(seed.join("README.md"), "hello\n").expect("write");
        assert!(setup(&seed, &["add", "README.md"]));
        assert!(setup(&seed, &["commit", "-q", "-m", "init"]));
        assert!(setup(
            &seed,
            &["remote", "add", "origin", &bare.to_string_lossy()]
        ));
        assert!(setup(&seed, &["push", "-q", "origin", "main"]));
        std::fs::write(seed.join("state.txt"), "state\n").expect("write");
        assert!(setup(&seed, &["add", "state.txt"]));
        assert!(setup(&seed, &["commit", "-q", "-m", "state"]));
        assert!(setup(
            &seed,
            &["push", "-q", "origin", &format!("main:{STATE_BRANCH}")]
        ));
        Some(bare)
    }

    #[test]
    fn publishes_the_ticket_on_its_own_branch_from_the_queue_state() {
        let temp = tempfile::tempdir().expect("tempdir");
        let Some(remote) = queue(temp.path()) else {
            eprintln!("git not available; skipping");
            return;
        };
        let cache = temp.path().join("home").join("queue.git");
        let url = remote.to_string_lossy().into_owned();
        sync_queue(&cache, &url).expect("first sync creates the cache");
        sync_queue(&cache, &url).expect("a second sync reuses it");
        assert!(setup(&cache, &["config", "user.email", "t@example.com"]));
        assert!(setup(&cache, &["config", "user.name", "t"]));

        let id = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
        let ticket = temp.path().join(format!("{id}.toml"));
        std::fs::write(&ticket, "title = \"t\"\n").expect("write");
        let branch =
            publish_ticket(&cache, QUEUE_REMOTE, &ticket, id, "Missing tool `x`").expect("publish");
        assert_eq!(branch, format!("tickets/{id}"));
        let listed = git(
            &remote,
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
            "the branch starts from the queue's state branch, not main: {listed}"
        );
        let subject = git(
            &remote,
            None,
            &["log", "-1", "--format=%s", &format!("refs/heads/{branch}")],
        )
        .expect("log");
        assert_eq!(subject, format!("Add ticket {id}: Missing tool `x`"));
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
            publish_ticket(temp.path(), "origin", &temp.path().join("NOPE.toml"), "NOPE", "t"),
            Err(TelemetryError::Git { command, .. }) if command.starts_with("hash-object")
        ));
    }
}
