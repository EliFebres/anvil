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

/// `code` with comments, string literals and character literals replaced
/// by spaces of the same byte length, line breaks kept, so a name inside
/// them is never read as code and offsets still match the original.
fn strip(code: &str) -> String {
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    let chars: Vec<(usize, char)> = code.char_indices().collect();
    let byte = |index: usize| chars.get(index).map_or(code.len(), |(at, _)| *at);
    let ch = |index: usize| chars.get(index).map(|(_, c)| *c);
    let mut blanked: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i].1;
        let end = if c == '/' && ch(i + 1) == Some('/') {
            (i..chars.len())
                .find(|&k| chars[k].1 == '\n')
                .unwrap_or(chars.len())
        } else if c == '/' && ch(i + 1) == Some('*') {
            let mut depth = 0usize;
            let mut k = i;
            while k < chars.len() {
                if ch(k) == Some('/') && ch(k + 1) == Some('*') {
                    depth += 1;
                    k += 2;
                } else if ch(k) == Some('*') && ch(k + 1) == Some('/') {
                    depth -= 1;
                    k += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    k += 1;
                }
            }
            k.min(chars.len())
        } else if c == 'r'
            && matches!(ch(i + 1), Some('"' | '#'))
            && (i == 0 || !is_ident(chars[i - 1].1) || chars[i - 1].1 == 'b')
        {
            let hashes = (i + 1..chars.len())
                .take_while(|&k| chars[k].1 == '#')
                .count();
            if ch(i + 1 + hashes) == Some('"') {
                let mut k = i + 2 + hashes;
                while k < chars.len()
                    && !(chars[k].1 == '"' && (0..hashes).all(|h| ch(k + 1 + h) == Some('#')))
                {
                    k += 1;
                }
                (k + 1 + hashes).min(chars.len())
            } else {
                i + 1
            }
        } else if c == '"' {
            let mut k = i + 1;
            while k < chars.len() && chars[k].1 != '"' {
                k += if chars[k].1 == '\\' { 2 } else { 1 };
            }
            (k + 1).min(chars.len())
        } else if c == '\'' && ch(i + 1) == Some('\\') {
            (i + 3..chars.len())
                .find(|&k| chars[k].1 == '\'')
                .map_or(i + 1, |k| k + 1)
        } else if c == '\'' && ch(i + 2) == Some('\'') {
            i + 3
        } else {
            i += 1;
            continue;
        };
        blanked.push((byte(i), byte(end)));
        i = end.max(i + 1);
    }
    let mut out = String::with_capacity(code.len());
    let mut copied = 0;
    for (from, to) in blanked {
        out.push_str(&code[copied..from]);
        out.extend(
            code[from..to].chars().flat_map(|c| {
                std::iter::repeat_n(if c == '\n' { '\n' } else { ' ' }, c.len_utf8())
            }),
        );
        copied = to;
    }
    out.push_str(&code[copied..]);
    out
}

