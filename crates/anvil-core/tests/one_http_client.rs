//! Every outbound HTTP request goes through `anvil_core::net`, whose
//! client checks each request against the egress rules. `clippy.toml`
//! forbids naming the reqwest client types elsewhere; these tests make
//! sure no file, test code included, switches those lints off or builds or
//! holds a client some other way, that no other crate depends on reqwest
//! or on another HTTP client, and that no non-test code opens a socket.

use std::path::{Path, PathBuf};

/// The one file allowed to build an HTTP client.
const ALLOWED: &str = "crates/anvil-core/src/net.rs";

/// Text that builds a client or turns the guard off. `allow(warnings)`,
/// `clippy::all` and `clippy::style` would silence the disallowed lints
/// without naming them.
const FORBIDDEN: [&str; 17] = [
    "reqwest::Client::new",
    "reqwest::Client::builder",
    "Client::new(",
    "Client::builder(",
    "Client::default(",
    "ClientBuilder::new",
    "ClientBuilder::default",
    "Client as Default",
    "ClientBuilder as Default",
    "reqwest::get(",
    "reqwest::blocking",
    "disallowed_methods",
    "disallowed_types",
    "clippy::all",
    "clippy::style",
    "allow(warnings",
    "expect(warnings",
];

/// HTTP client crates no workspace crate may depend on directly. reqwest
/// is allowed in `anvil-core` only; `deny.toml` bans the rest, letting
/// hyper and hyper-util in only under reqwest and its own helpers.
const HTTP_CRATES: [&str; 9] = [
    "reqwest",
    "ureq",
    "hyper",
    "hyper-util",
    "isahc",
    "attohttpc",
    "surf",
    "curl",
    "curl-sys",
];

/// Socket types that would reach the network without the egress check.
const SOCKETS: [&str; 4] = ["TcpStream", "UdpSocket", "TcpSocket", "UnixStream"];

/// Clippy lint keys a manifest's `[lints.clippy]` must not turn off.
const GUARD_LINTS: [&str; 4] = ["all", "style", "disallowed_methods", "disallowed_types"];

fn rust_files(dir: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|name| name != "target") {
                rust_files(&path, found);
            }
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            found.push(path);
        }
    }
}

#[test]
fn only_the_net_module_builds_an_http_client() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let this_file = Path::new(file!()).to_string_lossy().replace('\\', "/");
    let mut files = Vec::new();
    rust_files(&root.join("crates"), &mut files);
    assert!(files.len() > 50, "found only {} Rust files", files.len());
    let mut offenders = Vec::new();
    for file in files {
        let relative = file
            .strip_prefix(&root)
            .unwrap_or(&file)
            .to_string_lossy()
            .replace('\\', "/");
        if relative == ALLOWED || relative.ends_with(&this_file) {
            continue;
        }
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        for pattern in FORBIDDEN {
            if text.contains(pattern) {
                offenders.push(format!("{relative}: {pattern}"));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "HTTP clients must be built in {ALLOWED} only:\n{}",
        offenders.join("\n")
    );
}

/// The text before the first inline test module (`#[cfg(test)]` then
/// `mod name {`); a `#[cfg(test)] mod name;` declaration does not end it.
fn non_test_code(text: &str) -> &str {
    const MARK: &str = "#[cfg(test)]";
    let mut from = 0;
    while let Some(found) = text[from..].find(MARK) {
        let at = from + found;
        let after = text[at + MARK.len()..].trim_start();
        let line = after.lines().next().unwrap_or_default();
        if line.starts_with("mod ") && line.contains('{') {
            return &text[..at];
        }
        from = at + MARK.len();
    }
    text
}

/// Every crate a manifest names directly: normal, dev and build tables,
/// per-target tables and `[workspace.dependencies]`, with `package = `
/// renames resolved.
fn direct_dependencies(manifest: &toml::Table) -> Vec<String> {
    fn names(table: &toml::Table, found: &mut Vec<String>) {
        for key in ["dependencies", "dev-dependencies", "build-dependencies"] {
            let Some(deps) = table.get(key).and_then(toml::Value::as_table) else {
                continue;
            };
            for (name, spec) in deps {
                let package = spec
                    .get("package")
                    .and_then(toml::Value::as_str)
                    .unwrap_or(name);
                found.push(package.to_owned());
            }
        }
    }
    let mut found = Vec::new();
    names(manifest, &mut found);
    if let Some(targets) = manifest.get("target").and_then(toml::Value::as_table) {
        for target in targets.values().filter_map(toml::Value::as_table) {
            names(target, &mut found);
        }
    }
    if let Some(workspace) = manifest.get("workspace").and_then(toml::Value::as_table) {
        names(workspace, &mut found);
    }
    found
}

/// The HTTP client crates one manifest depends on, as `label: crate`.
fn http_offenders(label: &str, manifest: &toml::Table, reqwest_allowed: bool) -> Vec<String> {
    direct_dependencies(manifest)
        .into_iter()
        .filter(|name| HTTP_CRATES.contains(&name.as_str()))
        .filter(|name| !(reqwest_allowed && name == "reqwest"))
        .map(|name| format!("{label}: {name}"))
        .collect()
}

/// Every workspace manifest with its label and whether it may name
/// reqwest: the root (its `[workspace.dependencies]` declares it for
/// anvil-core) and anvil-core.
fn manifests(root: &Path) -> Vec<(String, toml::Table, bool)> {
    let read = |path: &Path| -> toml::Table {
        let text = std::fs::read_to_string(path).expect("manifest reads");
        toml::from_str(&text).expect("manifest parses")
    };
    let mut found = vec![(
        "Cargo.toml".to_owned(),
        read(&root.join("Cargo.toml")),
        true,
    )];
    for entry in std::fs::read_dir(root.join("crates"))
        .expect("crates/")
        .filter_map(Result::ok)
    {
        let path = entry.path().join("Cargo.toml");
        if path.is_file() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let core = name == "anvil-core";
            found.push((format!("crates/{name}/Cargo.toml"), read(&path), core));
        }
    }
    found
}

