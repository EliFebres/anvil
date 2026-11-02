//! A push with a GitHub token carries the token as a header on that one
//! command line, never in a config file or in the error text that reaches
//! the log.

mod support;

use std::path::Path;
use std::process::Command;

use anvil_types::github::git_auth_args;
use factory_core::Git;
use support::{World, git, have_git, write};

/// A `pre-push` hook that records the auth settings git sees. `-c` values
/// reach hooks through `GIT_CONFIG_PARAMETERS`, so what the hook prints is
/// what the push ran with.
fn install_recording_hook(world: &World, seen: &Path) {
    let hooks = world.dir.path().join("hooks");
    std::fs::create_dir_all(&hooks).expect("mkdir");
    let seen_text = seen.to_string_lossy().replace('\\', "/");
    write(
        &hooks.join("pre-push"),
        &format!(
            "#!/bin/sh\ngit config --get-all http.extraheader >> \"{seen_text}\"\n\
             printf 'helper=%s\\n' \"$(git config --get credential.helper)\" >> \"{seen_text}\"\n"
        ),
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            hooks.join("pre-push"),
            std::fs::Permissions::from_mode(0o755),
        )
        .expect("chmod");
    }
    // A global `core.hooksPath` on the developer's machine must not shadow it.
    git(
        &world.factory,
        &["config", "core.hooksPath", &hooks.to_string_lossy()],
    );
}

#[tokio::test]
async fn a_push_carries_the_token_header_without_persisting_or_logging_it() {
    if !have_git() {
        eprintln!("git not available; skipping");
        return;
    }
    let world = World::new();
    let seen = world.dir.path().join("seen.txt");
    install_recording_hook(&world, &seen);
    let header = git_auth_args("fake-token")[3].clone();
    let base64 = header.rsplit(' ').next().expect("base64 value");

    // The push works and the hook saw exactly the header and no helper.
    let repo = Git::new(&world.factory).with_github_token("fake-token");
    write(&world.factory.join("note.txt"), "one\n");
    git(&world.factory, &["add", "-A"]);
    git(&world.factory, &["commit", "-q", "-m", "note"]);
    repo.push("origin", "main").await.expect("push");
    let recorded = std::fs::read_to_string(&seen).expect("hook ran");
    assert_eq!(
        recorded,
        format!("AUTHORIZATION: basic {base64}\nhelper=\n"),
        "{recorded}"
    );

    // Nothing was written to any config file: not the clone's, not the
    // global one, not the system one.
    let persisted = Command::new("git")
        .args(["config", "--show-origin", "--get-all", "http.extraheader"])
        .current_dir(&world.factory)
        .output()
        .expect("git runs");
    assert_eq!(String::from_utf8_lossy(&persisted.stdout).trim(), "");
    let local = std::fs::read_to_string(world.factory.join(".git/config")).expect("config");
    assert!(!local.contains("extraheader"), "{local}");
    assert!(!local.contains("credential"), "{local}");

    // The error a failed push produces, which is what reaches the log, the
    // webhook and the state file, names the arguments and never the token.
    let error = repo
        .push("origin", "no-such-branch")
        .await
        .expect_err("nothing to push")
        .to_string();
    assert!(
        error.starts_with("git push --quiet origin no-such-branch"),
        "{error}"
    );
    for secret in ["fake-token", base64, "extraheader", "credential"] {
        assert!(!error.contains(secret), "{secret} in {error}");
    }
    assert!(!format!("{repo:?}").contains("fake-token"));

    // A handle derived for a worktree pushes with the same token.
    std::fs::remove_file(&seen).expect("reset");
    write(&world.factory.join("note.txt"), "two\n");
    git(&world.factory, &["commit", "-q", "-am", "note two"]);
    repo.at(&world.factory)
        .push("origin", "main")
        .await
        .expect("push from the derived handle");
    let recorded = std::fs::read_to_string(&seen).expect("hook ran again");
    assert!(recorded.contains(base64), "{recorded}");
}
