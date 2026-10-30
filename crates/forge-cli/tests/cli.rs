//! Black-box tests of the `forge` binary. None of them reach a network.

use std::path::Path;
use std::process::Command;

/// A command that runs the freshly built `forge` binary in `dir` with no
/// home directory, so the user's real global config stays out of the test.
fn forge(dir: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_forge"));
    command
        .current_dir(dir)
        .env_remove("HOME")
        .env_remove("USERPROFILE")
        .env("FORGE_LOG", "off");
    command
}

fn init(dir: &Path) {
    let out = forge(dir)
        .args(["config", "init"])
        .output()
        .expect("forge runs");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn version_prints_name_and_version() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = forge(dir.path())
        .arg("--version")
        .output()
        .expect("forge runs");
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        format!("forge {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn no_arguments_prints_help_and_fails() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = forge(dir.path()).output().expect("forge runs");
    assert!(!out.status.success());
}

#[test]
fn config_init_writes_a_valid_template_and_refuses_to_overwrite() {
    let dir = tempfile::tempdir().expect("tempdir");
    init(dir.path());
    let path = dir.path().join(".forge").join("config.toml");
    let text = std::fs::read_to_string(&path).expect("config written");
    let config: forge_types::ForgeConfig = toml::from_str(&text).expect("template parses");
    config.validate().expect("template validates");

    let second = forge(dir.path())
        .args(["config", "init"])
        .output()
        .expect("forge runs");
    assert!(!second.status.success(), "second init must fail");
    assert!(String::from_utf8_lossy(&second.stderr).contains("already exists"));
    assert_eq!(
        std::fs::read_to_string(&path).expect("still there"),
        text,
        "existing config must be untouched"
    );
}

#[test]
fn config_show_prints_the_merged_toml_with_sources() {
    let dir = tempfile::tempdir().expect("tempdir");
    init(dir.path());
    let out = forge(dir.path())
        .args(["config", "show"])
        .output()
        .expect("forge runs");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("# from "), "{stdout}");
    assert!(stdout.contains("[providers.runpod]"), "{stdout}");
    assert!(stdout.contains("coder = \"runpod\""), "{stdout}");
}

#[test]
fn commands_without_config_fail_with_exit_1_and_a_hint() {
    let dir = tempfile::tempdir().expect("tempdir");
    for args in [vec!["doctor"], vec!["chat"], vec!["add", "a", "flag"]] {
        let out = forge(dir.path()).args(&args).output().expect("forge runs");
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("forge config init"), "{args:?}: {stderr}");
    }
}

#[test]
fn sessions_lists_nothing_without_config_or_sessions() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = forge(dir.path())
        .arg("sessions")
        .output()
        .expect("forge runs");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("no sessions"));
}

#[test]
fn resume_of_an_unknown_session_fails() {
    let dir = tempfile::tempdir().expect("tempdir");
    init(dir.path());
    let out = forge(dir.path())
        .args(["resume", "nope"])
        .output()
        .expect("forge runs");
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("no session `nope`"));
}

#[test]
fn doctor_reports_a_missing_api_key_without_a_network() {
    let dir = tempfile::tempdir().expect("tempdir");
    init(dir.path());
    let out = forge(dir.path())
        .arg("doctor")
        .env_remove("FORGE_RUNPOD_KEY")
        .output()
        .expect("forge runs");
    assert_eq!(out.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("FORGE_RUNPOD_KEY"), "{stdout}");
    assert!(stdout.contains("FAILED"), "{stdout}");
}

#[test]
fn skill_add_list_show_and_remove_in_project_scope() {
    let dir = tempfile::tempdir().expect("tempdir");
    let source = dir.path().join("my-skill");
    std::fs::create_dir_all(source.join("scripts")).expect("mkdir");
    std::fs::write(
        source.join("SKILL.md"),
        "---\r\nname: my-skill\r\ndescription: Does the thing. Use when asked.\r\nlicense: MIT\r\n---\r\n\r\nTHE BODY TEXT\r\n",
    )
    .expect("write");
    std::fs::write(source.join("scripts").join("go.sh"), "echo go\n").expect("write");

    let out = forge(dir.path())
        .args(["skill", "add", "my-skill", "--project"])
        .output()
        .expect("forge runs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("installed skill `my-skill`"), "{stdout}");
    assert!(stdout.contains("files: scripts/go.sh"), "{stdout}");
    assert!(dir.path().join(".forge/skills/my-skill/SKILL.md").is_file());

    let again = forge(dir.path())
        .args(["skill", "add", "my-skill", "--project"])
        .output()
        .expect("forge runs");
    assert_eq!(again.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&again.stderr).contains("--force"));

    let list = forge(dir.path())
        .args(["skill", "list"])
        .output()
        .expect("forge runs");
    let stdout = String::from_utf8_lossy(&list.stdout);
    assert!(stdout.contains("my-skill"), "{stdout}");
    assert!(stdout.contains(".forge/skills"), "{stdout}");
    assert!(!stdout.contains("THE BODY TEXT"), "{stdout}");

    let show = forge(dir.path())
        .args(["skill", "show", "my-skill"])
        .output()
        .expect("forge runs");
    let stdout = String::from_utf8_lossy(&show.stdout);
    assert!(stdout.contains("THE BODY TEXT"), "{stdout}");
    assert!(stdout.contains("description: Does the thing."), "{stdout}");

    let removed = forge(dir.path())
        .args(["skill", "remove", "my-skill", "--project"])
        .output()
        .expect("forge runs");
    assert!(removed.status.success());
    assert!(!dir.path().join(".forge/skills/my-skill").exists());

    // The home directory's own skills may still be listed (Windows finds the
    // home without the environment variables), so only check ours is gone.
    let after = forge(dir.path())
        .args(["skill", "list"])
        .output()
        .expect("forge runs");
    assert!(!String::from_utf8_lossy(&after.stdout).contains("my-skill"));
}
