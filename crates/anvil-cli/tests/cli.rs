//! Black-box tests of the `anvil` binary. None of them reach a network.

use std::path::Path;
use std::process::Command;

/// A command that runs the freshly built `anvil` binary in `dir` with
/// `ANVIL_HOME` pointed at an empty folder under `dir`, so the user's real
/// `~/.anvil` stays out of the test (Windows finds the home even without
/// `HOME` and `USERPROFILE`). Tests that need a home set `ANVIL_HOME` again.
fn anvil(dir: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_anvil"));
    command
        .current_dir(dir)
        .env_remove("HOME")
        .env_remove("USERPROFILE")
        .env("ANVIL_HOME", dir.join("empty-home"))
        .env("ANVIL_LOG", "off");
    command
}

fn init(dir: &Path) {
    let out = anvil(dir)
        .args(["config", "init"])
        .output()
        .expect("anvil runs");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn version_prints_name_and_version() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = anvil(dir.path())
        .arg("--version")
        .output()
        .expect("anvil runs");
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        format!("anvil {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn no_arguments_prints_help_and_fails() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = anvil(dir.path()).output().expect("anvil runs");
    assert!(!out.status.success());
}

#[test]
fn config_init_writes_a_valid_template_and_refuses_to_overwrite() {
    let dir = tempfile::tempdir().expect("tempdir");
    init(dir.path());
    let path = dir.path().join(".anvil").join("config.toml");
    let text = std::fs::read_to_string(&path).expect("config written");
    let config: anvil_types::AnvilConfig = toml::from_str(&text).expect("template parses");
    config.validate().expect("template validates");

    let second = anvil(dir.path())
        .args(["config", "init"])
        .output()
        .expect("anvil runs");
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
    let out = anvil(dir.path())
        .args(["config", "show"])
        .output()
        .expect("anvil runs");
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
        let out = anvil(dir.path()).args(&args).output().expect("anvil runs");
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("anvil config init"), "{args:?}: {stderr}");
    }
}

#[test]
fn sessions_lists_nothing_without_config_or_sessions() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = anvil(dir.path())
        .arg("sessions")
        .output()
        .expect("anvil runs");
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
    let out = anvil(dir.path())
        .args(["resume", "nope"])
        .output()
        .expect("anvil runs");
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("no session `nope`"));
}

#[test]
fn doctor_reports_a_missing_api_key_without_a_network() {
    let dir = tempfile::tempdir().expect("tempdir");
    init(dir.path());
    let out = anvil(dir.path())
        .arg("doctor")
        .env_remove("ANVIL_RUNPOD_KEY")
        .output()
        .expect("anvil runs");
    assert_eq!(out.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("key:ANVIL_RUNPOD_KEY"), "{stdout}");
    assert!(stdout.contains("FAIL"), "{stdout}");
    assert!(stdout.contains("SKIP  provider:runpod"), "{stdout}");
}

/// A config with one provider on a closed local port, so no ping leaves the machine.
const LOCAL_CONFIG: &str = r#"
[providers.local]
protocol = "openai"
base_url = "http://127.0.0.1:9/v1"
api_key_env = "ANVIL_TEST_ORDER_KEY"
model = "m"
context_window = 1000
supports_tools = true
supports_reasoning = false

[roles]
coder = "local"
summarizer = "local"
"#;

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, text).expect("write");
}

