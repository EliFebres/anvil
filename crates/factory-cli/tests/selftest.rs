//! `factory selftest --no-container`: the worker pipeline against a local
//! bare repository, with anvil and Gate A on the host. The podman run is
//! the CI `selftest` job's; this keeps the pipeline itself under `cargo test`.

use std::path::Path;
use std::process::Command;

use anvil_types::factory_config::DEFAULT_ANVIL_BIN;

#[test]
fn the_selftest_merges_its_ticket_on_the_host() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    if !root.join(DEFAULT_ANVIL_BIN).is_file() {
        eprintln!("{DEFAULT_ANVIL_BIN} is not built; skipping (the CI selftest job builds it)");
        return;
    }
    let out = Command::new(env!("CARGO_BIN_EXE_factory"))
        .args(["selftest", "--no-container"])
        .current_dir(&root)
        .output()
        .expect("factory runs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("selftest passed"), "{stdout}");
    assert!(!stdout.contains("FAIL"), "{stdout}");
}
