#!/usr/bin/env bash
# thiserror is used, the tests are untouched and pass.
set -eu

grep -q thiserror Cargo.toml || { echo "thiserror is not a dependency"; exit 1; }
grep -rq 'derive(.*Error' src/ || { echo "no derive(Error) in src/"; exit 1; }
grep -rq 'KvError' src/ || { echo "no KvError in src/"; exit 1; }
diff --strip-trailing-cr -q tests/kv.rs "$EVAL_TASK_DIR/expected/kv.rs"
cargo test -q
