//! Every GitHub Actions workflow declares least-privilege permissions.
//!
//! The repository default for `GITHUB_TOKEN` is read-only, and this test
//! keeps it that way: each workflow must carry a top-level `permissions:`
//! block, and only `release.yml` may write, and then only to the scopes a
//! release needs: `contents` for the release itself, `id-token` and
//! `attestations` for the build provenance attestation. A job-level
//! `permissions:` block may only narrow (`read` or `none`), so no job can
//! widen what the top of the file grants. The blocks are read line by line so
//! anvil-types needs no YAML crate.

use std::path::{Path, PathBuf};

/// The workflow directory relative to this crate.
fn workflows_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.github/workflows")
}

/// The only file that may write.
const WRITER: &str = "release.yml";
/// The only scopes `WRITER` may write to: the release and its assets, the
/// OIDC token that signs the attestation, and the attestation store.
const WRITER_SCOPES: &[&str] = &["contents", "id-token", "attestations"];

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
                "write" if file_name == WRITER && WRITER_SCOPES.contains(&scope.as_str()) => None,
                "write" => Some(format!(
                    "grants `{scope}: write`; only `{WRITER}` may write, and only to {WRITER_SCOPES:?}"
                )),
                other => Some(format!("has `{scope}: {other}`, which is not a permission level")),
            })
            .collect(),
    }
}

