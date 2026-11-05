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

/// A bare remote with `main` and `factory/state`, the factory's clone of it,
/// and a fake `systemctl` first on `PATH` so the liveness probe never reaches
/// the host's systemd.
struct World {
    _dir: tempfile::TempDir,
    home: PathBuf,
    repo: PathBuf,
    bin: PathBuf,
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
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).expect("mkdir");
        let world = Self {
            _dir: dir,
            home,
            repo,
            bin,
        };
        world.install_fake_systemctl();
        world.set_service_active(true);
        world
    }

    /// Writes a `systemctl` that logs its arguments and exits with the code
    /// in `bin/exit-code`, so a test flips the service state without
    /// rewriting an executable.
    #[cfg(unix)]
    fn install_fake_systemctl(&self) {
        use std::os::unix::fs::PermissionsExt;
        let script = self.bin.join("systemctl");
        let text = format!(
            "#!/bin/sh\necho \"$*\" >> '{}'\nexit \"$(cat '{}')\"\n",
            self.systemctl_log().display(),
            self.bin.join("exit-code").display()
        );
        write(&script, &text);
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    /// Off unix the probe never runs systemctl, so no fake is needed.
    #[cfg(not(unix))]
    fn install_fake_systemctl(&self) {}

    /// Where the fake `systemctl` records each call.
    fn systemctl_log(&self) -> PathBuf {
        self.bin.join("systemctl.log")
    }

    /// Makes the fake `systemctl is-active` report the unit active or not.
    fn set_service_active(&self, active: bool) {
        write(
            &self.bin.join("exit-code"),
            if active { "0\n" } else { "3\n" },
        );
    }

    fn factory(&self, args: &[&str]) -> std::process::Output {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let path = std::env::join_paths(
            std::iter::once(self.bin.clone()).chain(std::env::split_paths(&path)),
        )
        .expect("PATH joins");
        Command::new(env!("CARGO_BIN_EXE_factory"))
            .args(args)
            .current_dir(&self.repo)
            .env("FACTORY_HOME", &self.home)
            .env("PATH", path)
            .output()
            .expect("factory runs")
    }
}

/// The lines `factory status` starts with, before the queue. Off Linux the
/// service state is always unknown.
fn expected_header(active: bool, paused: Option<&str>) -> String {
    let pause_line = paused
        .map(|reason| format!("PAUSED: {reason}\n"))
        .unwrap_or_default();
    if !cfg!(target_os = "linux") {
        return format!("service state unknown\n{pause_line}\n");
    }
    match (active, paused) {
        (true, None) => "running\n\n".to_owned(),
        (true, Some(_)) => format!("{pause_line}\n"),
        (false, _) => {
            format!("STOPPED (start it with systemctl --user start factory)\n{pause_line}\n")
        }
    }
}

/// Runs `factory status` and returns its stdout, asserting it succeeded.
fn status_text(world: &World) -> String {
    let out = world.factory(&["status"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn status_header_follows_the_service_state_and_the_pause_flag() {
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("git not available; skipping");
        return;
    }
    let world = World::new();
    for (active, paused) in [
        (false, None),
        (true, None),
        (true, Some("testing")),
        (false, Some("testing")),
    ] {
        world.set_service_active(active);
        match paused {
            Some(reason) => assert!(world.factory(&["pause", reason]).status.success()),
            None => assert!(world.factory(&["resume"]).status.success()),
        }
        let text = status_text(&world);
        let header = expected_header(active, paused);
        assert!(
            text.starts_with(&format!("{header}queue (1 open):")),
            "active={active} paused={paused:?}: {text}"
        );
    }
    if cfg!(target_os = "linux") {
        let calls = std::fs::read_to_string(world.systemctl_log()).expect("fake was called");
        assert!(
            calls
                .lines()
                .all(|line| line == "--user is-active --quiet factory.service"),
            "{calls}"
        );
    }
}

#[test]
fn status_shows_the_queue_and_pause_resume_toggle_the_flag() {
    if Command::new("git").arg("--version").output().is_err() {
        eprintln!("git not available; skipping");
        return;
    }
    let world = World::new();

    let text = status_text(&world);
    assert!(text.starts_with(&expected_header(true, None)), "{text}");
    assert!(text.contains("queue (1 open):"), "{text}");
    assert!(text.contains("01ARZ3NDEKTSV4RRFFQ69G5FAV"), "{text}");
    assert!(text.contains("Something to fix"), "{text}");
    assert!(text.contains("in progress:\n  (none)"), "{text}");
    assert!(text.contains("last 10 outcomes:\n  (none yet)"), "{text}");
    // The template picks podman; whether it is installed or the image is
    // built, the line reports instead of failing the command.
    assert!(text.contains("\nsandbox: "), "{text}");
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
    let text = status_text(&world);
    assert!(
        text.starts_with(&expected_header(true, Some("testing"))),
        "{text}"
    );

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
