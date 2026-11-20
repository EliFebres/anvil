//! The documents a reviewer acts on stay true to the code.
//!
//! `docs/SECURITY_TESTS.md` names the test behind every security claim, and
//! `docs/ADMIN_GUIDE.md` lists every key of the managed policy with its
//! default. Both drift silently when a test is renamed or a key is added, so
//! these tests read the documents and compare them with the source tree and
//! the policy schema.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anvil_types::policy::Policy;

/// The repository root, two levels above this crate.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Reads a document under the repository root.
fn read_doc(relative: &str) -> String {
    std::fs::read_to_string(repo_root().join(relative))
        .unwrap_or_else(|err| panic!("cannot read {relative}: {err}"))
}

/// The cells of one Markdown table row, trimmed, without the outer pipes.
fn cells(line: &str) -> Vec<String> {
    let inner = line.trim().trim_start_matches('|').trim_end_matches('|');
    inner
        .split('|')
        .map(|cell| cell.trim().to_owned())
        .collect()
}

/// Every backticked span in `text`.
fn backticked(text: &str) -> Vec<String> {
    text.split('`')
        .enumerate()
        .filter(|(index, _)| index % 2 == 1)
        .map(|(_, span)| span.to_owned())
        .collect()
}

/// The cells of column `column` in every Markdown table whose header row
/// holds that column name, separator rows left out.
fn column(document: &str, column: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut index = None;
    for line in document.lines() {
        if !line.trim_start().starts_with('|') {
            index = None;
            continue;
        }
        let row = cells(line);
        if row
            .iter()
            .all(|cell| cell.chars().all(|c| c == '-' || c == ':'))
        {
            continue;
        }
        match index {
            None => index = row.iter().position(|cell| cell == column),
            Some(at) => found.extend(row.get(at).cloned()),
        }
    }
    found
}

/// Every `.rs` file under `dir`, recursively.
fn rust_files(dir: &Path, into: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(dir).unwrap_or_else(|err| panic!("{}: {err}", dir.display()));
    for entry in entries {
        let path = entry.expect("directory entry").path();
        if path.is_dir() {
            rust_files(&path, into);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            into.push(path);
        }
    }
}

/// The names of every function defined in the workspace's sources.
fn defined_functions() -> BTreeSet<String> {
    let pattern = regex::Regex::new(r"\bfn\s+([a-z_][a-z0-9_]*)").expect("pattern compiles");
    let mut files = Vec::new();
    rust_files(&repo_root().join("crates"), &mut files);
    let mut names = BTreeSet::new();
    for file in files {
        let text = std::fs::read_to_string(&file).expect("read source");
        names.extend(pattern.captures_iter(&text).map(|c| c[1].to_owned()));
    }
    names
}

#[test]
fn every_test_named_in_security_tests_exists() {
    let document = read_doc("docs/SECURITY_TESTS.md");
    let named: BTreeSet<String> = column(&document, "test")
        .iter()
        .flat_map(|cell| backticked(cell))
        .filter(|span| {
            span.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        })
        .collect();
    assert!(
        named.len() >= 50,
        "docs/SECURITY_TESTS.md names only {} tests in its `test` columns; is the table header still `test`?",
        named.len()
    );
    let defined = defined_functions();
    let missing: Vec<&String> = named
        .iter()
        .filter(|name| !defined.contains(*name))
        .collect();
    assert!(
        missing.is_empty(),
        "docs/SECURITY_TESTS.md names tests that do not exist: {missing:?}"
    );

    let tests = column(&document, "test");
    let wheres = column(&document, "where");
    assert_eq!(tests.len(), wheres.len(), "every row has a `where` cell");
    let mut misplaced = Vec::new();
    for (test_cell, where_cell) in tests.iter().zip(&wheres) {
        let sources: Vec<String> = backticked(where_cell)
            .iter()
            .filter_map(|file| std::fs::read_to_string(repo_root().join(file)).ok())
            .collect();
        for name in backticked(test_cell) {
            let definition = regex::Regex::new(&format!(r"\bfn\s+{name}\b")).expect("name pattern");
            if !sources.iter().any(|text| definition.is_match(text)) {
                misplaced.push(name);
            }
        }
    }
    assert!(
        misplaced.is_empty(),
        "docs/SECURITY_TESTS.md names tests that are not in the files of their `where` column: {misplaced:?}"
    );
}

