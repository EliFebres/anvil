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

/// Writes the user configuration template into the empty home under `dir`.
fn init(dir: &Path) {
    let out = anvil(dir)
        .args(["config", "init", "--global"])
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
    let path = dir
        .path()
        .join("empty-home")
        .join(".anvil")
        .join("config.toml");
    let text = std::fs::read_to_string(&path).expect("config written");
    let config: anvil_types::AnvilConfig = toml::from_str(&text).expect("template parses");
    config.validate().expect("template validates");

    let project = anvil(dir.path())
        .args(["config", "init"])
        .output()
        .expect("anvil runs");
    assert!(project.status.success());
    let rules = anvil_types::project::load(dir.path()).expect("repository template loads");
    assert!(
        rules.ignored.is_empty(),
        "the repository template sets only tightening rules: {:?}",
        rules.ignored
    );

    let second = anvil(dir.path())
        .args(["config", "init", "--global"])
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
    assert!(stdout.contains("[providers.endpoint]"), "{stdout}");
    assert!(stdout.contains("coder = \"endpoint\""), "{stdout}");
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
        .env_remove("ANVIL_ENDPOINT_KEY")
        .output()
        .expect("anvil runs");
    assert_eq!(out.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("key:ANVIL_ENDPOINT_KEY"), "{stdout}");
    assert!(stdout.contains("FAIL"), "{stdout}");
    assert!(stdout.contains("SKIP  provider:endpoint"), "{stdout}");
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
fn the_real_env_beats_the_home_env_and_the_project_env_is_never_read() {
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

    std::fs::remove_file(&home_env).expect("remove the home file");
    write(
        &project.join(".env"),
        "ANVIL_TEST_ORDER_KEY=real-looking-key
",
    );
    let row = key_row(&project, &home, None);
    assert!(
        row.starts_with("FAIL") && row.contains("not set"),
        "a key only in the repository's .env or .anvil/.env is never loaded: {row}"
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
    assert!(stdout.contains("ANVIL_ENDPOINT_KEY in"), "{stdout}");
    assert!(stdout.contains("models.internal.example"), "{stdout}");
    assert!(!stdout.contains("anthropic.com"), "{stdout}");
    assert!(!stdout.contains("openai.com"), "{stdout}");
    let written = snapshot();
    let config: anvil_types::AnvilConfig = toml::from_str(&written[1]).expect("config parses");
    config.validate().expect("config validates");

    let filled = written[0].replace(
        "ANVIL_ENDPOINT_KEY=\"<your model server's API key>\"",
        "ANVIL_ENDPOINT_KEY=my-real-endpoint-key",
    );
    assert_ne!(filled, written[0]);
    std::fs::write(&files[0], &filled).expect("fill in");
    let before = snapshot();

    let second = setup();
    assert_eq!(second.status.code(), Some(0), "every placeholder is filled");
    let stdout = String::from_utf8_lossy(&second.stdout);
    assert!(!stdout.contains("ANVIL_ENDPOINT_KEY in"), "{stdout}");
    assert_eq!(snapshot(), before, "second setup changes nothing");
    assert!(before[0].contains("ANVIL_ENDPOINT_KEY=my-real-endpoint-key"));
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
fn stats_work_on_an_empty_project() {
    let dir = tempfile::tempdir().expect("tempdir");
    let home = tempfile::tempdir().expect("home");
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
}

#[test]
fn removed_commands_are_not_part_of_the_binary() {
    let dir = tempfile::tempdir().expect("tempdir");
    let help = anvil(dir.path())
        .arg("--help")
        .output()
        .expect("anvil runs");
    assert!(help.status.success());
    let stdout = String::from_utf8_lossy(&help.stdout);
    let commands: Vec<&str> = stdout
        .lines()
        .filter_map(|line| line.strip_prefix("  "))
        .filter_map(|line| line.split_whitespace().next())
        .collect();
    assert!(commands.contains(&"doctor"), "{stdout}");
    for removed in ["ticket", "update", "memory"] {
        assert!(
            !commands.contains(&removed),
            "`{removed}` in:
{stdout}"
        );
    }
}

/// The escape sequence that enters the alternate screen; the full-screen
/// UI starts with it, the line renderer never emits it.
const ALTERNATE_SCREEN: &str = "\x1b[?1049h";

#[test]
fn plain_is_selected_without_a_tty() {
    let dir = tempfile::tempdir().expect("tempdir");
    init(dir.path());
    for args in [
        vec!["chat"],
        vec!["--plain", "chat"],
        vec!["--yes", "run", "hello"],
    ] {
        let out = anvil(dir.path())
            .args(&args)
            .stdin(std::process::Stdio::null())
            .output()
            .expect("anvil runs");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !stdout.contains(ALTERNATE_SCREEN) && !stderr.contains(ALTERNATE_SCREEN),
            "{args:?} entered the alternate screen off a terminal"
        );
    }
}

#[test]
fn purge_removes_session_folders_older_than_the_given_days() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sessions = dir.path().join(".anvil").join("sessions");
    let session = sessions.join("01JAAAAAAAAAAAAAAAAAAAAAAA");
    std::fs::create_dir_all(&session).expect("mkdir");
    std::fs::write(session.join("session.jsonl"), "{}\n").expect("write");
    let other = sessions.join("not-a-session");
    std::fs::create_dir_all(&other).expect("mkdir");

    let kept = anvil(dir.path())
        .args(["purge", "--older-than", "30"])
        .output()
        .expect("anvil runs");
    assert!(
        kept.status.success(),
        "{}",
        String::from_utf8_lossy(&kept.stderr)
    );
    assert!(session.is_dir(), "a fresh session is kept");

    let out = anvil(dir.path())
        .args(["purge", "--older-than", "0"])
        .output()
        .expect("anvil runs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("removed 1 session folder(s)"));
    assert!(!session.exists(), "an older session is removed");
    assert!(
        other.is_dir(),
        "a folder that is not a session is left alone"
    );
}

#[test]
fn audit_verify_exits_non_zero_at_the_first_break_and_show_prints_the_records() {
    use anvil_audit::audit::chain::{GENESIS, RecordBody, seal};
    use anvil_audit::audit::{AUDIT_FILE, AuditEvent};

    let dir = tempfile::tempdir().expect("tempdir");
    let session = "01JBBBBBBBBBBBBBBBBBBBBBBB";
    let folder = dir.path().join(".anvil").join("sessions").join(session);
    std::fs::create_dir_all(&folder).expect("mkdir");
    let mut prev = GENESIS.to_owned();
    let mut lines = Vec::new();
    for (seq, outcome) in ["one", "two", "three"].iter().enumerate() {
        let body = RecordBody {
            seq: seq as u64,
            ts: "2026-11-19T21:00:00.000Z".to_owned(),
            session: session.to_owned(),
            agent: "main".to_owned(),
            user: "dev".to_owned(),
            host: "box".to_owned(),
            version: "0.0.0".to_owned(),
            commit: "unknown".to_owned(),
            binary_sha256: "unknown".to_owned(),
            policy: "unmanaged".to_owned(),
            event: if seq == 0 {
                AuditEvent::SessionStart {
                    mode: "run".to_owned(),
                    providers: Vec::new(),
                    roles: Default::default(),
                    flags: Default::default(),
                }
            } else {
                AuditEvent::SessionEnd {
                    outcome: (*outcome).to_owned(),
                }
            },
            prev: prev.clone(),
        };
        let (line, hash) = seal(&body).expect("seal");
        prev = hash;
        lines.push(line);
    }
    let file = folder.join(AUDIT_FILE);
    std::fs::write(
        &file,
        lines.iter().map(|l| format!("{l}\n")).collect::<String>(),
    )
    .expect("write");

    let intact = anvil(dir.path())
        .args(["audit", "verify", session])
        .output()
        .expect("anvil runs");
    let stdout = String::from_utf8_lossy(&intact.stdout);
    assert!(intact.status.success(), "{stdout}");
    assert!(stdout.contains("chain intact, 3 records"), "{stdout}");

    let shown = anvil(dir.path())
        .args(["audit", "show", session])
        .output()
        .expect("anvil runs");
    let stdout = String::from_utf8_lossy(&shown.stdout);
    assert!(shown.status.success(), "{stdout}");
    assert!(stdout.contains("session end: two"), "{stdout}");

    let swapped = format!("{}\n{}\n{}\n", lines[0], lines[2], lines[1]);
    let copy = dir.path().join("copy.jsonl");
    std::fs::write(&copy, swapped).expect("write");
    let broken = anvil(dir.path())
        .args(["audit", "verify"])
        .arg(&copy)
        .output()
        .expect("anvil runs");
    let stdout = String::from_utf8_lossy(&broken.stdout);
    assert_eq!(broken.status.code(), Some(1), "{stdout}");
    assert!(stdout.contains("chain broken at line 2"), "{stdout}");

    let same = dir.path().join("same.jsonl");
    std::fs::copy(&file, &same).expect("copy");
    let matched = anvil(dir.path())
        .args(["audit", "verify", session, "--mirror"])
        .arg(&same)
        .output()
        .expect("anvil runs");
    let stdout = String::from_utf8_lossy(&matched.stdout);
    assert!(matched.status.success(), "{stdout}");
    assert!(
        stdout.contains("the mirror holds the same 3 records"),
        "{stdout}"
    );

    // A session file cut at its end still verifies on its own; the mirror
    // shows the cut.
    std::fs::write(&file, format!("{}\n{}\n", lines[0], lines[1])).expect("cut");
    let cut = anvil(dir.path())
        .args(["audit", "verify", session, "--mirror"])
        .arg(&same)
        .output()
        .expect("anvil runs");
    let stdout = String::from_utf8_lossy(&cut.stdout);
    assert_eq!(cut.status.code(), Some(1), "{stdout}");
    assert!(
        stdout.contains("the mirror differs from the session file at line 3"),
        "{stdout}"
    );
}

#[test]
fn audit_verify_fails_on_an_empty_log_and_on_one_that_does_not_open_the_session() {
    use anvil_audit::audit::AuditEvent;
    use anvil_audit::audit::chain::{GENESIS, RecordBody, seal};

    let dir = tempfile::tempdir().expect("tempdir");
    let empty = dir.path().join("empty.jsonl");
    std::fs::write(&empty, "").expect("write");
    let out = anvil(dir.path())
        .args(["audit", "verify"])
        .arg(&empty)
        .output()
        .expect("anvil runs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!out.status.success(), "an empty log verified: {stdout}");
    assert!(stdout.contains("no records"), "{stdout}");

    let body = RecordBody {
        seq: 0,
        ts: "2026-11-19T21:00:00.000Z".to_owned(),
        session: "01JBBBBBBBBBBBBBBBBBBBBBBB".to_owned(),
        agent: "main".to_owned(),
        user: "dev".to_owned(),
        host: "box".to_owned(),
        version: "0.0.0".to_owned(),
        commit: "unknown".to_owned(),
        binary_sha256: "unknown".to_owned(),
        policy: "unmanaged".to_owned(),
        event: AuditEvent::SessionEnd {
            outcome: "done".to_owned(),
        },
        prev: GENESIS.to_owned(),
    };
    let (line, _) = seal(&body).expect("seal");
    let headless = dir.path().join("headless.jsonl");
    std::fs::write(&headless, format!("{line}\n")).expect("write");
    let out = anvil(dir.path())
        .args(["audit", "verify"])
        .arg(&headless)
        .output()
        .expect("anvil runs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !out.status.success(),
        "a log without its start verified: {stdout}"
    );
    assert!(stdout.contains("session_start"), "{stdout}");
}
