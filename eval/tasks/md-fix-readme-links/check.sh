#!/usr/bin/env bash
# Every relative link target in README.md exists; there are still five links.
set -eu
PY="${EVAL_PYTHON:-python3}"

"$PY" - <<'PYEOF'
import os
import re
import sys

text = open("README.md", encoding="utf-8").read()
targets = re.findall(r"\]\(([^)]+)\)", text)
if len(targets) != 5:
    print(f"expected 5 links, found {len(targets)}: {targets}")
    sys.exit(1)
missing = [t for t in targets if not t.startswith("http") and not os.path.exists(t.split("#")[0])]
if missing:
    print(f"links to missing files: {missing}")
    sys.exit(1)
for phrase in ["setup guide", "configuration reference", "CLI reference", "questions page", "how to contribute"]:
    if phrase not in text:
        print(f"link text changed: {phrase!r} is gone")
        sys.exit(1)
PYEOF
