#!/usr/bin/env bash
# main() runs and prints 3.75; annotations and call sites are fixed; tests pass.
set -eu
PY="${EVAL_PYTHON:-python3}"

[ "$("$PY" -c 'import inventory; inventory.main()')" = "3.75" ]
"$PY" - <<'PYEOF'
import ast
import sys

tree = ast.parse(open("inventory.py", encoding="utf-8").read())
problems = []
for node in ast.walk(tree):
    if isinstance(node, ast.FunctionDef) and node.name == "total_value":
        returns = ast.unparse(node.returns) if node.returns else ""
        if returns != "float":
            problems.append(f"total_value returns {returns!r}, want 'float'")
    if isinstance(node, ast.Call) and isinstance(node.func, ast.Name):
        if node.func.id == "Item" and len(node.args) >= 2:
            count = node.args[1]
            if not (isinstance(count, ast.Constant) and isinstance(count.value, int)):
                problems.append(f"Item(...) count is not an int literal: {ast.unparse(count)}")
        if node.func.id == "restock" and len(node.args) >= 2:
            count = node.args[1]
            if not (isinstance(count, ast.Constant) and isinstance(count.value, int)):
                problems.append(f"restock(...) count is not an int literal: {ast.unparse(count)}")
            if isinstance(node.args[0], ast.Call):
                problems.append("restock receives find(...) directly; narrow the Optional first")
for problem in problems:
    print(problem)
sys.exit(1 if problems else 0)
PYEOF
"$PY" -m pytest -q
