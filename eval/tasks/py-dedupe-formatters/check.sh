#!/usr/bin/env bash
# Output unchanged; the cleaning expression appears once.
set -eu
PY="${EVAL_PYTHON:-python3}"

diff --strip-trailing-cr -q tests/test_report.py "$EVAL_TASK_DIR/expected/test_report.py"
"$PY" -m pytest -q
count=$(grep -c -F 'replace("\n", " ")' report.py || true)
[ "$count" = "1" ] || { echo "cleaning expression appears $count times, want 1"; exit 1; }
