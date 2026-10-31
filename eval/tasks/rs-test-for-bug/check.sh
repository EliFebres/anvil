#!/usr/bin/env bash
# A test mentions the exact-count case, the suite passes, and that test catches the original bug.
set -eu

grep -rq 'truncate_words' tests/ || { echo "no test calls truncate_words"; exit 1; }
cargo test -q
scratch=$(mktemp -d)
cp -r . "$scratch"
cp "$EVAL_TASK_DIR/expected/lib_buggy.rs" "$scratch/src/lib.rs"
# Only a failing test counts: a build error or a blocked test binary also
# exits non-zero, and that must not pass as "the test caught the bug".
output=$(cd "$scratch" && cargo test -q 2>&1 || true)
if ! grep -q 'test result: FAILED' <<<"$output"; then
  echo "the new test does not fail on the buggy implementation:"
  tail -n 20 <<<"$output"
  exit 1
fi
