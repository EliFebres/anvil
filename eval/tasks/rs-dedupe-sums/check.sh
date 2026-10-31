#!/usr/bin/env bash
# Tests unchanged and passing; at most one accumulation left.
set -eu

diff --strip-trailing-cr -q tests/sums.rs "$EVAL_TASK_DIR/expected/sums.rs"
cargo test -q
count=$(grep -c 'total += ' src/lib.rs || true)
[ "$count" -le 1 ] || { echo "accumulation appears $count times, want at most 1"; exit 1; }
