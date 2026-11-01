//! Black-box tests of the move from the old name (`forge`) to `anvil`.
//! Together with `src/legacy.rs` this is the only file that may spell the
//! old name; `just no-old-name` excludes both.

use std::path::Path;
use std::process::Command;

/// A config with one provider on a closed local port, so no ping leaves the machine.
const LOCAL_CONFIG: &str = r#"
[providers.local]
protocol = "openai"
base_url = "http://127.0.0.1:9/v1"
api_key_env = "ANVIL_TEST_LEGACY_KEY"
model = "m"
context_window = 1000
supports_tools = true
supports_reasoning = false

[roles]
coder = "local"
summarizer = "local"
"#;

/// The seeded rule text that embeds the old home path.
const OLD_RULES: &str = "check = { command = \"python ~/.forge/checks/docstrings.py\" }\n";

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, text).expect("write");
}

/// The `anvil` binary run in `project` with `ANVIL_HOME` set to `home` and
/// no old-name variable inherited from this process.
fn anvil(project: &Path, home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_anvil"));
    command
        .current_dir(project)
        .env_remove("HOME")
        .env_remove("USERPROFILE")
        .env("ANVIL_HOME", home)
        .env("ANVIL_LOG", "off")
        .env("ANVIL_TEST_LEGACY_KEY", "real-looking-key");
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("FORGE_") {
            command.env_remove(name);
        }
    }
    command
}

#[test]
fn old_home_and_project_directories_are_moved_once_with_a_notice() {
    let dir = tempfile::tempdir().expect("tempdir");
    let home = dir.path().join("home");
    let project = dir.path().join("project");
    write(&home.join(".forge").join("config.toml"), LOCAL_CONFIG);
    write(&home.join(".forge").join("rules.toml"), OLD_RULES);
    write(&project.join(".forge").join("permissions.toml"), "");

    let out = anvil(&project, &home)
        .arg("sessions")
        .output()
        .expect("anvil runs");

    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("moved ~/.forge to ~/.anvil\n"), "{stderr}");
    assert!(stderr.contains("moved .forge to .anvil\n"), "{stderr}");
    assert!(!home.join(".forge").exists());
    assert!(!project.join(".forge").exists());
    assert!(home.join(".anvil").join("config.toml").is_file());
    assert!(project.join(".anvil").join("permissions.toml").is_file());
    let rules = std::fs::read_to_string(home.join(".anvil").join("rules.toml")).expect("rules");
    assert_eq!(rules, OLD_RULES.replace("~/.forge/", "~/.anvil/"));

    let again = anvil(&project, &home)
        .arg("sessions")
        .output()
        .expect("anvil runs");
    assert!(again.status.success());
    assert!(!String::from_utf8_lossy(&again.stderr).contains("moved"));
}

#[test]
fn both_directories_present_warns_and_moves_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let home = dir.path().join("home");
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).expect("mkdir");
    write(&home.join(".forge").join("config.toml"), "# old\n");
    write(&home.join(".anvil").join("config.toml"), LOCAL_CONFIG);

    let out = anvil(&project, &home)
        .arg("sessions")
        .output()
        .expect("anvil runs");

    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("warning: both ~/.forge and ~/.anvil exist"),
        "{stderr}"
    );
    assert_eq!(
        std::fs::read_to_string(home.join(".forge").join("config.toml")).expect("old"),
        "# old\n"
    );
    assert_eq!(
        std::fs::read_to_string(home.join(".anvil").join("config.toml")).expect("new"),
        LOCAL_CONFIG
    );
}

/// The `legacy` row of `anvil doctor`, with the line after it.
fn legacy_rows(out: &std::process::Output) -> (String, String) {
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    let index = lines
        .iter()
        .position(|line| line.contains("  legacy "))
        .unwrap_or_else(|| panic!("no legacy row in:\n{stdout}"));
    let next = lines.get(index + 1).copied().unwrap_or_default();
    (lines[index].to_owned(), next.to_owned())
}

#[test]
fn doctor_fails_while_an_old_directory_or_variable_remains() {
    let dir = tempfile::tempdir().expect("tempdir");
    let home = dir.path().join("home");
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).expect("mkdir");
    write(&home.join(".anvil").join("config.toml"), LOCAL_CONFIG);
    write(&home.join(".forge").join("config.toml"), "# old\n");

    let out = anvil(&project, &home)
        .env("FORGE_HOME", "/somewhere")
        .arg("doctor")
        .output()
        .expect("anvil runs");

    assert_eq!(out.status.code(), Some(1));
    let (row, fix) = legacy_rows(&out);
    assert!(row.starts_with("FAIL  legacy"), "{row}");
    assert!(row.contains("~/.forge"), "{row}");
    assert!(row.contains("FORGE_HOME"), "{row}");
    assert!(fix.trim_start().starts_with("fix:"), "{fix}");
}

#[test]
fn doctor_passes_the_legacy_row_on_a_clean_machine() {
    let dir = tempfile::tempdir().expect("tempdir");
    let home = dir.path().join("home");
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).expect("mkdir");
    write(&home.join(".anvil").join("config.toml"), LOCAL_CONFIG);

    let out = anvil(&project, &home)
        .arg("doctor")
        .output()
        .expect("anvil runs");

    let (row, _) = legacy_rows(&out);
    assert!(row.starts_with("PASS  legacy"), "{row}");
}
