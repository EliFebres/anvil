#!/usr/bin/env bash
# The tests are untouched and pass.
set -eu

diff --strip-trailing-cr -q tests/range.rs "$EVAL_TASK_DIR/expected/range.rs"
cargo test -q