/// The test functions in one source file, by name, with the platform
/// marker the documents must give them: `Unix` for `#[cfg(unix)]`,
/// `Windows` for `#[cfg(windows)]`, `Linux CI` for a Linux-only test, and
/// an empty marker for a test that runs everywhere.
///
/// A function counts as a test only when its own attributes (others may
/// stand between) include `#[test]` or `#[tokio::test]`. Comments and
/// string literals are blanked first, so a name inside them never counts.
fn test_functions(text: &str) -> std::collections::BTreeMap<String, &'static str> {
    let code = strip(text);
    let pattern = regex::Regex::new(r"\bfn\s+([a-z_][a-z0-9_]*)").expect("pattern compiles");
    let mut found = std::collections::BTreeMap::new();
    for capture in pattern.captures_iter(&code) {
        let Some(whole) = capture.get(0) else {
            continue;
        };
        let mut head = code[..whole.start()].trim_end();
        for keyword in ["async", "unsafe", "const", "pub(crate)", "pub"] {
            head = head.strip_suffix(keyword).unwrap_or(head).trim_end();
        }
        let mut attributes = Vec::new();
        while head.ends_with(']') {
            let mut depth = 0usize;
            let Some(open) = head.char_indices().rev().find_map(|(at, c)| {
                match c {
                    ']' => depth += 1,
                    '[' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(at);
                        }
                    }
                    _ => {}
                }
                None
            }) else {
                break;
            };
            let Some(hash) = head[..open].strip_suffix('#') else {
                break;
            };
            // The attribute's text from the original, where strings are kept.
            attributes.push(flat(&text[open..head.len()]));
            head = hash.trim_end();
        }
        let is_test = attributes
            .iter()
            .any(|attribute| attribute == "[test]" || attribute.starts_with("[tokio::test"));
        if !is_test {
            continue;
        }
        let marker = if attributes.iter().any(|a| a == "[cfg(unix)]") {
            "Unix"
        } else if attributes.iter().any(|a| a == "[cfg(windows)]") {
            "Windows"
        } else if attributes.iter().any(|a| a.contains("target_os=\"linux\"")) {
            "Linux CI"
        } else {
            ""
        };
        found.insert(capture[1].to_owned(), marker);
    }
    found
}

/// `text` without white space.
fn flat(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

/// The test functions of every source file in the workspace.
fn defined_tests() -> std::collections::BTreeMap<String, &'static str> {
    let mut files = Vec::new();
    rust_files(&repo_root().join("crates"), &mut files);
    let mut tests = std::collections::BTreeMap::new();
    for file in files {
        let text = std::fs::read_to_string(&file).expect("read source");
        tests.extend(test_functions(&text));
    }
    tests
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
    let defined = defined_tests();
    let missing: Vec<&String> = named
        .iter()
        .filter(|name| !defined.contains_key(*name))
        .collect();
    assert!(
        missing.is_empty(),
        "docs/SECURITY_TESTS.md names tests that do not exist as #[test] or #[tokio::test] functions: {missing:?}"
    );

    let tests = column(&document, "test");
    let wheres = column(&document, "where");
    assert_eq!(tests.len(), wheres.len(), "every row has a `where` cell");
    let mut misplaced = Vec::new();
    let mut unmarked = Vec::new();
    for (test_cell, where_cell) in tests.iter().zip(&wheres) {
        let mut in_row = std::collections::BTreeMap::new();
        for file in backticked(where_cell) {
            if let Ok(text) = std::fs::read_to_string(repo_root().join(&file)) {
                in_row.extend(test_functions(&text));
            }
        }
        for name in backticked(test_cell) {
            let Some(marker) = in_row.get(&name) else {
                misplaced.push(name);
                continue;
            };
            let marked = |label: &str| test_cell.contains(&format!("`{name}` ({label})"));
            let right = if marker.is_empty() {
                !["Unix", "Windows", "Linux CI"]
                    .iter()
                    .any(|label| marked(label))
            } else {
                marked(marker)
            };
            if !right {
                unmarked.push(format!("{name}: {marker:?}"));
            }
        }
    }
    assert!(
        misplaced.is_empty(),
        "docs/SECURITY_TESTS.md names tests that are not test functions in the files of their `where` column: {misplaced:?}"
    );
    assert!(
        unmarked.is_empty(),
        "docs/SECURITY_TESTS.md must follow each platform-only test with its marker, such as `name` (Unix), and no other test with one: {unmarked:?}"
    );
}

