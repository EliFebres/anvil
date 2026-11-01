#!/usr/bin/env bash
# --lines counts lines; the default still counts words.
set -eu
PY="${EVAL_PYTHON:-python3}"

[ "$("$PY" count.py --lines sample.txt)" = "3" ]
[ "$("$PY" count.py sample.txt)" = "12" ]