/// A policy file that sets every key of the schema.
const EVERY_KEY: &str = r#"
version = 1

[network]
allowed_hosts = ["models.internal.example:443"]
require_tls = true
ca_bundle = "/pki/ca.pem"
client_cert = "/pki/me.pem"
client_key = "/pki/me.key"
proxy = "system"

[models]
providers = ["endpoint"]
models = ["m"]

[permissions]
deny = ["shell:rm *"]
ask = ["shell"]
allow_yes = false
allow_allow_tool = false
allow_always = false
require_plan_approval = true

[files]
extra_roots = ["/srv/docs"]
deny = ["**/*.kdbx"]

[features]
mcp = "allowlist"
mcp_servers = [{ name = "docs", command = "docs-server" }]
hooks = false
skills = "off"
memory = false
build = false
delegation = false

[projects]
allow_trust = false

[data]
transcript_dir = "/srv/transcripts"
retention_days = 30
redaction = true
patterns = [{ kind = "employee_id", regex = 'EMP-[0-9]{6}' }]
transcript = "metadata"

[limits]
session_tokens = 1
build_tokens = 1
build_usd = 1.0

[shell]
mode = "allowlist"
allow = ["cargo test *"]
executor = "container"

[shell.container]
runtime = "docker"
image = "build@sha256:0000000000000000000000000000000000000000000000000000000000000000"
user = "65534:65534"
memory = "2g"
pids_limit = 256

[audit]
required = true
mirror_dir = "/srv/audit"
record = "digest"
"#;

/// The keys of a serialized policy, as the guide writes them: `version`,
/// `[table] key` and `[shell.container] key`.
fn schema_keys(value: &toml::Value, table: &str, into: &mut BTreeSet<String>) {
    let Some(entries) = value.as_table() else {
        return;
    };
    for (key, child) in entries {
        if child.is_table() {
            let nested = if table.is_empty() {
                key.clone()
            } else {
                format!("{table}.{key}")
            };
            schema_keys(child, &nested, into);
        } else if table.is_empty() {
            into.insert(key.clone());
        } else {
            into.insert(format!("[{table}] {key}"));
        }
    }
}

/// The keys the guide's key table lists. A cell such as
/// `` `[network] client_cert`, `client_key` `` names two keys of one table.
fn guide_keys(document: &str) -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    for cell in column(document, "key") {
        let mut table = String::new();
        for span in backticked(&cell) {
            match span
                .strip_prefix('[')
                .and_then(|rest| rest.split_once("] "))
            {
                Some((name, key)) => {
                    table = name.to_owned();
                    keys.insert(format!("[{table}] {key}"));
                }
                None if table.is_empty() => {
                    keys.insert(span);
                }
                None => {
                    keys.insert(format!("[{table}] {span}"));
                }
            }
        }
    }
    keys
}

#[test]
fn the_admin_guide_lists_every_policy_key_and_no_other() {
    let policy: Policy = toml::from_str(EVERY_KEY).expect("the fixture is a valid policy");
    let value = toml::Value::try_from(&policy).expect("a policy serializes");
    let mut schema = BTreeSet::new();
    schema_keys(&value, "", &mut schema);
    let listed = guide_keys(&read_doc("docs/ADMIN_GUIDE.md"));
    let undocumented: Vec<&String> = schema.difference(&listed).collect();
    let unknown: Vec<&String> = listed.difference(&schema).collect();
    assert!(
        undocumented.is_empty() && unknown.is_empty(),
        "docs/ADMIN_GUIDE.md and the policy schema disagree.\n\
         keys in the schema the guide does not list: {undocumented:?}\n\
         keys the guide lists that the schema does not have: {unknown:?}"
    );
}
