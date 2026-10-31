# Bug

`truncate_words("a b c", 3)` panics with an index out of bounds. A text with exactly `max_words` words should come back unchanged, with no `...`.

First add a test under `tests/` that reproduces this, then fix `src/lib.rs` so the whole suite passes.
