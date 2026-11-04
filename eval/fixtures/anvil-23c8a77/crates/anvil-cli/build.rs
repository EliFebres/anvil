//! Records the build target so `anvil update` can pick the right release asset.

fn main() {
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_owned());
    println!("cargo:rustc-env=ANVIL_TARGET={target}");
    // Set by release.yml; a source build has neither.
    println!("cargo:rerun-if-env-changed=ANVIL_BUILD_SHA");
    println!("cargo:rerun-if-env-changed=ANVIL_RELEASE_REPO");
}
