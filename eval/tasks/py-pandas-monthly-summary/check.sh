#!/usr/bin/env bash
# summary.py writes monthly.csv matching the expected totals.
set -eu
PY="${EVAL_PYTHON:-python3}"

"$PY" summary.py
"$PY" - <<'PYEOF'
import os
import pandas as pd

out = pd.read_csv("monthly.csv")
expected = pd.read_csv(os.path.join(os.environ["EVAL_TASK_DIR"], "expected", "monthly.csv"))
assert list(out.columns) == ["month", "total"], list(out.columns)
assert list(out["month"]) == list(expected["month"]), out["month"].tolist()
assert ((out["total"] - expected["total"]).abs() < 1e-9).all(), out["total"].tolist()
PYEOF
