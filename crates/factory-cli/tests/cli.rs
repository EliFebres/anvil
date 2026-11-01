//! Black-box tests of the `factory` binary.

use std::path::Path;
use std::process::Command;

/// A command that runs the freshly built `factory` binary in `dir` with
/// `FACTORY_HOME` pointed at `home`, so the real home stays out of the test.
fn factory(dir: &Path, home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_factory"));
    command
        .current_dir(dir)
        .env("FACTORY_HOME", home)
        .env_remove("FACTORY_TEST_PROBE");
    command
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, text).expect("write");
}

#[test]
fn version_prints_name_and_version() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = factory(dir.path(), dir.path())
        .arg("--version")
        .output()
        .expect("factory runs");
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        format!("factory {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn no_arguments_prints_help_and_fails() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = factory(dir.path(), dir.path())
        .output()
        .expect("factory runs");
    assert!(!out.status.success());
}

#[test]
fn env_loads_factory_env_first_and_the_real_environment_wins() {
    let dir = tempfile::tempdir().expect("tempdir");
    let home = dir.path().join("home");
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).expect("mkdir");
    let factory_env = home.join(".factory").join("env");
    let anvil_env = home.join(".anvil").join(".env");
    let project_env = project.join(".anvil").join(".env");

    let run = |extra: Option<&str>| {
        let mut command = factory(&project, &home);
        if let Some(value) = extra {
            command.env("FACTORY_TEST_PROBE", value);
        }
        let out = command
            .args(["env", "FACTORY_TEST_PROBE"])
            .output()
            .expect("factory runs");
        assert!(
            out.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    };

    assert_eq!(run(None), "FACTORY_TEST_PROBE: unset");

    write(&project_env, "FACTORY_TEST_PROBE=project\n");
    let out = run(None);
    assert!(
        out.ends_with(&format!("(from {})", project_env.display())),
        "{out}"
    );

    write(&anvil_env, "FACTORY_TEST_PROBE=anvil\n");
    let out = run(None);
    assert!(
        out.ends_with(&format!("(from {})", anvil_env.display())),
        "{out}"
    );

    write(&factory_env, "FACTORY_TEST_PROBE=factory\n");
    let out = run(None);
    assert!(
        out.ends_with(&format!("(from {})", factory_env.display())),
        "{out}"
    );

    assert_eq!(
        run(Some("real")),
        "FACTORY_TEST_PROBE: set (from the environment)"
    );
    assert!(
        !run(Some("real")).contains("real"),
        "the value is never printed"
    );
}

#[test]
fn a_malformed_env_file_is_an_error_naming_the_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let home = dir.path().join("home");
    write(&home.join(".factory").join("env"), "BROKEN LINE=a b\n");
    let out = factory(dir.path(), &home)
        .args(["env", "X"])
        .output()
        .expect("factory runs");
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains(".factory"));
}
