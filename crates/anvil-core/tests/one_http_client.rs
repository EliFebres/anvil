//! Every outbound HTTP request goes through `anvil_core::net`, whose
//! client checks each request against the egress rules. `clippy.toml`
//! forbids building a reqwest client elsewhere; these tests make sure no
//! file, test code included, switches that lint off or builds or holds a
//! client some other way, and that no other crate depends on reqwest.

use std::path::{Path, PathBuf};

/// The one file allowed to build an HTTP client.
const ALLOWED: &str = "crates/anvil-core/src/net.rs";

/// Text that builds a client or turns the guard off.
const FORBIDDEN: [&str; 9] = [
    "reqwest::Client::new",
    "reqwest::Client::builder",
    "Client::new(",
    "Client::builder(",
    "Client::default(",
    "ClientBuilder::new",
    "reqwest::get(",
    "reqwest::blocking",
    "disallowed_methods",
];

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
