# tickets

The ticket queue, kept as files so it has git history. Open tickets live at `tickets/open/<ulid>.toml`; the file stem is the ticket `id` and its `status` is `open`.

A ticket records what went wrong or was missing (`symptom`), the interface wanted (`spec`), an optional synthetic reproduction (`repro`), and the checks the factory must make pass (`acceptance`). `evidence` holds telemetry ids only, never content. The full schema is `forge_types::Ticket`; print it as JSON Schema with `cargo run -p forge-types -- --schema`.

This directory is the data boundary between forge and the factory. The validator refuses any ticket whose `repro` or `spec` contains a path outside this repository or more than 40 lines of code, and `cargo test -p forge-types` runs that validator over every file here.

Lifecycle: `open` → `claimed` → `in_review` → `merged` or `rejected`. Only `open/` exists for now.
