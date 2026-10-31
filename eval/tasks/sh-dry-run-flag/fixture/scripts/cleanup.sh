#!/usr/bin/env bash
# Delete every *.tmp file under build/.
set -eu
find build -type f -name '*.tmp' -print0 | while IFS= read -r -d '' file; do
  rm -f -- "$file"
done
