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
fn rules_list_add_check_and_remove_with_a_temp_home() {
    let dir = tempfile::tempdir().expect("tempdir");
    let home = dir.path().join("home");
    let rules = |args: &[&str]| {
        forge(dir.path())
            .env("FORGE_HOME", &home)
            .arg("rules")
            .args(args)
            .output()
            .expect("forge runs")
    };

    let list = rules(&["list"]);
    assert!(
        list.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&list.stderr)
    );
    let stdout = String::from_utf8_lossy(&list.stdout);
    assert!(stdout.contains("google-docstrings"), "{stdout}");
    assert!(stdout.contains("readable-first"), "{stdout}");
    assert!(String::from_utf8_lossy(&list.stderr).contains("seeded"));
    assert!(home.join(".forge").join("rules.toml").is_file());
    assert!(
        home.join(".forge")
            .join("checks")
            .join("docstrings.py")
            .is_file()
    );

    let add = rules(&[
        "add",
        "no-todo",
        "--languages",
        "rust,python",
        "--regex",
        "TODO",
        "Leave",
        "no",
        "TODO",
        "comments.",
    ]);
    assert!(
        add.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&add.stderr)
    );
    let listed = rules(&["list"]);
    let stdout = String::from_utf8_lossy(&listed.stdout);
    assert!(stdout.contains("no-todo"), "{stdout}");
    assert!(stdout.contains("Leave no TODO comments."), "{stdout}");
    assert!(stdout.contains("check: regex `TODO`"), "{stdout}");

    std::fs::write(dir.path().join("x.rs"), "// TODO later\npub fn f() {}\n").expect("write");
    let check = rules(&["check", "x.rs"]);
    assert_eq!(check.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&check.stdout);
    assert!(stdout.contains("[no-todo] check failed"), "{stdout}");
    assert!(stdout.contains("x.rs:1: // TODO later"), "{stdout}");

    let removed = rules(&["remove", "no-todo"]);
    assert!(removed.status.success());
    let listed = rules(&["list"]);
    let stdout = String::from_utf8_lossy(&listed.stdout);
    assert!(!stdout.contains("no-todo"), "{stdout}");
    let again = rules(&["remove", "no-todo"]);
    assert_eq!(again.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&again.stderr).contains("no rule with id"));
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

#[test]
fn ticket_list_and_stats_work_on_an_empty_project() {
    let dir = tempfile::tempdir().expect("tempdir");
    let home = tempfile::tempdir().expect("home");
    let list = forge(dir.path())
        .env("FORGE_HOME", home.path())
        .args(["ticket", "list"])
        .output()
        .expect("forge runs");
    assert!(
        list.status.success(),
        "{}",
        String::from_utf8_lossy(&list.stderr)
    );
    assert!(String::from_utf8_lossy(&list.stdout).contains("no tickets"));

    let stats = forge(dir.path())
        .env("FORGE_HOME", home.path())
        .args(["stats", "--since", "2w"])
        .output()
        .expect("forge runs");
    assert!(
        stats.status.success(),
        "{}",
        String::from_utf8_lossy(&stats.stderr)
    );
    let stdout = String::from_utf8_lossy(&stats.stdout);
    assert!(stdout.contains("sessions: 0"), "{stdout}");
    assert!(stdout.contains("completion rate: n/a"), "{stdout}");

    let bad = forge(dir.path())
        .env("FORGE_HOME", home.path())
        .args(["stats", "--since", "soon"])
        .output()
        .expect("forge runs");
    assert!(!bad.status.success());
    assert!(String::from_utf8_lossy(&bad.stderr).contains("not a duration"));

    let missing = forge(dir.path())
        .env("FORGE_HOME", home.path())
        .args(["ticket", "show", "01NOPE"])
        .output()
        .expect("forge runs");
    assert!(!missing.status.success());
}

#[test]
fn ticket_submit_help_states_the_outbound_rule() {
    let dir = tempfile::tempdir().expect("tempdir");
    let help = forge(dir.path())
        .args(["ticket", "submit", "--help"])
        .output()
        .expect("forge runs");
    assert!(help.status.success());
    let stdout = String::from_utf8_lossy(&help.stdout);
    assert!(
        stdout.contains("only thing that ever leaves this machine"),
        "{stdout}"
    );
    assert!(stdout.contains("--all"), "{stdout}");
}
