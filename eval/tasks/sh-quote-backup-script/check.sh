#!/usr/bin/env bash
# The script parses and copies a file whose name has spaces.
set -eu

bash -n scripts/backup.sh
printf 'hello\n' > "data/my file.txt"
bash scripts/backup.sh > summary.txt
test -f "out/my file.txt" || { echo "out/my file.txt is missing"; exit 1; }
test -f out/plain.txt || { echo "out/plain.txt is missing"; exit 1; }
diff --strip-trailing-cr "data/my file.txt" "out/my file.txt"
if [ -e out/my ] || [ -e out/file.txt ]; then echo "the name was split"; exit 1; fi
grep -q '2 file' summary.txt || { echo "summary line is wrong: $(cat summary.txt)"; exit 1; }
