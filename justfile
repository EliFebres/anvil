set windows-shell := ["powershell.exe", "-NoLogo", "-Command"]

# Run every check that must pass before a commit.
default: check

# Format the whole workspace in place.
fmt:
    cargo fmt --all

# fmt (check only), clippy with warnings denied, all tests, no API key in a tracked file, no old name.
check: no-old-name
    cargo fmt --all --check
    cargo clippy --all-targets -- -D warnings
    cargo test --all
    cargo run -q -p anvil-cli -- scan-keys

# The program used to have another name. It may survive only in the legacy
# migration module and its test; `f[o]rge` keeps this recipe from matching itself.
# ripgrep exits 1 when nothing matched, which is the only passing outcome.
[unix]
no-old-name:
    rg -i 'f[o]rge' --hidden --glob '!.git' --glob '!target' --glob '!crates/anvil-cli/src/legacy.rs' --glob '!crates/anvil-cli/tests/legacy.rs' . ; test $? -eq 1

[windows]
no-old-name:
    rg -i 'f[o]rge' --hidden --glob '!.git' --glob '!target' --glob '!crates/anvil-cli/src/legacy.rs' --glob '!crates/anvil-cli/tests/legacy.rs' . ; if ($LASTEXITCODE -ne 1) { exit 1 }; exit 0

# Print the Ticket JSON Schema.
schema:
    cargo run -q -p anvil-types -- --schema
