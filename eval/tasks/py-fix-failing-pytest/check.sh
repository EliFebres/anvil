#!/usr/bin/env bash
# The tests are untouched and all pass.
set -eu
PY="${EVAL_PYTHON:-python3}"

diff --strip-trailing-cr -q tests/test_slugify.py "$EVAL_TASK_DIR/expected/test_slugify.py"
"$PY" -m pytest -q
