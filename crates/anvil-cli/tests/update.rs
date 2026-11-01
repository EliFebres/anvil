//! `anvil update` against a fake GitHub: the asset is verified and swapped in.

use std::path::Path;
use std::process::Command;

use serde_json::json;
use sha2::{Digest, Sha256};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, text).expect("write");
}

fn hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The asset name `anvil update` looks for on this platform.
fn asset_name() -> String {
    let target = std::env::var("TARGET").ok();
    let _ = target;
    let triple = if cfg!(windows) {
        "x86_64-pc-windows-msvc"
    } else if cfg!(target_os = "macos") {
        if cfg!(target_arch = "aarch64") {
            "aarch64-apple-darwin"
        } else {
            "x86_64-apple-darwin"
        }
    } else if cfg!(target_arch = "aarch64") {
        "aarch64-unknown-linux-gnu"
    } else {
        "x86_64-unknown-linux-gnu"
    };
    if cfg!(windows) {
        format!("anvil-{triple}.exe")
    } else {
        format!("anvil-{triple}")
    }
}

#[tokio::test]
async fn update_verifies_the_checksum_swaps_the_binary_and_prints_the_changes() {
    let server = MockServer::start().await;
    let asset = asset_name();
    let new_binary = b"#!/bin/sh\necho new anvil\n".to_vec();
    let notes = format!(
        "## Changes\n\n- Add a `grep` tool with a max-results flag\n- Fix the plan gate\n\n## SHA256\n\n{}  ./{asset}\n{}  ./other-asset\n",
        hex(&new_binary),
        "0".repeat(64)
    );
    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/releases"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {
                "tag_name": "2026-11-01-abc1234",
                "body": notes,
                "draft": false,
                "prerelease": false,
                "assets": [
                    { "name": asset, "browser_download_url": format!("{}/download/{asset}", server.uri()) },
                    { "name": "other-asset", "browser_download_url": format!("{}/download/other", server.uri()) }
                ]
            },
            {
                "tag_name": "2026-10-31-0000000",
                "body": "## Changes\n\n- older\n",
                "draft": false,
                "prerelease": false,
                "assets": []
            }
        ])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/download/{asset}")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(new_binary.clone()))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let project = dir.path().join("project");
    write(
        &project.join("factory.toml"),
        &anvil_types::FactoryConfig::template("owner/repo"),
    );
    let exe = dir.path().join("bin").join("anvil.exe");
    write(&exe, "old binary");

    let out = Command::new(env!("CARGO_BIN_EXE_anvil"))
        .args(["update", "--exe"])
        .arg(&exe)
        .current_dir(&project)
        .env("ANVIL_HOME", dir.path().join("home"))
        .env("ANVIL_GITHUB_API_URL", server.uri())
        .env("ANVIL_LOG", "off")
        .output()
        .expect("anvil runs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "stdout: {stdout}\nstderr: {stderr}");
    assert_eq!(std::fs::read(&exe).expect("read"), new_binary);
    assert!(stdout.contains("installed 2026-11-01-abc1234"), "{stdout}");
    assert!(
        stdout.contains("- Add a `grep` tool with a max-results flag"),
        "{stdout}"
    );
    assert!(
        !stdout.contains("- older"),
        "a source build reports the latest release only: {stdout}"
    );

    // A wrong checksum leaves the binary alone.
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/repos/owner/repo/releases"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
            "tag_name": "2026-11-02-def5678",
            "body": format!("## SHA256\n\n{}  {asset}\n", "f".repeat(64)),
            "assets": [{ "name": asset, "browser_download_url": format!("{}/download/{asset}", server.uri()) }]
        }])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/download/{asset}")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"tampered".to_vec()))
        .mount(&server)
        .await;
    let out = Command::new(env!("CARGO_BIN_EXE_anvil"))
        .args(["update", "--exe"])
        .arg(&exe)
        .current_dir(&project)
        .env("ANVIL_HOME", dir.path().join("home"))
        .env("ANVIL_GITHUB_API_URL", server.uri())
        .env("ANVIL_LOG", "off")
        .output()
        .expect("anvil runs");
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("checksum mismatch"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read(&exe).expect("read"),
        new_binary,
        "nothing was changed"
    );
}
