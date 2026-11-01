//! The files under deploy/nuc/ agree with factory.toml.

use std::path::{Path, PathBuf};

use anvil_types::config::{SETUP_PROVIDERS, merge_tables};
use anvil_types::{AnvilConfig, FactoryConfig, Unmatched};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(relative: &str) -> String {
    let path = repo_root().join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

#[test]
fn the_factory_profile_matches_factory_toml_and_locks_permissions() {
    let factory: FactoryConfig =
        toml::from_str(&read("factory.toml")).expect("factory.toml parses");
    let mut merged: toml::Table = toml::from_str(SETUP_PROVIDERS).expect("providers parse");
    let overlay: toml::Table =
        toml::from_str(&read("deploy/nuc/anvil-factory.toml")).expect("profile parses");
    merge_tables(&mut merged, overlay);
    let anvil: AnvilConfig = merged.try_into().expect("the profile is a config overlay");
    anvil
        .validate()
        .expect("the merged configuration validates");

    assert_eq!(
        anvil.roles.coder, factory.roles.coder,
        "the profile's coder must be factory.toml's coder"
    );
    let reviewer = anvil
        .roles
        .reviewer
        .as_deref()
        .expect("the profile sets anvil's reviewer");
    assert_ne!(
        reviewer, anvil.roles.coder,
        "anvil's own reviewer is a different provider"
    );
    assert_ne!(
        anvil.providers[reviewer].base_url, anvil.providers[&anvil.roles.coder].base_url,
        "and sits on a different base URL"
    );
    let rules = anvil.permissions.expect("the profile sets permissions");
    assert_eq!(rules.unmatched, Some(Unmatched::Deny));
    for tool in [
        "shell:cargo *",
        "shell:just *",
        "shell:git *",
        "write_file",
        "edit_file",
    ] {
        assert!(
            rules.allow.iter().any(|r| r == tool),
            "{tool} missing from allow"
        );
    }
    for denied in ["shell:git push*", "shell:git reset --hard*", "shell:*&&*"] {
        assert!(
            rules.deny.iter().any(|r| r == denied),
            "{denied} missing from deny"
        );
    }
}

#[test]
fn the_container_files_are_consistent() {
    let containerfile = read("deploy/nuc/Containerfile");
    assert!(containerfile.contains("FROM docker.io/library/rust:1-bookworm"));
    for tool in [
        "socat",
        "python3-pytest",
        "python3-pandas",
        "ripgrep",
        "rustfmt clippy",
    ] {
        assert!(
            containerfile.contains(tool),
            "{tool} missing from the Containerfile"
        );
    }
    assert!(containerfile.contains("ENTRYPOINT [\"/entrypoint.sh\"]"));
    let entrypoint = read("deploy/nuc/entrypoint.sh");
    assert!(entrypoint.contains("/run/factory/proxy.sock"));
    assert!(entrypoint.contains("TCP-LISTEN:3128"));
    assert!(entrypoint.trim_end().ends_with("exec \"$@\""));
    let bootstrap = read("deploy/nuc/bootstrap.sh");
    assert!(bootstrap.contains("podman build"));
    assert!(bootstrap.contains("factory install-service"));
    let unit = read("deploy/nuc/factory.service");
    assert!(unit.contains("ExecStart=%h/.local/bin/factory run"));
    assert!(
        !unit.contains("EnvironmentFile"),
        "secrets never come from the unit"
    );
}