/// Every job-level `permissions:` entry that grants more than `read`.
///
/// A job-level block is an indented `permissions:` line followed by lines
/// indented deeper, or an inline value on the same line. Only `read` and
/// `none` are accepted there, so a job can narrow the file's grant but never
/// widen it.
fn job_level_problems(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        let body = strip_comment(line);
        let indent = body.len() - body.trim_start().len();
        let Some(rest) = body.trim_start().strip_prefix("permissions:") else {
            continue;
        };
        if indent == 0 {
            continue;
        }
        let inline = rest.trim();
        let permissions = if inline.is_empty() {
            let mut scopes = Vec::new();
            while let Some(next) = lines.peek() {
                let next_body = strip_comment(next);
                if next_body.trim().is_empty() {
                    lines.next();
                    continue;
                }
                let next_indent = next_body.len() - next_body.trim_start().len();
                if next_indent <= indent {
                    break;
                }
                scopes.push(split_pair(next_body.trim()));
                lines.next();
            }
            Permissions::Scopes(scopes)
        } else {
            inline_permissions(inline)
        };
        match permissions {
            Permissions::ReadAll => {}
            Permissions::WriteAll => found.push("a job grants `write-all`".to_owned()),
            Permissions::Scopes(pairs) => {
                for (scope, level) in pairs {
                    if level != "read" && level != "none" {
                        found.push(format!(
                            "a job grants `{scope}: {level}`; job-level permissions may only narrow"
                        ));
                    }
                }
            }
        }
    }
    found
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
        for problem in problems(&file_name, permissions.as_ref())
            .into_iter()
            .chain(job_level_problems(&text))
        {
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
fn release_may_write_only_the_attestation_scopes_besides_contents() {
    let text = "permissions:\n  contents: write\n  id-token: write\n  attestations: write\n";
    let release = top_level_permissions(text).expect("found");
    assert!(problems("release.yml", Some(&release)).is_empty());
    assert_eq!(problems("ci.yml", Some(&release)).len(), 3);
    for scope in ["packages", "pull-requests", "actions", "artifact-metadata"] {
        let wider =
            top_level_permissions(&format!("permissions: {{{scope}: write}}\n")).expect("found");
        assert_eq!(
            problems("release.yml", Some(&wider)).len(),
            1,
            "release.yml must not write {scope}"
        );
    }
}

#[test]
fn a_job_may_narrow_but_never_widen() {
    let narrow = "permissions:\n  contents: write\njobs:\n  build:\n    permissions:\n      contents: read\n    steps: []\n";
    assert!(job_level_problems(narrow).is_empty());
    let wide = "permissions:\n  contents: read\njobs:\n  a:\n    permissions:\n      contents: write # sneaky\n    steps: []\n  b:\n    permissions: write-all\n  c:\n    permissions: {packages: write}\n";
    let found = job_level_problems(wide);
    assert_eq!(found.len(), 3, "{found:?}");
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

/// The reference after `@` in a `uses:` line, when it is not a full commit SHA.
///
/// Local actions (`./path`) and container actions (`docker://`) carry no
/// `@ref` to pin and are returned as problems too, since no workflow here uses one.
fn unpinned_reference(line: &str) -> Option<String> {
    let body = strip_comment(line).trim();
    let target = body
        .strip_prefix("- uses:")
        .or_else(|| body.strip_prefix("uses:"))?
        .trim();
    let pinned = target.split_once('@').is_some_and(|(_, reference)| {
        reference.len() == 40 && reference.chars().all(|c| c.is_ascii_hexdigit())
    });
    (!pinned).then(|| target.to_owned())
}

/// Every action a workflow runs is pinned to a full 40-character commit SHA,
/// because a tag or branch can be moved to other code after review.
#[test]
fn every_action_is_pinned_to_a_commit_sha() {
    let mut failures = Vec::new();
    let mut checked = 0;
    for entry in std::fs::read_dir(workflows_dir()).expect("workflows dir") {
        let path = entry.expect("entry").path();
        let text = std::fs::read_to_string(&path).expect("read workflow");
        for line in text.lines() {
            if !strip_comment(line)
                .trim_start()
                .trim_start_matches("- ")
                .starts_with("uses:")
            {
                continue;
            }
            checked += 1;
            if let Some(target) = unpinned_reference(line) {
                failures.push(format!(
                    "{}: `{target}` is not pinned to a commit SHA",
                    path.display()
                ));
            }
        }
    }
    assert!(checked > 0, "no `uses:` lines found");
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn tags_branches_and_short_shas_are_not_pins() {
    assert_eq!(
        unpinned_reference("      - uses: actions/checkout@v4"),
        Some("actions/checkout@v4".to_owned())
    );
    assert!(unpinned_reference("      - uses: dtolnay/rust-toolchain@master").is_some());
    assert!(unpinned_reference("      - uses: actions/checkout@3d3c42e").is_some());
    assert!(unpinned_reference("      - uses: ./local-action").is_some());
    assert_eq!(
        unpinned_reference(
            "      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1"
        ),
        None
    );
}

/// A release is published only from a version tag: the trigger names `v*`
/// tags and nothing else, and the build enforces the lockfile.
#[test]
fn release_runs_only_on_version_tags_and_builds_locked() {
    let path = workflows_dir().join(WRITER);
    let text = std::fs::read_to_string(&path).expect("release.yml");
    let on_block: Vec<&str> = text
        .lines()
        .skip_while(|line| !line.starts_with("on:"))
        .skip(1)
        .take_while(|line| line.is_empty() || line.starts_with(char::is_whitespace))
        .map(|line| strip_comment(line).trim())
        .filter(|line| !line.is_empty())
        .collect();
    assert_eq!(on_block, ["push:", "tags: ['v*']"], "release.yml trigger");
    let builds: Vec<&str> = text
        .lines()
        .filter(|line| line.contains("cargo build"))
        .collect();
    assert!(!builds.is_empty(), "release.yml builds nothing");
    for line in builds {
        assert!(line.contains("--locked"), "not locked: {line}");
    }
}

#[test]
fn release_publishes_checksums_an_sbom_vendored_source_and_provenance() {
    let path = workflows_dir().join(WRITER);
    let text = std::fs::read_to_string(&path).expect("release.yml");
    let steps: Vec<&str> = text
        .lines()
        .map(|line| strip_comment(line).trim())
        .collect();
    let has = |needle: &str| steps.iter().any(|line| line.contains(needle));
    for needle in [
        "> SHA256SUMS",
        "cargo cyclonedx",
        "cargo vendor --locked",
        "-vendored-src.tar.gz",
        "uses: actions/attest@",
        "subject-checksums: dist/SHA256SUMS",
    ] {
        assert!(has(needle), "release.yml has no step with `{needle}`");
    }
}

/// The channel pinned in the repository's `rust-toolchain.toml`.
fn pinned_toolchain() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../rust-toolchain.toml");
    let text = std::fs::read_to_string(&path).expect("rust-toolchain.toml exists");
    let table: toml::Table = toml::from_str(&text).expect("rust-toolchain.toml parses");
    table["toolchain"]["channel"]
        .as_str()
        .expect("toolchain.channel is a string")
        .to_owned()
}

/// Every Rust install in a workflow names the pinned version, so CI formats
/// and lints with the same toolchain as the host.
#[test]
fn every_workflow_installs_the_pinned_toolchain() {
    let pinned = pinned_toolchain();
    let mut installs = 0;
    for entry in std::fs::read_dir(workflows_dir()).expect("workflows dir") {
        let path = entry.expect("entry").path();
        let text = std::fs::read_to_string(&path).expect("read workflow");
        let name = path.display();
        let uses = text
            .lines()
            .filter(|line| line.contains("dtolnay/rust-toolchain@"))
            .count();
        let versions: Vec<&str> = text
            .lines()
            .filter_map(|line| strip_comment(line).trim().strip_prefix("toolchain:"))
            .map(str::trim)
            .collect();
        assert_eq!(
            versions.len(),
            uses,
            "{name}: every dtolnay/rust-toolchain step sets `toolchain:`"
        );
        for version in versions {
            assert_eq!(version, pinned, "{name}: must match rust-toolchain.toml");
        }
        installs += uses;
    }
    assert!(installs > 0, "no workflow installs Rust");
}
