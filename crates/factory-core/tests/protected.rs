//! Every protected path in the repository's own `factory.toml` holds a pull
//! request, and a dependency addition does too.

use std::path::Path;

use anvil_types::FactoryConfig;
use factory_core::ProtectedPaths;

/// The committed `factory.toml` at the repository root.
fn config() -> FactoryConfig {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../factory.toml");
    let text = std::fs::read_to_string(&path).expect("factory.toml exists");
    let config: FactoryConfig = toml::from_str(&text).expect("factory.toml parses");
    config.validate().expect("factory.toml validates");
    config
}

/// A path that a glob from the list should match.
fn representative(glob: &str) -> String {
    if let Some(prefix) = glob.strip_suffix("/**") {
        return format!("{prefix}/some/nested/file.rs");
    }
    if glob == "**/permissions*.toml" {
        return ".anvil/permissions.toml".to_owned();
    }
    if glob.contains('*') {
        panic!("add a representative path for `{glob}`");
    }
    glob.to_owned()
}

#[test]
fn every_protected_path_in_factory_toml_holds_a_pull_request() {
    let config = config();
    assert!(
        !config.protected.paths.is_empty(),
        "the protected list lives in factory.toml, not in code"
    );
    let protected = ProtectedPaths::from_config(&config.protected).expect("globs compile");
    for glob in &config.protected.paths {
        let path = representative(glob);
        let held = protected.held(std::slice::from_ref(&path));
        assert_eq!(held.len(), 1, "`{path}` should be held by `{glob}`");
        assert_eq!(held[0].rule, *glob, "`{path}` matched another rule first");
    }
    let expected = [
        "crates/factory-core/**",
        "crates/factory-review/**",
        "crates/eval/**",
        "deploy/**",
        ".github/**",
        "CLAUDE.md",
        "justfile",
        "factory.toml",
        "tickets/**",
        "**/permissions*.toml",
    ];
    for glob in expected {
        assert!(
            config.protected.paths.iter().any(|p| p == glob),
            "factory.toml lost the protected entry `{glob}`"
        );
    }
    assert!(config.protected.cargo_dependencies);
}

#[test]
fn ordinary_code_is_not_held() {
    let protected = ProtectedPaths::from_config(&config().protected).expect("globs compile");
    let free = [
        "crates/anvil-core/src/tools/grep.rs",
        "crates/anvil-cli/src/main.rs",
        "skills/hello/SKILL.md",
        "README.md",
        "crates/anvil-types/src/config.rs",
        "Cargo.lock",
    ];
    let held = protected.held(&free.iter().map(|p| (*p).to_owned()).collect::<Vec<_>>());
    assert!(held.is_empty(), "{held:?}");
}

#[test]
fn a_new_dependency_holds_and_a_version_bump_does_not() {
    let protected = ProtectedPaths::from_config(&config().protected).expect("globs compile");
    let base = "[package]\nname = \"anvil-core\"\n\n[dependencies]\nserde = \"1\"\n";
    let added =
        "[package]\nname = \"anvil-core\"\n\n[dependencies]\nserde = \"1\"\nsha2 = \"0.10\"\n";
    let bumped = "[package]\nname = \"anvil-core\"\n\n[dependencies]\nserde = \"1.0.219\"\n";
    let held = protected.dependency_additions("crates/anvil-core/Cargo.toml", base, added);
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].path, "crates/anvil-core/Cargo.toml");
    assert_eq!(held[0].rule, "new dependency `sha2` in [dependencies]");
    assert!(
        protected
            .dependency_additions("crates/anvil-core/Cargo.toml", base, bumped)
            .is_empty()
    );
    assert!(
        protected
            .held(&["crates/anvil-core/Cargo.toml".to_owned()])
            .is_empty(),
        "the manifest itself is not a protected path; only additions are"
    );
}
