#!/usr/bin/env bash
# A minutes test exists, the suite passes, and the new test catches the original bug.
set -eu
PY="${EVAL_PYTHON:-python3}"

grep -q '90m' tests/test_duration.py || { echo "no test mentions 90m"; exit 1; }
"$PY" -m pytest -q
scratch=$(mktemp -d)
cp -r . "$scratch"
cp "$EVAL_TASK_DIR/expected/duration_buggy.py" "$scratch/duration.py"
# Only a failing test counts: an import error or a missing interpreter also
# exits non-zero, and that must not pass as "the test caught the bug".
output=$(cd "$scratch" && "$PY" -m pytest -q 2>&1 || true)
if ! grep -Eq '[0-9]+ failed' <<<"$output"; then
  echo "the new test does not fail on the buggy implementation:"
  tail -n 20 <<<"$output"
  exit 1
fi