#[test]
fn no_workspace_crate_depends_on_another_http_client() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let all = manifests(&root);
    assert!(all.len() > 5, "found only {} manifests", all.len());
    let offenders: Vec<String> = all
        .iter()
        .flat_map(|(label, manifest, core)| http_offenders(label, manifest, *core))
        .collect();
    assert!(
        offenders.is_empty(),
        "only anvil-core may depend on an HTTP client, and only on reqwest:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn the_dependency_scan_sees_renames_and_target_tables() {
    let manifest: toml::Table = toml::from_str(concat!(
        "[dependencies]\nweb = { package = \"ureq\", version = \"2\" }\n",
        "[target.'cfg(unix)'.dev-dependencies]\nhyper = \"1\"\n",
        "[build-dependencies]\nreqwest = \"0.13\"\n",
    ))
    .expect("parses");
    let mut all = http_offenders("x", &manifest, false);
    all.sort();
    assert_eq!(all, ["x: hyper", "x: reqwest", "x: ureq"]);
    let mut core = http_offenders("x", &manifest, true);
    core.sort();
    assert_eq!(core, ["x: hyper", "x: ureq"]);
}

#[test]
fn no_manifest_turns_the_client_lints_off() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut offenders = Vec::new();
    for (label, manifest, _) in manifests(&root) {
        let tables = [
            manifest.get("lints"),
            manifest.get("workspace").and_then(|w| w.get("lints")),
        ];
        for clippy in tables
            .into_iter()
            .flatten()
            .filter_map(|lints| lints.get("clippy"))
            .filter_map(toml::Value::as_table)
        {
            for key in GUARD_LINTS {
                let level = clippy
                    .get(key)
                    .and_then(|value| value.get("level").unwrap_or(value).as_str());
                if level.is_some_and(|level| level == "allow" || level == "expect") {
                    offenders.push(format!("{label}: clippy::{key}"));
                }
            }
        }
    }
    for name in ["config.toml", "config"] {
        let text = std::fs::read_to_string(root.join(".cargo").join(name)).unwrap_or_default();
        for key in GUARD_LINTS {
            if text.contains(&format!("clippy::{key}")) {
                offenders.push(format!(".cargo/{name}: clippy::{key}"));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "these switch off the lints that keep HTTP clients in net.rs:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn no_non_test_code_opens_a_socket_outside_net() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = Vec::new();
    rust_files(&root.join("crates"), &mut files);
    let mut offenders = Vec::new();
    for file in files {
        let relative = file
            .strip_prefix(&root)
            .unwrap_or(&file)
            .to_string_lossy()
            .replace('\\', "/");
        let is_test = relative.contains("/tests/")
            || relative.ends_with("/tests.rs")
            || relative.ends_with("_tests.rs");
        if relative == ALLOWED || is_test {
            continue;
        }
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        let code = non_test_code(&text);
        for socket in SOCKETS {
            if code.contains(socket) {
                offenders.push(format!("{relative}: {socket}"));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "network sockets belong in {ALLOWED} only:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn only_anvil_core_depends_on_reqwest() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut offenders = Vec::new();
    for entry in std::fs::read_dir(root.join("crates"))
        .expect("crates/")
        .filter_map(Result::ok)
    {
        let manifest = entry.path().join("Cargo.toml");
        let text = std::fs::read_to_string(&manifest).unwrap_or_default();
        if entry.file_name() != "anvil-core" && text.contains("reqwest") {
            offenders.push(manifest.display().to_string());
        }
    }
    assert!(
        offenders.is_empty(),
        "only anvil-core may depend on reqwest:
{}",
        offenders.join(
            "
"
        )
    );
}