#[test]
fn a_name_in_a_comment_or_a_string_or_on_a_helper_is_not_a_test() {
    let source = r##"
// fn commented_out_test() {}
/* #[test]
fn in_a_block_comment() {} */
const S: &str = "#[test] fn in_a_string() {}";
const R: &str = r#"#[test] fn in_a_raw_string() {}"#;
fn a_helper() {}
#[test]
fn a_real_test() {}
#[cfg(unix)]
#[tokio::test]
async fn a_unix_test() {}
#[cfg(windows)]
#[test]
#[ignore = "slow"]
fn a_windows_test() {}
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
pub async fn a_linux_test() {}
"##;
    let tests = test_functions(source);
    assert_eq!(
        tests.into_iter().collect::<Vec<_>>(),
        vec![
            ("a_linux_test".to_owned(), "Linux CI"),
            ("a_real_test".to_owned(), ""),
            ("a_unix_test".to_owned(), "Unix"),
            ("a_windows_test".to_owned(), "Windows"),
        ]
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

/// The guide's `default` cell for each key its key table lists.
fn guide_defaults(document: &str) -> std::collections::BTreeMap<String, String> {
    let mut defaults = std::collections::BTreeMap::new();
    for (key_cell, default_cell) in column(document, "key")
        .into_iter()
        .zip(column(document, "default"))
    {
        let mut table = String::new();
        for span in backticked(&key_cell) {
            let key = match span
                .strip_prefix('[')
                .and_then(|rest| rest.split_once("] "))
            {
                Some((name, key)) => {
                    table = name.to_owned();
                    format!("[{table}] {key}")
                }
                None if table.is_empty() => span,
                None => format!("[{table}] {span}"),
            };
            defaults.insert(key, default_cell.clone());
        }
    }
    defaults
}

/// Each key's default as the guide must spell it: a scalar as its TOML
/// literal in backticks, an empty list as `none` or `any`. Keys a default
/// leaves unset are not in the serialized value and are left to the reader.
fn spelled_defaults(
    value: &toml::Value,
    table: &str,
    into: &mut std::collections::BTreeMap<String, Vec<String>>,
) {
    let Some(entries) = value.as_table() else {
        return;
    };
    for (key, child) in entries {
        let name = if table.is_empty() {
            key.clone()
        } else {
            format!("[{table}] {key}")
        };
        let spellings = match child {
            toml::Value::Table(_) => {
                let nested = if table.is_empty() {
                    key.clone()
                } else {
                    format!("{table}.{key}")
                };
                spelled_defaults(child, &nested, into);
                continue;
            }
            toml::Value::Array(items) if items.is_empty() => {
                vec!["none".to_owned(), "any".to_owned()]
            }
            toml::Value::Array(_) => continue,
            scalar => vec![format!("`{scalar}`")],
        };
        into.insert(name, spellings);
    }
}

#[test]
fn the_admin_guide_states_each_policy_default_the_schema_applies() {
    // The smallest policies the schema accepts: every other key takes the
    // default the code applies, so nothing here restates a default by hand.
    let minimal = "version = 1\n[network]\nallowed_hosts = [\"h.example:443\"]\n";
    let container = format!(
        "{minimal}[shell]\nexecutor = \"container\"\n[shell.container]\nimage = \"build@sha256:{}\"\n",
        "0".repeat(64)
    );
    let mut expected = std::collections::BTreeMap::new();
    for text in [minimal.to_owned(), container] {
        let policy: Policy = toml::from_str(&text).expect("a minimal policy is valid");
        let value = toml::Value::try_from(&policy).expect("a policy serializes");
        spelled_defaults(&value, "", &mut expected);
    }
    for given in [
        "version",
        "[network] allowed_hosts",
        "[shell] executor",
        "[shell.container] image",
    ] {
        expected.remove(given);
    }
    assert!(
        expected.len() >= 20,
        "only {} defaults were derived from the schema",
        expected.len()
    );
    let listed = guide_defaults(&read_doc("docs/ADMIN_GUIDE.md"));
    let wrong: Vec<String> = expected
        .iter()
        .filter_map(|(key, spellings)| {
            let cell = listed.get(key).map_or("(no row)", String::as_str);
            let words: Vec<&str> = cell.split([',', ' ']).collect();
            let stated = spellings.iter().any(|spelling| {
                cell.contains(spelling.as_str()) || words.contains(&spelling.as_str())
            });
            (!stated)
                .then(|| format!("{key}: the guide says {cell}, the schema applies {spellings:?}"))
        })
        .collect();
    assert!(
        wrong.is_empty(),
        "docs/ADMIN_GUIDE.md states defaults the schema does not apply:\n{}",
        wrong.join("\n")
    );
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