/// The `key:ANVIL_TEST_ORDER_KEY` row of `anvil doctor`.
fn key_row(dir: &Path, home: &Path, real_env: Option<&str>) -> String {
    let mut command = anvil(dir);
    command
        .env("ANVIL_HOME", home)
        .env_remove("ANVIL_TEST_ORDER_KEY")
        .arg("doctor");
    if let Some(value) = real_env {
        command.env("ANVIL_TEST_ORDER_KEY", value);
    }
    let out = command.output().expect("anvil runs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    stdout
        .lines()
        .find(|line| line.contains("key:ANVIL_TEST_ORDER_KEY"))
        .unwrap_or_else(|| panic!("no key row in:\n{stdout}"))
        .to_owned()
}

#[test]
fn env_order_real_env_beats_home_env_beats_project_env() {
    let dir = tempfile::tempdir().expect("tempdir");
    let home = dir.path().join("home");
    let project = dir.path().join("project");
    write(&home.join(".anvil").join("config.toml"), LOCAL_CONFIG);
    std::fs::create_dir_all(&project).expect("mkdir");
    let home_env = home.join(".anvil").join(".env");
    let project_env = project.join(".anvil").join(".env");

    write(&home_env, "ANVIL_TEST_ORDER_KEY=real-looking-key\n");
    write(&project_env, "ANVIL_TEST_ORDER_KEY=\"<placeholder>\"\n");
    assert!(
        key_row(&project, &home, None).starts_with("PASS"),
        "home beats project"
    );

    write(&home_env, "ANVIL_TEST_ORDER_KEY=\"<placeholder>\"\n");
    write(&project_env, "ANVIL_TEST_ORDER_KEY=real-looking-key\n");
    let row = key_row(&project, &home, None);
    assert!(
        row.starts_with("FAIL"),
        "home placeholder wins over project: {row}"
    );
    assert!(row.contains("placeholder"), "{row}");

    assert!(
        key_row(&project, &home, Some("from-the-real-env")).starts_with("PASS"),
        "real environment beats both files"
    );
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn setup_is_idempotent_and_keeps_filled_keys() {
    let dir = tempfile::tempdir().expect("tempdir");
    let home = dir.path().join("home");
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).expect("mkdir");
    git(&project, &["init", "-q"]);
    git(
        &project,
        &["remote", "add", "origin", "https://github.com/o/r.git"],
    );
    let setup = || {
        anvil(&project)
            .env("ANVIL_HOME", &home)
            .arg("setup")
            .output()
            .expect("anvil runs")
    };
    let files = [
        home.join(".anvil").join(".env"),
        home.join(".anvil").join("config.toml"),
        project.join("factory.toml"),
        project.join("deploy").join("nuc").join("env.template"),
    ];
    let snapshot = || -> Vec<String> {
        files
            .iter()
            .map(|path| std::fs::read_to_string(path).expect("file written"))
            .collect()
    };

    let first = setup();
    assert_eq!(
        first.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let stdout = String::from_utf8_lossy(&first.stdout);
    assert!(stdout.contains("ANTHROPIC_API_KEY in"), "{stdout}");
    assert!(stdout.contains("claude-opus-5-5"), "{stdout}");
    let written = snapshot();
    assert!(written[2].contains("repo = \"o/r\""), "{}", written[2]);
    assert!(
        written[3].contains("FACTORY_GITHUB_TOKEN="),
        "{}",
        written[3]
    );
    let config: anvil_types::AnvilConfig = toml::from_str(&written[1]).expect("config parses");
    config.validate().expect("config validates");

    let filled = written[0].replace(
        "ANTHROPIC_API_KEY=\"<console.anthropic.com -> API keys>\"",
        "ANTHROPIC_API_KEY=my-real-anthropic-key",
    );
    assert_ne!(filled, written[0]);
    std::fs::write(&files[0], &filled).expect("fill in");
    let before = snapshot();

    let second = setup();
    assert_eq!(second.status.code(), Some(2));
    let stdout = String::from_utf8_lossy(&second.stdout);
    assert!(!stdout.contains("ANTHROPIC_API_KEY in"), "{stdout}");
    assert!(stdout.contains("OPENAI_API_KEY in"), "{stdout}");
    assert_eq!(snapshot(), before, "second setup changes nothing");
    assert!(before[0].contains("ANTHROPIC_API_KEY=my-real-anthropic-key"));
}

#[test]
fn scan_keys_catches_each_prefix_and_an_env_value() {
    let dir = tempfile::tempdir().expect("tempdir");
    let home = dir.path().join("home");
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).expect("mkdir");
    git(&project, &["init", "-q"]);
    write(
        &home.join(".anvil").join(".env"),
        "GITHUB_TOKEN=secret-value-from-env-file\n",
    );
    let fakes = [
        format!("sk-{}", "a".repeat(40)),
        format!("AIza{}", "b".repeat(35)),
        format!("ghp_{}", "c".repeat(36)),
        format!("github_pat_{}", "d".repeat(60)),
        format!("hf_{}", "e".repeat(30)),
    ];
    let mut text = String::new();
    for fake in &fakes {
        text.push_str(&format!("key = \"{fake}\"\n"));
    }
    text.push_str("token = secret-value-from-env-file\n");
    write(&project.join("leak.toml"), &text);
    write(&project.join("clean.txt"), "nothing to see\n");
    git(&project, &["add", "."]);

    let out = anvil(&project)
        .env("ANVIL_HOME", &home)
        .arg("scan-keys")
        .output()
        .expect("anvil runs");
    assert_eq!(out.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&out.stdout);
    for (line, kind) in [
        (1, "sk- key"),
        (2, "AIza key"),
        (3, "ghp_ token"),
        (4, "github_pat_ token"),
        (5, "hf_ token"),
        (6, "value of GITHUB_TOKEN"),
    ] {
        assert!(
            stdout.contains(&format!("leak.toml:{line}: {kind}")),
            "{stdout}"
        );
    }
    for fake in &fakes {
        assert!(!stdout.contains(fake.as_str()), "value printed: {stdout}");
    }
    assert!(!stdout.contains("secret-value-from-env-file"), "{stdout}");
    assert!(!stdout.contains("clean.txt"), "{stdout}");

    git(&project, &["rm", "-q", "--cached", "leak.toml"]);
    let clean = anvil(&project)
        .env("ANVIL_HOME", &home)
        .arg("scan-keys")
        .output()
        .expect("anvil runs");
    assert_eq!(
        clean.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&clean.stdout)
    );
}

