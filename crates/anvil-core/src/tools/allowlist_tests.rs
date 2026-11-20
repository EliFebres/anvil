//! Adversarial tests for allowlist mode: arguments that would steer an
//! allowed program out of the workspace, or into running another program.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anvil_types::shell::split_command;
use serde_json::json;

use super::*;
use crate::provider::types::ToolCall;

/// Why `command` is refused in `root`, through the argument check alone.
fn refusal(root: &Path, command: &str) -> Option<String> {
    shell::argument_refusal(root, &split_command(command).expect("splits"))
}

/// A project folder and its canonical path.
fn project() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("project");
    std::fs::write(dir.path().join("a.txt"), "inside\n").expect("write");
    let root = std::fs::canonicalize(dir.path()).expect("canonical");
    (dir, root)
}

#[test]
fn git_options_before_the_subcommand_are_refused() {
    let (_dir, root) = project();
    for command in [
        "git -c alias.x=!echo x",
        "git -c core.pager=less log",
        "git -calias.x=!sh x",
        "git --config-env=alias.x=E x",
        "git --config-env alias.x=E x",
        "git -C sub status",
        "git --git-dir=.git status",
        "git --work-tree=. status",
        "git --exec-path=. status",
        "git --namespace=x log",
        "git --super-prefix=x/ status",
        "git --attr-source=HEAD diff",
        "GIT.EXE -c alias.x=!echo x",
    ] {
        let reason = refusal(&root, command).unwrap_or_else(|| panic!("{command} allowed"));
        assert!(
            reason.contains("before the subcommand"),
            "{command}: {reason}"
        );
    }
    for command in [
        "git status",
        "git --no-pager log -1",
        "git -P diff",
        "git --version",
        "git log -1 --oneline",
        "git diff --text",
        "git diff HEAD~1..HEAD",
        "git log --output-indicator-new=+",
        "git ls-files --others",
    ] {
        assert_eq!(refusal(&root, command), None, "{command}");
    }
}

#[test]
fn git_options_that_write_read_outside_or_run_programs_are_refused() {
    let (_dir, root) = project();
    for command in [
        "git log -1 --output=out.txt",
        "git log --output out.txt",
        "git log --outp=out.txt",
        "git diff --no-index a.txt b.txt",
        "git diff --no-i a.txt b.txt",
        "git diff --ext-diff",
        "git log -p --textconv",
        "git grep -O foo",
        "git grep -Oless foo",
        "git grep --open-files-in-pager=less foo",
        "git format-patch -o patches HEAD~1",
        "git archive -oout.zip HEAD",
    ] {
        let reason = refusal(&root, command).unwrap_or_else(|| panic!("{command} allowed"));
        assert!(reason.contains("git option"), "{command}: {reason}");
    }
}

#[test]
fn arguments_that_reach_outside_the_workspace_are_refused_for_every_program() {
    let (_dir, root) = project();
    let outside = tempfile::tempdir().expect("outside");
    let out = std::fs::canonicalize(outside.path()).expect("canonical");
    let out = out.display().to_string();
    let out = out.strip_prefix(r"\\?\").unwrap_or(&out).to_owned();
    let mut refused = vec![
        "cat ../../secret".to_owned(),
        "cat sub/../../secret".to_owned(),
        "cat ~/.anvil/.env".to_owned(),
        "cat ~".to_owned(),
        format!("cat \"{out}\""),
        format!("cargo build \"--target-dir={out}\""),
        format!("cc \"-I{out}\" x.c"),
        "git diff --no-index ~/.anvil/.env NUL".to_owned(),
        "env KEY=../../x run".to_owned(),
    ];
    if cfg!(windows) {
        refused.extend(
            [
                r"cat C:\Windows\win.ini",
                r"cat \\server\share\x",
                r"git log --output=C:\x",
                r"cat \Windows\win.ini",
            ]
            .map(str::to_owned),
        );
    } else {
        refused.push("cat /etc/passwd".to_owned());
    }
    for command in &refused {
        assert!(refusal(&root, command).is_some(), "{command} was allowed");
    }
    let shown = root.display().to_string();
    let inside = shown.strip_prefix(r"\\?\").unwrap_or(&shown).to_owned();
    for command in [
        "cat a.txt".to_owned(),
        "cat sub/../a.txt".to_owned(),
        format!("cat \"{inside}/a.txt\""),
        "cargo test -- --nocapture".to_owned(),
        "git log HEAD~1..HEAD".to_owned(),
        "cat NUL".to_owned(),
    ] {
        assert_eq!(refusal(&root, &command), None, "{command}");
    }
}

/// Whether git can be started here.
fn git_available() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success())
}

/// A registry whose only shell limit is `shell_programs = ["git"]`, so
/// every git command matches `git *` and only the argument checks stop it.
fn git_star_registry() -> ToolRegistry {
    let rules = "default = \"allow\"\nallow = [\"shell\"]\nshell_programs = [\"git\"]\n";
    ToolRegistry::with_builtins(
        Permissions::from_toml(rules, None).expect("rules"),
        Box::new(StaticPrompt(PromptAnswer::Yes)),
    )
}

#[tokio::test]
async fn under_git_star_an_alias_cannot_run_and_output_cannot_land_outside() {
    if !git_available() {
        return;
    }
    let (dir, _root) = project();
    let made = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(dir.path())
        .status()
        .expect("git runs");
    assert!(made.success());
    let outside = tempfile::tempdir().expect("outside");
    let target = outside.path().join("w.txt");
    let ctx = ToolContext::new(dir.path());
    let registry = git_star_registry();
    for command in [
        "git -c alias.x=!echo ALIAS-RAN x".to_owned(),
        "git -c alias.p=push p origin main".to_owned(),
        format!("git log -1 --format=x \"--output={}\"", target.display()),
        "git diff --no-index ~/.anvil/.env NUL".to_owned(),
    ] {
        let call = ToolCall {
            id: "a1".to_owned(),
            name: "shell".to_owned(),
            arguments_json: json!({ "command": command }).to_string(),
        };
        let run = registry.execute(&call, &ctx).await;
        assert!(run.result.is_error, "{command}: {}", run.result.content);
        assert!(
            !run.result.content.contains("ALIAS-RAN"),
            "{command}: {}",
            run.result.content
        );
    }
    assert!(!target.exists(), "git wrote outside the workspace");
}

#[tokio::test]
async fn the_direct_runner_refuses_steering_arguments_itself() {
    let (dir, _root) = project();
    let mut ctx = ToolContext::new(dir.path());
    ctx.runner.direct = true;
    for command in ["git -c alias.x=!echo ALIAS-RAN x", "cat ../../secret"] {
        let run = shell::run_command(&ctx, command, Duration::from_secs(20)).await;
        let error = run.expect_err(command);
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput, "{command}");
    }
}
