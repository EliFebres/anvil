set windows-shell := ["powershell.exe", "-NoLogo", "-Command"]

# Run every check that must pass before a commit.
default: check

# Format the whole workspace in place.
fmt:
    cargo fmt --all

# fmt (check only), clippy with warnings denied, and all tests.
check:
    cargo fmt --all --check
    cargo clippy --all-targets -- -D warnings
    cargo test --all

# Print the Ticket JSON Schema.
schema:
    cargo run -q -p forge-types -- --schema
