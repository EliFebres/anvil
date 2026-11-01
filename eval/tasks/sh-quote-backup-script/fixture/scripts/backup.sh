#!/usr/bin/env bash
# Copy every file under data/ into out/, keeping the file names.
set -e
mkdir -p out
for f in $(ls data); do
  cp data/$f out/$f
done
echo "backed up $(ls out | wc -l) file(s)"
