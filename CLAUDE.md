# CLAUDE.md

This is a Cargo workspace (Rust 2024 edition) containing `anvil`, a CLI coding agent, and `factory`, an unattended improvement service. Read the conventions first; they are not optional.

## Conventions

- Readability first: an intermediate Rust programmer must be able to follow any function without reading another file. YAGNI, DRY, orthogonality: a change in one crate must not require changes in another unless the shared type in `anvil-types` changed.
- Every public item gets a doc comment in Google-docstring shape: one-line summary, blank line, `# Arguments`, `# Returns`, `# Errors`, `# Panics` where they apply. Private items get a one-line summary.
- Validate at boundaries with assertions and descriptive errors: `thiserror` error enums per crate, `anyhow` only in the two binaries. Invariants inside a function use `debug_assert!` with a message.
- Minimal inline comments. A comment explains why, never what.
- Async runtime: tokio. HTTP: reqwest with rustls (no OpenSSL, so Windows builds are painless). Serialization: serde + serde_json + toml. CLI: clap derive. Logging: tracing with a JSON file sink and a human console sink.
- No `unwrap()` outside tests. No `unsafe`.
- Tests live next to the code (`#[cfg(test)]`) plus `tests/` for integration. Use `insta` for snapshot tests of prompts and tool schemas, because those drift silently.
- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test` must pass before any commit. Add a `justfile` with `just check` that runs all three.
- The agent was once called by another name. That name may appear only in `crates/anvil-cli/src/legacy.rs` (the migration from the old `~/.anvil`-equivalent directory and variables) and its test `crates/anvil-cli/tests/legacy.rs`; `just no-old-name`, part of `just check` and CI, fails on it anywhere else.
- Every workflow declares a top-level `permissions:` block with the minimum it needs. `release.yml` gets `contents: write`; everything else gets `contents: read`. `cargo test -p anvil-types` checks every file in `.github/workflows/`.

Workspace lints enforce part of this mechanically: `unsafe_code = "forbid"`, `missing_docs = "warn"`, and `clippy::unwrap_used = "warn"` (allowed in tests via `clippy.toml`). With `-D warnings` these are hard failures.

## Crates

**`anvil-types`** holds shared types only: the Ticket schema, provider and roles config, telemetry references, and the skill manifest, plus the validators for each. Its only logic is validation; the ticket validator is the data boundary between anvil and the factory (no project paths, no more than 40 lines of code in `repro` or `spec`). It also owns the loading both binaries share, since it is the only crate both may depend on: the merged anvil config (`config::load`), the `.env` files (`dotenv`) and the `factory.toml` schema (`factory_config`), plus the git arguments both pass to reach GitHub with a token (`github::git_auth_args`). It ships a tiny `anvil-types --schema` binary that prints the Ticket JSON Schema. It must never depend on another workspace crate, on tokio, or on reqwest. Every other crate may depend on it.

**`anvil-core`** is the agent loop, tool registry, provider adapters (OpenAI and Anthropic protocols), and context management. It depends on `anvil-types` only. It must never depend on `anvil-compat`, `anvil-memory`, `anvil-telemetry`, any `factory-*` crate, or either CLI; those plug into it from the outside through the CLI.

**`anvil-compat`** is Claude Code compatibility: SKILL.md, CLAUDE.md, `.claude/commands`, `.claude/agents`, hooks, and the MCP client. It depends on `anvil-types` and `anvil-core` (to register tools and hooks). It must never depend on `factory-*`, `anvil-memory`, or `anvil-telemetry`.

**`anvil-memory`** is cross-project memory and coding-style rules. It depends on `anvil-types` only. It must never depend on `anvil-core`, `anvil-compat`, or any `factory-*` crate.

**`anvil-telemetry`** records usage, classifies failures, and drafts tickets. It consumes telemetry event values and never calls into the agent loop, so it depends on `anvil-types` only. It must never depend on `anvil-core` or any `factory-*` crate.

**`anvil-cli`** is the `anvil` binary: clap argument parsing and wiring only. It is the one place where the `anvil-*` libraries meet. It must never depend on any `factory-*` crate. It is one of the two crates allowed to use `anyhow`.

The full-screen UI lives in `crates/anvil-cli/src/tui/` (ratatui + crossterm). Everything under `tui/` is pure except `runner.rs`, which owns the terminal: `App` consumes `UiMsg`s and draws into a frame, so tests drive it with messages and a `TestBackend`. Modules under `tui/` may import `anvil_core` and `anvil_types` types, tokio channels and the UI crates, and nothing else from this crate: never `Wiring`, `anvil_memory`, `anvil_compat` or telemetry. `frontend.rs` gathers what the UI shows into `SessionInfo` and bridges prompts to it. Every color comes from `tui/theme.rs`. Scripts and CI use `--plain` (or `--yes`, or no TTY) for the line renderer, which must stay byte-identical; `ANVIL_NO_ANIMATION` skips the startup logo animation.

**`factory-core`** is the ticket queue, worker loop, git operations, and merge policy. It drives `anvil` as a subprocess, not as a library, so it depends on `anvil-types` only. It must never depend on any `anvil-*` library.

**`factory-review`** is multi-model review. It owns its own model calls and depends on `anvil-types` only. It must never depend on `anvil-core` or `factory-core`.

**`factory-cli`** is the `factory` binary: argument parsing and wiring only. It depends on `factory-core`, `factory-review`, and `eval`. It must never depend on any `anvil-*` library. It is the other crate allowed to use `anyhow`.

**`eval`** is the eval harness that scores a model or a build against a fixed task set. Both binaries use it. It depends on `anvil-types` only and must never depend on `anvil-core` or `factory-core`.

## Repository layout

- `skills/`: Claude Code-format skills that ship with anvil (`skills/<name>/SKILL.md`).
- `tickets/open/<ulid>.toml`: the ticket queue, committed so it has history. `cargo test -p anvil-types` validates every file in it.
- Trunk is `main` and is protected. The factory works on `factory/<ticket-id>` branches and merges through pull requests with required checks (`.github/workflows/ci.yml`).
- Config: global `~/.anvil/config.toml`, project `.anvil/config.toml` (project overrides global). `anvil config init [--global]` writes a template.
