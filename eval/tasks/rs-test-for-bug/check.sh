#!/usr/bin/env bash
# A test mentions the exact-count case, the suite passes, and that test catches the original bug.
set -eu

grep -rq 'truncate_words' tests/ || { echo "no test calls truncate_words"; exit 1; }
cargo test -q
scratch=$(mktemp -d)
cp -r . "$scratch"
cp "$EVAL_TASK_DIR/expected/lib_buggy.rs" "$scratch/src/lib.rs"
if (cd "$scratch" && cargo test -q >/dev/null 2>&1); then
  echo "the new test does not fail on the buggy implementation"
  exit 1
fi
