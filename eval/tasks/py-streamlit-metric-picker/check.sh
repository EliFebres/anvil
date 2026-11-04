#!/usr/bin/env bash
# The page parses, uses a sidebar selectbox and st.metric; metrics.py stays pure.
set -eu
PY="${EVAL_PYTHON:-python3}"

"$PY" -c "import ast, sys; ast.parse(open('app/pages/2_metrics.py', encoding='utf-8').read())"
grep -q 'selectbox(' app/pages/2_metrics.py || { echo "no selectbox"; exit 1; }
grep -q 'sidebar' app/pages/2_metrics.py || { echo "selectbox is not in the sidebar"; exit 1; }
grep -q 'st.metric(' app/pages/2_metrics.py || { echo "no st.metric"; exit 1; }
if grep -q 'streamlit' app/metrics.py; then echo "metrics.py imports streamlit"; exit 1; fi
"$PY" -m pytest -q
