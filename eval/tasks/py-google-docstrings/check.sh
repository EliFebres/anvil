#!/usr/bin/env bash
# Every public function has a Google docstring with Args and Returns; tests still pass.
set -eu
PY="${EVAL_PYTHON:-python3}"

"$PY" - <<'PYEOF'
import ast
import sys

tree = ast.parse(open("shapes.py", encoding="utf-8").read())
problems = []
for node in tree.body:
    if not isinstance(node, ast.FunctionDef) or node.name.startswith("_"):
        continue
    doc = ast.get_docstring(node) or ""
    if not doc.strip():
        problems.append(f"{node.name}: no docstring")
        continue
    if node.args.args and "Args:" not in doc:
        problems.append(f"{node.name}: no Args section")
    if "Returns:" not in doc:
        problems.append(f"{node.name}: no Returns section")
for problem in problems:
    print(problem)
sys.exit(1 if problems else 0)
PYEOF
"$PY" -m pytest -q
