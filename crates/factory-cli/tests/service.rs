//! Black-box tests of `factory status`, `factory pause` and `factory resume`
//! against a bare repository standing in for GitHub.

use std::path::{Path, PathBuf};
use std::process::Command;

use anvil_types::ticket::dirs::{OPEN_DIR, STATE_BRANCH};

/// Runs git for test setup and panics with its stderr when it fails.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, text).expect("write");
}

/// A bare remote with `main` and `factory/state`, and the factory's clone of it.
struct World {
    _dir: tempfile::TempDir,
    home: PathBuf,
    repo: PathBuf,
}

impl World {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let remote = dir.path().join("github.git");
        std::fs::create_dir_all(&remote).expect("mkdir");
        git(&remote, &["init", "-q", "--bare", "-b", "main"]);
        let seed = dir.path().join("seed");
        std::fs::create_dir_all(&seed).expect("mkdir");
        git(&seed, &["init", "-q", "-b", "main"]);
        write(&seed.join("README.md"), "# repo\n");
        write(&seed.join(OPEN_DIR).join(".gitkeep"), "");
        write(
            &seed.join(OPEN_DIR).join("01ARZ3NDEKTSV4RRFFQ69G5FAV.toml"),
            r#"id = "01ARZ3NDEKTSV4RRFFQ69G5FAV"
created_at = "2026-10-30T10:00:00Z"
kind = "bug"
title = "Something to fix"
symptom = "It breaks."
spec = "Do it right."
acceptance = ["works"]
priority = "p1"
status = "open"
origin = "human"
"#,
        );
        git(&seed, &["add", "-A"]);
        git(&seed, &["commit", "-q", "-m", "init"]);
        git(
            &seed,
            &["remote", "add", "origin", &remote.to_string_lossy()],
        );
        git(&seed, &["push", "-q", "origin", "main"]);
        git(
            &seed,
            &["push", "-q", "origin", &format!("main:{STATE_BRANCH}")],
        );
        git(
            dir.path(),
            &["clone", "-q", &remote.to_string_lossy(), "factory-repo"],
        );
        let repo = dir.path().join("factory-repo");
        write(
            &repo.join("factory.toml"),
            &anvil_types::FactoryConfig::template("owner/repo"),
        );
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).expect("mkdir");
        Self {
            _dir: dir,
            home,
            repo,
        }
    }

    fn factory(&self, args: &[&str]) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_factory"))
            .args(args)
            .current_dir(&self.repo)
            .env("FACTORY_HOME", &self.home)
            .output()
            .expect("factory runs")
    }
}

#[test]
fn status_shows_the_queue_and_pause_resume_toggle_the_flag() {
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("git not available; skipping");
        return;
    }
    let world = World::new();

    let out = world.factory(&["status"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.starts_with("running\n"), "{text}");
    assert!(text.contains("queue (1 open):"), "{text}");
    assert!(text.contains("01ARZ3NDEKTSV4RRFFQ69G5FAV"), "{text}");
    assert!(text.contains("Something to fix"), "{text}");
    assert!(text.contains("in progress:\n  (none)"), "{text}");
    assert!(text.contains("last 10 outcomes:\n  (none yet)"), "{text}");
    assert!(
        world.home.join(".factory/state").join(".git").exists(),
        "the state worktree was created"
    );

    let out = world.factory(&["pause", "testing"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(world.home.join(".factory/pause"))
            .expect("flag")
            .trim(),
        "testing"
    );
    let out = world.factory(&["status"]);
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("PAUSED: testing"));

    let out = world.factory(&["resume"]);
    assert!(out.status.success());
    assert!(!world.home.join(".factory/pause").exists());
    let out = world.factory(&["pause"]);
    assert!(out.status.success());
    assert!(
        std::fs::read_to_string(world.home.join(".factory/pause"))
            .expect("flag")
            .contains("paused by hand")
    );
}

#[test]
fn run_once_without_a_token_fails_before_touching_the_queue() {
    if Command::new("git").arg("--version").output().is_err() {
        return;
    }
    let world = World::new();
    write(
        &world.home.join(".anvil/config.toml"),
        anvil_types::config::SETUP_PROVIDERS,
    );
    let out = Command::new(env!("CARGO_BIN_EXE_factory"))
        .args(["run", "--once"])
        .current_dir(&world.repo)
        .env("FACTORY_HOME", &world.home)
        .env_remove("FACTORY_GITHUB_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .output()
        .expect("factory runs");
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("FACTORY_GITHUB_TOKEN"), "{stderr}");
}
