//! Every GitHub Actions workflow declares least-privilege permissions.
//!
//! The repository default for `GITHUB_TOKEN` is read-only, and this test
//! keeps it that way: each workflow must carry a top-level `permissions:`
//! block, and only `release.yml` may write, and then only to `contents`.
//! The block is read line by line so forge-types needs no YAML crate.

use std::path::{Path, PathBuf};

/// The workflow directory relative to this crate.
fn workflows_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.github/workflows")
}

/// The only file that may write, and the only scope it may write to.
const WRITER: &str = "release.yml";
const WRITER_SCOPE: &str = "contents";

/// The top-level `permissions:` of a workflow.
#[derive(Debug, PartialEq, Eq)]
enum Permissions {
    /// `permissions: read-all`.
    ReadAll,
    /// `permissions: write-all`.
    WriteAll,
    /// One `scope: level` pair per entry, block or flow form.
    Scopes(Vec<(String, String)>),
}

/// Finds the top-level `permissions:` block; `None` when the file has none.
///
/// A top-level key starts in column 0. The block form lists indented
/// `scope: level` lines; the inline form is `read-all`, `write-all` or a flow
/// mapping such as `{contents: read, pull-requests: write}`.
fn top_level_permissions(text: &str) -> Option<Permissions> {
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        let Some(rest) = line.strip_prefix("permissions:") else {
            continue;
        };
        let inline = strip_comment(rest).trim();
        if !inline.is_empty() {
            return Some(inline_permissions(inline));
        }
        let mut scopes = Vec::new();
        while let Some(next) = lines.peek() {
            let body = strip_comment(next);
            if body.trim().is_empty() {
                lines.next();
                continue;
            }
            if !body.starts_with(char::is_whitespace) {
                break;
            }
            scopes.push(split_pair(body.trim()));
            lines.next();
        }
        return Some(Permissions::Scopes(scopes));
    }
    None
}

/// Parses the value written on the same line as `permissions:`.
fn inline_permissions(value: &str) -> Permissions {
    match value {
        "read-all" => Permissions::ReadAll,
        "write-all" => Permissions::WriteAll,
        flow => {
            let inner = flow.trim_start_matches('{').trim_end_matches('}');
            let scopes = inner
                .split(',')
                .map(str::trim)
                .filter(|entry| !entry.is_empty())
                .map(split_pair)
                .collect();
            Permissions::Scopes(scopes)
        }
    }
}

/// Splits `scope: level` into its two trimmed halves.
fn split_pair(entry: &str) -> (String, String) {
    let (scope, level) = entry.split_once(':').unwrap_or((entry, ""));
    (scope.trim().to_owned(), level.trim().to_owned())
}

/// Drops a trailing `# comment`.
fn strip_comment(line: &str) -> &str {
    line.split_once(" #")
        .map_or(line, |(body, _)| body)
        .trim_end_matches('#')
}

/// Everything wrong with one workflow's permissions, in file order.
fn problems(file_name: &str, permissions: Option<&Permissions>) -> Vec<String> {
    match permissions {
        None => vec!["lacks a top-level `permissions:` block".to_owned()],
        Some(Permissions::ReadAll) => Vec::new(),
        Some(Permissions::WriteAll) => vec!["grants `write-all`".to_owned()],
        Some(Permissions::Scopes(scopes)) => scopes
            .iter()
            .filter_map(|(scope, level)| match level.as_str() {
                "read" | "none" => None,
                "write" if file_name == WRITER && scope == WRITER_SCOPE => None,
                "write" => Some(format!(
                    "grants `{scope}: write`; only `{WRITER}` may write, and only to `{WRITER_SCOPE}`"
                )),
                other => Some(format!("has `{scope}: {other}`, which is not a permission level")),
            })
            .collect(),
    }
}

#[test]
fn every_workflow_declares_least_privilege_permissions() {
    let dir = workflows_dir();
    let entries =
        std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()));
    let mut checked = 0;
    let mut failures = Vec::new();
    for entry in entries {
        let path = entry.expect("readable directory entry").path();
        let is_workflow = path
            .extension()
            .is_some_and(|ext| ext == "yml" || ext == "yaml");
        if !is_workflow {
            continue;
        }
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("utf-8 file name")
            .to_owned();
        let text = std::fs::read_to_string(&path).expect("readable workflow");
        let permissions = top_level_permissions(&text);
        for problem in problems(&file_name, permissions.as_ref()) {
            failures.push(format!("{}: {problem}", path.display()));
        }
        checked += 1;
    }
    assert!(checked > 0, "no workflows found in {}", dir.display());
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn block_form_is_parsed_and_judged() {
    let text = "name: x\n\npermissions:\n  contents: read  # the checkout\n\n  pull-requests: write\n\njobs:\n  a: {}\n";
    let permissions = top_level_permissions(text).expect("found");
    assert_eq!(
        permissions,
        Permissions::Scopes(vec![
            ("contents".to_owned(), "read".to_owned()),
            ("pull-requests".to_owned(), "write".to_owned()),
        ])
    );
    let found = problems("ci.yml", Some(&permissions));
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].contains("pull-requests: write"), "{}", found[0]);
}

#[test]
fn only_release_may_write_contents() {
    let release = top_level_permissions("permissions: {contents: write}\n").expect("found");
    assert!(problems("release.yml", Some(&release)).is_empty());
    assert_eq!(problems("ci.yml", Some(&release)).len(), 1);
}

#[test]
fn missing_nested_and_write_all_are_rejected() {
    assert_eq!(
        top_level_permissions("jobs:\n  a:\n    permissions:\n      contents: write\n"),
        None
    );
    assert_eq!(problems("ci.yml", None).len(), 1);
    assert_eq!(
        top_level_permissions("permissions: write-all\n"),
        Some(Permissions::WriteAll)
    );
    assert_eq!(problems("ci.yml", Some(&Permissions::WriteAll)).len(), 1);
    assert!(problems("ci.yml", Some(&Permissions::ReadAll)).is_empty());
}
