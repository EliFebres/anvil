//! Black-box tests of the `forge` binary.

use std::process::Command;

/// A command that runs the freshly built `forge` binary.
fn forge() -> Command {
    Command::new(env!("CARGO_BIN_EXE_forge"))
}

#[test]
fn version_prints_name_and_version() {
    let out = forge().arg("--version").output().expect("forge runs");
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        format!("forge {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn no_arguments_prints_help_and_fails() {
    let out = forge().output().expect("forge runs");
    assert!(!out.status.success());
}

#[test]
fn config_init_writes_a_valid_template_and_refuses_to_overwrite() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = forge()
        .args(["config", "init"])
        .current_dir(dir.path())
        .output()
        .expect("forge runs");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let path = dir.path().join(".forge").join("config.toml");
    let text = std::fs::read_to_string(&path).expect("config written");
    let config: forge_types::ForgeConfig = toml::from_str(&text).expect("template parses");
    config.validate().expect("template validates");

    let second = forge()
        .args(["config", "init"])
        .current_dir(dir.path())
        .output()
        .expect("forge runs");
    assert!(!second.status.success(), "second init must fail");
    assert!(
        String::from_utf8_lossy(&second.stderr).contains("already exists"),
        "stderr: {}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(&path).expect("still there"),
        text,
        "existing config must be untouched"
    );
}