#[test]
fn rules_list_add_check_and_remove_with_a_temp_home() {
    let dir = tempfile::tempdir().expect("tempdir");
    let home = dir.path().join("home");
    let rules = |args: &[&str]| {
        anvil(dir.path())
            .env("ANVIL_HOME", &home)
            .arg("rules")
            .args(args)
            .output()
            .expect("anvil runs")
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
    assert!(home.join(".anvil").join("rules.toml").is_file());
    assert!(
        home.join(".anvil")
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

    let out = anvil(dir.path())
        .args(["skill", "add", "my-skill", "--project"])
        .output()
        .expect("anvil runs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("installed skill `my-skill`"), "{stdout}");
    assert!(stdout.contains("files: scripts/go.sh"), "{stdout}");
    assert!(dir.path().join(".anvil/skills/my-skill/SKILL.md").is_file());

    let again = anvil(dir.path())
        .args(["skill", "add", "my-skill", "--project"])
        .output()
        .expect("anvil runs");
    assert_eq!(again.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&again.stderr).contains("--force"));

    let list = anvil(dir.path())
        .args(["skill", "list"])
        .output()
        .expect("anvil runs");
    let stdout = String::from_utf8_lossy(&list.stdout);
    assert!(stdout.contains("my-skill"), "{stdout}");
    assert!(stdout.contains(".anvil/skills"), "{stdout}");
    assert!(!stdout.contains("THE BODY TEXT"), "{stdout}");

    let show = anvil(dir.path())
        .args(["skill", "show", "my-skill"])
        .output()
        .expect("anvil runs");
    let stdout = String::from_utf8_lossy(&show.stdout);
    assert!(stdout.contains("THE BODY TEXT"), "{stdout}");
    assert!(stdout.contains("description: Does the thing."), "{stdout}");

    let removed = anvil(dir.path())
        .args(["skill", "remove", "my-skill", "--project"])
        .output()
        .expect("anvil runs");
    assert!(removed.status.success());
    assert!(!dir.path().join(".anvil/skills/my-skill").exists());

    // The home directory's own skills may still be listed (Windows finds the
    // home without the environment variables), so only check ours is gone.
    let after = anvil(dir.path())
        .args(["skill", "list"])
        .output()
        .expect("anvil runs");
    assert!(!String::from_utf8_lossy(&after.stdout).contains("my-skill"));
}

#[test]
fn ticket_list_and_stats_work_on_an_empty_project() {
    let dir = tempfile::tempdir().expect("tempdir");
    let home = tempfile::tempdir().expect("home");
    let list = anvil(dir.path())
        .env("ANVIL_HOME", home.path())
        .args(["ticket", "list"])
        .output()
        .expect("anvil runs");
    assert!(
        list.status.success(),
        "{}",
        String::from_utf8_lossy(&list.stderr)
    );
    assert!(String::from_utf8_lossy(&list.stdout).contains("no tickets"));

    let stats = anvil(dir.path())
        .env("ANVIL_HOME", home.path())
        .args(["stats", "--since", "2w"])
        .output()
        .expect("anvil runs");
    assert!(
        stats.status.success(),
        "{}",
        String::from_utf8_lossy(&stats.stderr)
    );
    let stdout = String::from_utf8_lossy(&stats.stdout);
    assert!(stdout.contains("sessions: 0"), "{stdout}");
    assert!(stdout.contains("completion rate: n/a"), "{stdout}");

    let bad = anvil(dir.path())
        .env("ANVIL_HOME", home.path())
        .args(["stats", "--since", "soon"])
        .output()
        .expect("anvil runs");
    assert!(!bad.status.success());
    assert!(String::from_utf8_lossy(&bad.stderr).contains("not a duration"));

    let missing = anvil(dir.path())
        .env("ANVIL_HOME", home.path())
        .args(["ticket", "show", "01NOPE"])
        .output()
        .expect("anvil runs");
    assert!(!missing.status.success());
}

#[test]
fn ticket_submit_help_states_the_outbound_rule() {
    let dir = tempfile::tempdir().expect("tempdir");
    let help = anvil(dir.path())
        .args(["ticket", "submit", "--help"])
        .output()
        .expect("anvil runs");
    assert!(help.status.success());
    let stdout = String::from_utf8_lossy(&help.stdout);
    assert!(
        stdout.contains("only thing that ever leaves this machine"),
        "{stdout}"
    );
    assert!(stdout.contains("--all"), "{stdout}");
}
