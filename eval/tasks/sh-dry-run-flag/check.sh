#!/usr/bin/env bash
# --dry-run lists and keeps; a plain run deletes.
set -eu

bash -n scripts/cleanup.sh
printf 'a' > build/a.tmp
printf 'b' > build/b.tmp
bash scripts/cleanup.sh --dry-run > listed.txt
test -f build/a.tmp || { echo "dry run deleted a.tmp"; exit 1; }
test -f build/b.tmp || { echo "dry run deleted b.tmp"; exit 1; }
grep -q 'a.tmp' listed.txt || { echo "dry run did not list a.tmp"; exit 1; }
grep -q 'b.tmp' listed.txt || { echo "dry run did not list b.tmp"; exit 1; }
bash scripts/cleanup.sh > plain.txt
[ ! -e build/a.tmp ] || { echo "a.tmp survived the real run"; exit 1; }
[ ! -e build/b.tmp ] || { echo "b.tmp survived the real run"; exit 1; }
test -f build/keep.txt || { echo "keep.txt was deleted"; exit 1; }
[ ! -s plain.txt ] || { echo "the real run printed: $(cat plain.txt)"; exit 1; }
