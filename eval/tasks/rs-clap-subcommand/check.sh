#!/usr/bin/env bash
# A scripted session with remove produces the expected list; a bad number fails.
set -eu

rm -f notes.txt
cargo build -q
cargo run -q -- add "buy milk"
cargo run -q -- add "call mum"
cargo run -q -- add "pay rent"
cargo run -q -- remove 2
cargo run -q -- list > out.txt
diff --strip-trailing-cr out.txt "$EVAL_TASK_DIR/expected/list.txt"
if cargo run -q -- remove 9 2>/dev/null; then echo "remove 9 should fail"; exit 1; fi
cargo run -q -- list > again.txt
diff --strip-trailing-cr again.txt "$EVAL_TASK_DIR/expected/list.txt"
grep -q 'Remove' src/main.rs
