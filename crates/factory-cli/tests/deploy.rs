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
    let budgets = [
        ("plan", &anvil.limits.plan, 400_000),
        ("build", &anvil.limits.build, 3_000_000),
        ("review", &anvil.limits.review, 300_000),
        ("verify", &anvil.limits.verify, 300_000),
    ];
    for (state, budget, tokens) in budgets {
        assert_eq!(budget.max_tokens, tokens, "[limits.{state}] max_tokens");
    }
    let states: u64 = budgets.iter().map(|(_, budget, _)| budget.max_tokens).sum();
    assert!(
        states <= anvil.limits.session_tokens,
        "the per-state budgets fit inside the session cap"
    );
    assert!(
        anvil
            .prompt
            .notes
            .iter()
            .any(|note| note.contains("no pipes")),
        "the profile tells the model its shell rules"
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
    for tool in [
        "socat",
        "python3-pytest",
        "python3-pandas",
        "ripgrep",
        "musl-tools",
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
    // The container's anvil is a static musl build, proven so with ldd; the
    // host build stays for anvil doctor and the eval harness.
    for musl in [
        "musl-tools",
        "MUSL_TARGET=\"x86_64-unknown-linux-musl\"",
        "rustup target add \"$MUSL_TARGET\"",
        "--target \"$MUSL_TARGET\" -p anvil-cli",
        "ldd",
        "ln -sf \"$REPO_DIR/target/release/anvil\" \"$BIN_DIR/anvil\"",
    ] {
        assert!(bootstrap.contains(musl), "{musl} missing from bootstrap.sh");
    }
    let factory: FactoryConfig =
        toml::from_str(&read("factory.toml")).expect("factory.toml parses");
    assert_eq!(
        factory.worker.anvil_bin, "target/x86_64-unknown-linux-musl/release/anvil",
        "factory.toml mounts the musl build bootstrap.sh and factory update make"
    );
    let unit = read("deploy/nuc/factory.service");
    assert!(unit.contains("ExecStart=%h/.local/bin/factory run"));
    assert!(
        !unit.contains("EnvironmentFile"),
        "secrets never come from the unit"
    );
}

/// The worker image's Rust is the pinned toolchain, read from
/// rust-toolchain.toml at build time: the container cannot download another,
/// and it must format exactly like the host and CI.
#[test]
fn the_worker_image_installs_the_pinned_toolchain() {
    let pinned: toml::Table = toml::from_str(&read("rust-toolchain.toml")).expect("parses");
    let toolchain = &pinned["toolchain"];
    let containerfile = read("deploy/nuc/Containerfile");
    let from = containerfile
        .lines()
        .find(|line| line.starts_with("FROM "))
        .expect("a FROM line");
    assert!(
        !from.contains("rust:"),
        "the toolchain comes from rust-toolchain.toml, not a rust base image: {from}"
    );
    let copy = containerfile
        .find("COPY rust-toolchain.toml ")
        .expect("the Containerfile copies rust-toolchain.toml");
    let install = containerfile
        .find("sh.rustup.rs")
        .expect("the Containerfile installs rustup");
    assert!(
        copy < install,
        "the pin is copied before the install reads it"
    );
    assert!(
        containerfile.contains("--default-toolchain \"$channel\""),
        "the installed toolchain is the pin's channel"
    );
    assert!(containerfile.contains("RUSTUP_AUTO_INSTALL=0"));

    let components: Vec<&str> = toolchain["components"]
        .as_array()
        .expect("components")
        .iter()
        .map(|c| c.as_str().expect("a component name"))
        .collect();
    assert!(
        containerfile.contains(&format!("--component {}", components.join(","))),
        "the image must install exactly the pinned components {components:?}"
    );
    for target in toolchain["targets"].as_array().expect("targets") {
        let target = target.as_str().expect("a target name");
        assert!(
            containerfile.contains(&format!("--target {target}")),
            "the image must add {target}, or rustup downloads it inside the offline container"
        );
    }
}

/// Every place that builds the image uses the repository root as the
/// context, which the Containerfile needs for rust-toolchain.toml, and the
/// ignore file keeps target/ out of it.
#[test]
fn every_image_build_uses_the_repository_root_as_context() {
    let ignore = read(".dockerignore");
    let lines: Vec<&str> = ignore
        .lines()
        .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
        .collect();
    assert_eq!(
        lines,
        ["*", "!rust-toolchain.toml", "!deploy/nuc/entrypoint.sh"]
    );
    let containerfile = read("deploy/nuc/Containerfile");
    assert!(containerfile.contains("COPY deploy/nuc/entrypoint.sh /entrypoint.sh"));
    for file in [
        "justfile",
        "deploy/nuc/bootstrap.sh",
        ".github/workflows/ci.yml",
    ] {
        let text = read(file);
        let builds: Vec<&str> = text
            .lines()
            .filter(|line| line.contains("build") && line.contains("deploy/nuc/Containerfile"))
            .collect();
        assert!(!builds.is_empty(), "{file} builds the image");
        for line in builds {
            assert!(
                line.trim_end()
                    .trim_end_matches(')')
                    .ends_with("-f deploy/nuc/Containerfile ."),
                "{file}: {line}"
            );
        }
    }
}
