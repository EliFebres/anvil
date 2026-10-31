#!/usr/bin/env bash
# Delete every *.tmp file under build/; --dry-run only lists them.
set -eu
dry_run=0
if [ "${1:-}" = "--dry-run" ]; then
  dry_run=1
fi
find build -type f -name '*.tmp' -print0 | while IFS= read -r -d '' file; do
  if [ "$dry_run" = 1 ]; then
    echo "$file"
  else
    rm -f -- "$file"
  fi
done
