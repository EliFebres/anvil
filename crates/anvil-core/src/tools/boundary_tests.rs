//! Adversarial tests for the workspace boundary and the secret-path list:
//! every file tool, every spelling a model might try.

use std::path::Path;

use serde_json::{Value, json};

use super::*;
use crate::provider::types::ToolCall;

/// Runs one built-in tool with every call allowed by the rules, so only
/// the boundary can stop it.
async fn unrestricted(ctx: &ToolContext, tool: &str, args: Value) -> Execution {
    let permissions = Permissions::from_toml("default = \"allow\"", None).expect("rules");
    let registry =
        ToolRegistry::with_builtins(permissions, Box::new(StaticPrompt(PromptAnswer::Yes)));
    let call = ToolCall {
        id: "b1".to_owned(),
        name: tool.to_owned(),
        arguments_json: args.to_string(),
    };
    registry.execute(&call, ctx).await
}

/// Every file tool aimed at `path`, with arguments that would succeed on
/// an ordinary file.
fn every_file_tool(path: &str) -> Vec<(&'static str, Value)> {
    vec![
        ("read_file", json!({"path": path})),
        ("write_file", json!({"path": path, "content": "x"})),
        (
            "edit_file",
            json!({"path": path, "old": "outside", "new": "x"}),
        ),
        ("grep", json!({"pattern": "outside", "path": path})),
        ("glob", json!({"pattern": "*", "path": path})),
        ("list_dir", json!({"path": path})),
        ("outline", json!({"path": path})),
    ]
}

/// Asserts that every file tool refuses `path` without revealing `secret`.
async fn assert_every_tool_refuses(ctx: &ToolContext, path: &str, secret: &str) {
    for (tool, args) in every_file_tool(path) {
        let run = unrestricted(ctx, tool, args).await;
        assert!(run.result.is_error, "{tool} {path}: {}", run.result.content);
        assert!(
            !run.result.content.contains(secret),
            "{tool} {path} revealed the file: {}",
            run.result.content
        );
    }
}

/// A project folder and, next to it, a folder outside it holding `out.txt`.
fn project_and_outside() -> (tempfile::TempDir, tempfile::TempDir, ToolContext) {
    let project = tempfile::tempdir().expect("project");
    let outside = tempfile::tempdir().expect("outside");
    std::fs::create_dir_all(project.path().join("sub").join("deeper")).expect("mkdir");
    std::fs::write(outside.path().join("out.txt"), "outside secret\n").expect("write");
    let ctx = ToolContext::new(project.path());
    (project, outside, ctx)
}

/// The outside folder's name, for building relative escapes.
fn name_of(dir: &Path) -> String {
    dir.file_name()
        .expect("name")
        .to_string_lossy()
        .into_owned()
}

#[tokio::test]
async fn boundary_dot_dot_traversal_is_refused() {
    let (_project, outside, ctx) = project_and_outside();
    let out = name_of(outside.path());
    let spellings = [
        format!("../{out}/out.txt"),
        format!("./../{out}/out.txt"),
        format!("sub/../../{out}/out.txt"),
        format!("sub/deeper/../../../{out}/out.txt"),
        format!("missing/../../{out}/out.txt"),
        format!("sub/./../.././{out}/out.txt"),
        format!("../{out}"),
    ];
    for path in &spellings {
        assert_every_tool_refuses(&ctx, path, "outside secret").await;
    }
    if cfg!(windows) {
        assert_every_tool_refuses(&ctx, &format!(r"..\{out}\out.txt"), "outside secret").await;
        assert_every_tool_refuses(&ctx, &format!(r"sub\..\..\{out}\out.txt"), "outside secret")
            .await;
    }
    assert_eq!(
        std::fs::read_to_string(outside.path().join("out.txt")).expect("read"),
        "outside secret\n",
        "nothing outside was changed"
    );
}

