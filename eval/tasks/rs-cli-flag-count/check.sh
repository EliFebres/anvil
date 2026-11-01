#!/usr/bin/env bash
# --count prints the match count; plain output is unchanged.
set -eu

cargo build -q
[ "$(cargo run -q -- --count foo sample.txt)" = "2" ]
cargo run -q -- foo sample.txt > out.txt
diff --strip-trailing-cr out.txt "$EVAL_TASK_DIR/expected/plain.txt"
if grep -q '^\[dependencies\]' Cargo.toml && grep -A5 '^\[dependencies\]' Cargo.toml | grep -q '='; then
  echo "a dependency was added"; exit 1
fi
