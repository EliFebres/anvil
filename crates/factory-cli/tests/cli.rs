//! Black-box tests of the `factory` binary.

use std::process::Command;

/// A command that runs the freshly built `factory` binary.
fn factory() -> Command {
    Command::new(env!("CARGO_BIN_EXE_factory"))
}

#[test]
fn version_prints_name_and_version() {
    let out = factory().arg("--version").output().expect("factory runs");
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        format!("factory {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn no_arguments_prints_help_and_fails() {
    let out = factory().output().expect("factory runs");
    assert!(!out.status.success());
}