#[tokio::test]
async fn boundary_absolute_paths_outside_are_refused() {
    let (_project, outside, ctx) = project_and_outside();
    let file = outside.path().join("out.txt").display().to_string();
    let folder = outside.path().display().to_string();
    for path in [file.as_str(), folder.as_str()] {
        assert_every_tool_refuses(&ctx, path, "outside secret").await;
    }
    let system = if cfg!(windows) {
        r"C:\Windows\win.ini"
    } else {
        "/etc/passwd"
    };
    for (tool, args) in every_file_tool(system) {
        let run = unrestricted(&ctx, tool, args).await;
        assert!(
            run.result.content.contains("outside the workspace"),
            "{tool} {system}: {}",
            run.result.content
        );
    }
    let home = std::env::home_dir().expect("home").display().to_string();
    let run = unrestricted(&ctx, "list_dir", json!({"path": home})).await;
    assert!(run.result.is_error, "{}", run.result.content);
}

#[cfg(unix)]
#[tokio::test]
async fn boundary_symlink_escape_is_refused() {
    let (project, outside, ctx) = project_and_outside();
    std::os::unix::fs::symlink(outside.path(), project.path().join("dir-link")).expect("symlink");
    std::os::unix::fs::symlink(
        outside.path().join("out.txt"),
        project.path().join("file-link"),
    )
    .expect("symlink");
    std::os::unix::fs::symlink("../..", project.path().join("sub").join("up")).expect("symlink");
    let out = name_of(outside.path());
    for path in [
        "dir-link/out.txt".to_owned(),
        "dir-link".to_owned(),
        "file-link".to_owned(),
        format!("sub/up/{out}/out.txt"),
    ] {
        assert_every_tool_refuses(&ctx, &path, "outside secret").await;
    }
    // The pattern is echoed in the result's header line, so the check looks for
    // a match from the outside file, which would name it, not for the pattern.
    let grep = unrestricted(&ctx, "grep", json!({"pattern": "outside secret"})).await;
    assert!(
        !grep.result.content.contains("out.txt"),
        "a search of the project does not follow links out: {}",
        grep.result.content
    );
}

