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
# exits non-zero when nothing matched, which is the only passing outcome. Outside
# a repository it exits non-zero too, so `rev-parse` first makes that a failure.
[unix]
no-old-name:
    git rev-parse --git-dir > /dev/null
    ! git grep -i -I -E --untracked 'f[o]rge' -- . ':!crates/anvil-cli/src/legacy.rs' ':!crates/anvil-cli/tests/legacy.rs'

[windows]
no-old-name:
    git grep -i -I -E --untracked 'f[o]rge' -- . ':!crates/anvil-cli/src/legacy.rs' ':!crates/anvil-cli/tests/legacy.rs' ; if ($LASTEXITCODE -ne 1) { exit 1 }; exit 0
