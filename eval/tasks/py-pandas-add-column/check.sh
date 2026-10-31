#!/usr/bin/env bash
# enrich.py writes sales_total.csv with a correct total column.
set -eu
PY="${EVAL_PYTHON:-python3}"

"$PY" enrich.py
"$PY" - <<'PYEOF'
import pandas as pd

src = pd.read_csv("sales.csv")
out = pd.read_csv("sales_total.csv")
assert list(out.columns[:3]) == ["item", "qty", "unit_price"], list(out.columns)
assert "total" in out.columns, list(out.columns)
assert len(out) == len(src), (len(out), len(src))
assert list(out["item"]) == list(src["item"]), "row order changed"
expected = src["qty"] * src["unit_price"]
assert ((out["total"] - expected).abs() < 1e-9).all(), out["total"].tolist()
PYEOF
