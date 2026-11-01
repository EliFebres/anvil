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
# git grep ships with git, so no extra tool is needed on any machine or runner; it
# exits non-zero when nothing matched, which is the only passing outcome.
[unix]
no-old-name:
    ! git grep -i -I -E --untracked 'f[o]rge' -- . ':!crates/anvil-cli/src/legacy.rs' ':!crates/anvil-cli/tests/legacy.rs'

[windows]
no-old-name:
    git grep -i -I -E --untracked 'f[o]rge' -- . ':!crates/anvil-cli/src/legacy.rs' ':!crates/anvil-cli/tests/legacy.rs' ; if ($LASTEXITCODE -ne 1) { exit 1 }; exit 0

# Build the factory's worker image (needs podman; the NUC does this in bootstrap.sh).
image:
    podman build -t localhost/anvil-factory:latest -f deploy/nuc/Containerfile deploy/nuc

# Print the Ticket JSON Schema.
schema:
    cargo run -q -p anvil-types -- --schema

# Prove every eval task both ways: the check fails on the raw fixture and
# passes on fixture + solution. Compiles the Rust fixtures, so it is not in `check`.
eval-tasks:
    cargo test -p eval --test tasks -- --ignored

# Score the coder role three times over and commit the result under eval/results/.
eval-baseline:
    cargo run -q -p eval -- baseline
