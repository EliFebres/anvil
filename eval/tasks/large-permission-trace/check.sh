#!/usr/bin/env bash
# A trace! in the non-test part of permissions.rs names the tool, the target
# and the decision; anvil-core still passes its permission tests and clippy.
set -eu

"$EVAL_PYTHON" - <<'PY'
import re, sys
src = open("crates/anvil-core/src/tools/permissions.rs", encoding="utf-8").read()
code = src.split("#[cfg(test)]")[0]
calls = re.findall(r"trace!\s*\((.*?)\);", code, re.S)
ok = [c for c in calls if "tool" in c and "target" in c and "decision" in c.lower()]
if not ok:
    sys.exit(f"no trace! naming tool, target and decision; found {len(calls)} trace! call(s)")
PY
cargo test -q -p anvil-core --lib permissions
cargo clippy -q -p anvil-core --lib -- -D warnings