#[cfg(windows)]
#[tokio::test]
async fn boundary_windows_drive_paths_are_refused() {
    let (project, _outside, ctx) = project_and_outside();
    let drive = project
        .path()
        .to_string_lossy()
        .chars()
        .next()
        .expect("drive letter");
    let other = if drive.eq_ignore_ascii_case(&'Q') {
        'R'
    } else {
        'Q'
    };
    for path in [
        r"C:\Windows\win.ini".to_owned(),
        r"c:\windows\WIN.INI".to_owned(),
        r"C:/Windows/win.ini".to_owned(),
        format!(r"{other}:\data\out.txt"),
        format!("{drive}:out.txt"),
        format!(r"{drive}:\"),
    ] {
        for (tool, args) in every_file_tool(&path) {
            let run = unrestricted(&ctx, tool, args).await;
            assert!(run.result.is_error, "{tool} {path}: {}", run.result.content);
            assert!(
                !run.result.content.contains("[fonts]"),
                "{tool} {path}: {}",
                run.result.content
            );
        }
    }
}

#[cfg(windows)]
#[tokio::test]
async fn boundary_windows_unc_and_device_paths_are_refused() {
    let (_project, outside, ctx) = project_and_outside();
    let file = outside.path().join("out.txt").display().to_string();
    let without_drive = file.get(2..).expect("a drive path");
    let drive = file.chars().next().expect("drive letter");
    for path in [
        format!(r"\\?\{file}"),
        format!(r"\\.\{file}"),
        format!(r"\\localhost\{drive}${without_drive}"),
        format!(r"\\127.0.0.1\{drive}${without_drive}"),
        format!(r"\\?\UNC\localhost\{drive}${without_drive}"),
        r"\\?\C:\Windows\win.ini".to_owned(),
        r"\\.\pipe\anvil".to_owned(),
    ] {
        assert_every_tool_refuses(&ctx, &path, "outside secret").await;
    }
}

#[cfg(windows)]
#[tokio::test]
async fn boundary_windows_case_variants_resolve_to_one_answer() {
    let (project, outside, ctx) = project_and_outside();
    std::fs::write(project.path().join("In.txt"), "inside\n").expect("write");
    let upper = outside.path().display().to_string().to_uppercase();
    let lower = outside.path().display().to_string().to_lowercase();
    for folder in [upper, lower] {
        assert_every_tool_refuses(&ctx, &format!(r"{folder}\OUT.TXT"), "outside secret").await;
    }
    let inside_upper = project
        .path()
        .join("in.txt")
        .display()
        .to_string()
        .to_uppercase();
    let run = unrestricted(&ctx, "read_file", json!({"path": inside_upper})).await;
    assert!(
        run.result.content.contains("inside"),
        "another spelling of a file inside stays inside: {}",
        run.result.content
    );
}

#[cfg(not(windows))]
#[tokio::test]
async fn boundary_windows_spellings_are_plain_names_elsewhere() {
    let (project, _outside, ctx) = project_and_outside();
    for path in [r"C:\Windows\win.ini", r"\\?\C:\x", r"\\server\share\x"] {
        let resolved = permissions::resolve_path(
            &std::fs::canonicalize(project.path()).expect("canonical"),
            path,
        )
        .expect("resolves");
        assert!(
            workspace::is_within(project.path(), &resolved),
            "{path} stays a name inside the project: {}",
            resolved.display()
        );
        let run = unrestricted(&ctx, "read_file", json!({"path": path})).await;
        assert!(run.result.is_error, "{path}: {}", run.result.content);
    }
}

/// Files with every kind of secret name, each holding `needle`.
const SECRET_NAMES: [&str; 16] = [
    ".env",
    ".env.production",
    "server.pem",
    "tls.key",
    "cert.pfx",
    "cert.p12",
    "id_rsa",
    "id_rsa.pub",
    "id_ed25519",
    "credentials",
    "credentials.json",
    ".netrc",
    ".npmrc",
    ".pypirc",
    ".ssh/config",
    ".aws/credentials",
];

#[tokio::test]
async fn secret_paths_are_refused_inside_the_workspace_and_skipped_by_searches() {
    let project = tempfile::tempdir().expect("project");
    for name in SECRET_NAMES {
        let path = project.path().join(name);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, "needle\n").expect("write");
    }
    std::fs::create_dir_all(project.path().join(".git")).expect("mkdir");
    std::fs::write(
        project.path().join(".git").join("config"),
        "[remote \"origin\"]\nurl = https://needle@example.com/r.git\n",
    )
    .expect("write");
    std::fs::write(project.path().join("notes.txt"), "needle here\n").expect("write");
    let ctx = ToolContext::new(project.path());
    for name in SECRET_NAMES.iter().copied().chain([".git/config"]) {
        for path in [
            name.to_owned(),
            format!("./{name}"),
            format!("sub/../{name}"),
        ] {
            for tool in ["read_file", "outline", "grep"] {
                let args = if tool == "grep" {
                    json!({"pattern": "needle", "path": path})
                } else {
                    json!({"path": path})
                };
                let run = unrestricted(&ctx, tool, args).await;
                assert!(
                    run.result.content.contains("holds secrets"),
                    "{tool} {path}: {}",
                    run.result.content
                );
            }
        }
    }
    let grep = unrestricted(&ctx, "grep", json!({"pattern": "needle", "path": "."})).await;
    assert!(
        grep.result.content.contains("notes.txt"),
        "{}",
        grep.result.content
    );
    assert_eq!(
        grep.result.content.matches("needle").count(),
        1,
        "only notes.txt matched: {}",
        grep.result.content
    );
    let glob = unrestricted(&ctx, "glob", json!({"pattern": "**/*"})).await;
    for name in [
        "server.pem",
        "tls.key",
        "id_rsa",
        "credentials.json",
        ".npmrc",
    ] {
        assert!(
            !glob.result.content.contains(name),
            "glob listed {name}: {}",
            glob.result.content
        );
    }
    let list = unrestricted(&ctx, "list_dir", json!({})).await;
    assert!(
        list.result.content.contains("notes.txt"),
        "{}",
        list.result.content
    );
    for name in [
        "server.pem",
        "tls.key",
        "id_rsa",
        "credentials",
        ".env",
        ".npmrc",
    ] {
        assert!(
            !list.result.content.lines().any(|line| line == name),
            "list_dir listed {name}: {}",
            list.result.content
        );
    }
}

