#!/usr/bin/env bash
# Builds clean under -D warnings, functions carry # Arguments, tests pass.
set -eu

RUSTFLAGS="-D warnings" cargo build -q
count=$(grep -c '# Arguments' src/geometry.rs || true)
[ "$count" -ge 4 ] || { echo "only $count functions have an # Arguments section, want 4"; exit 1; }
grep -q '# Returns' src/geometry.rs || { echo "no # Returns section"; exit 1; }
cargo test -q
