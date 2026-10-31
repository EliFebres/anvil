#!/usr/bin/env bash
# Copy every file under data/ into out/, keeping the file names.
set -e
mkdir -p out
for f in data/*; do
  [ -f "$f" ] || continue
  cp "$f" "out/$(basename "$f")"
done
echo "backed up $(find out -type f | wc -l) file(s)"