#[tokio::test]
async fn a_policy_deny_glob_extends_the_secret_list() {
    let project = tempfile::tempdir().expect("project");
    // A file type the built-in list does not cover, so only the policy refuses it.
    std::fs::write(project.path().join("customers.sqlite"), "needle\n").expect("write");
    let mut ctx = ToolContext::new(project.path());
    ctx.workspace.denied = workspace::deny_globs(&["**/*.sqlite".to_owned()]).expect("compiles");
    let run = unrestricted(&ctx, "read_file", json!({"path": "customers.sqlite"})).await;
    assert!(
        run.result.content.contains("[files] deny"),
        "{}",
        run.result.content
    );
    let list = unrestricted(&ctx, "list_dir", json!({})).await;
    assert!(
        !list.result.content.contains("customers.sqlite"),
        "{}",
        list.result.content
    );
}

/// Spellings Win32 folds onto a secret, a denied file or git's config:
/// a trailing dot or space, on any component.
const FOLDED_SPELLINGS: [&str; 9] = [
    "server.pem.",
    "server.pem ",
    ".env ",
    ".env. ",
    ".git/config.",
    ".git/config ",
    ".git./config",
    "vault.kdbx.",
    "notes.txt::$DATA",
];

#[tokio::test]
async fn folded_names_are_refused_for_reads_and_searches_on_every_system() {
    let project = tempfile::tempdir().expect("project");
    for name in [
        "server.pem",
        ".env",
        ".git/config",
        "vault.kdbx",
        "notes.txt",
    ] {
        let path = project.path().join(name);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, "needle\n").expect("write");
    }
    let mut ctx = ToolContext::new(project.path());
    ctx.workspace.denied = workspace::deny_globs(&["**/*.kdbx".to_owned()]).expect("compiles");
    for path in FOLDED_SPELLINGS {
        for (tool, args) in [
            ("read_file", json!({"path": path})),
            ("outline", json!({"path": path})),
            ("grep", json!({"pattern": "needle", "path": path})),
            ("list_dir", json!({"path": path})),
            ("glob", json!({"pattern": "*", "path": path})),
        ] {
            let run = unrestricted(&ctx, tool, args).await;
            assert!(
                run.result.is_error,
                "{tool} {path:?}: {}",
                run.result.content
            );
            assert!(
                run.result
                    .content
                    .contains("Windows would open a different file"),
                "{tool} {path:?}: {}",
                run.result.content
            );
            assert!(!run.result.content.contains("needle"), "{tool} {path:?}");
        }
    }
}

#[cfg(windows)]
#[tokio::test]
async fn boundary_windows_folded_junction_names_are_refused() {
    let (project, outside, ctx) = project_and_outside();
    let made = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(project.path().join("jx"))
        .arg(outside.path())
        .output()
        .expect("cmd runs");
    assert!(
        made.status.success(),
        "{}",
        String::from_utf8_lossy(&made.stderr)
    );
    for path in ["jx./out.txt", "jx /out.txt", "jx.", "jx. "] {
        for (tool, args) in [
            ("read_file", json!({"path": path})),
            ("outline", json!({"path": path})),
            ("grep", json!({"pattern": "outside", "path": path})),
            ("list_dir", json!({"path": path})),
            ("glob", json!({"pattern": "*", "path": path})),
        ] {
            let run = unrestricted(&ctx, tool, args).await;
            assert!(
                run.result.is_error,
                "{tool} {path:?}: {}",
                run.result.content
            );
            assert!(
                !run.result.content.contains("outside secret")
                    && !run.result.content.contains("out.txt\n"),
                "{tool} {path:?} reached the outside folder: {}",
                run.result.content
            );
        }
    }
}
