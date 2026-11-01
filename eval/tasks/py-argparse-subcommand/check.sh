#!/usr/bin/env bash
# A scripted session with done produces the expected list; a bad number exits 1.
set -eu
PY="${EVAL_PYTHON:-python3}"

rm -f todo.json
"$PY" todo.py add "write tests"
"$PY" todo.py add "ship it"
"$PY" todo.py done 1
"$PY" todo.py list > out.txt
diff --strip-trailing-cr out.txt "$EVAL_TASK_DIR/expected/list.txt"
if "$PY" todo.py done 9 2>/dev/null; then echo "done 9 should fail"; exit 1; fi
"$PY" todo.py list > again.txt
diff --strip-trailing-cr again.txt "$EVAL_TASK_DIR/expected/list.txt"
