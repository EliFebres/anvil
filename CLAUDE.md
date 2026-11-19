# CLAUDE.md

This is a Cargo workspace (Rust 2024 edition) containing `anvil`, a CLI coding agent. Read the conventions first; they are not optional.

## Conventions

- Readability first: an intermediate Rust programmer must be able to follow any function without reading another file. YAGNI, DRY, orthogonality: a change in one crate must not require changes in another unless the shared type in `anvil-types` changed.
- Every public item gets a doc comment in Google-docstring shape: one-line summary, blank line, `# Arguments`, `# Returns`, `# Errors`, `# Panics` where they apply. Private items get a one-line summary.
- Validate at boundaries with assertions and descriptive errors: `thiserror` error enums per crate, `anyhow` only in the `anvil` binary (`anvil-cli`). Invariants inside a function use `debug_assert!` with a message.
- Minimal inline comments. A comment explains why, never what.
- Async runtime: tokio. HTTP: reqwest with rustls (no OpenSSL, so Windows builds are painless). Serialization: serde + serde_json + toml. CLI: clap derive. Logging: tracing with a JSON file sink and a human console sink.
- No `unwrap()` outside tests. No `unsafe`.
- Tests live next to the code (`#[cfg(test)]`) plus `tests/` for integration. Use `insta` for snapshot tests of prompts and tool schemas, because those drift silently.
- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test` must pass before any commit. Add a `justfile` with `just check` that runs all three.
- The agent was once called by another name. That name may not appear anywhere in the tree; `just no-old-name`, part of `just check` and CI, fails on it.
- Every workflow declares a top-level `permissions:` block with the minimum it needs. `release.yml` gets `contents: write`, `id-token: write` and `attestations: write` (for build provenance); everything else gets `contents: read`. `cargo test -p anvil-types` checks every file in `.github/workflows/`.
- `SECURITY.md` is written for someone deciding whether anvil may run on their machines. A change that adds a network destination, a file anvil stores data in, a way a key is read, or a default permission updates it in the same commit.

Workspace lints enforce part of this mechanically: `unsafe_code = "forbid"`, `missing_docs = "warn"`, and `clippy::unwrap_used = "warn"` (allowed in tests via `clippy.toml`). With `-D warnings` these are hard failures.

## Crates

**`anvil-types`** holds shared types only: provider and roles config (`coder`, `summarizer` and the optional `planner`, `worker` and `reviewer`), the skill manifest and the session status file (`status.rs`), plus the validators for each. Its only logic is validation. It also owns loading: the user's anvil config (`config::load`), the tightening rules a repository may add (`project`), the trust decisions (`trust`) and the `.env` file (`dotenv`), plus the one list of environment variables child processes receive (`child_env`), shared so core, compat and memory scrub the same way, and the managed policy file (`policy`). It must never depend on another workspace crate, on tokio, or on reqwest. Every other crate may depend on it.

**`anvil-core`** is the agent loop, tool registry, provider adapters (OpenAI and Anthropic protocols), and context management. The loop plans on the planner and builds on the coder when the two differ, escalates a stuck Build round to the planner, attributes spend per `Role`, and runs the calls of one turn concurrently when their tool says `parallel_safe`; in Plan it refuses a call whose tool says it may write. `git.rs` is the one way anvil starts git in a project: with hooks, the file system monitor, pagers, editors and remotes turned off, and refusing a repository whose own config names a filter or signing program (`tests/one_git_entry.rs` enforces it). It depends on `anvil-types` only. It must never depend on `anvil-compat`, `anvil-memory`, `anvil-telemetry` or the CLI; those plug into it from the outside through the CLI.

**`anvil-compat`** is Claude Code compatibility: SKILL.md, CLAUDE.md, `.claude/commands`, `.claude/agents`, hooks, and the MCP client. It also ships the two built-in agents (`agents/explore.md`, `agents/implement.md`), which a project or user agent of the same name overrides. It depends on `anvil-types` and `anvil-core` (to register tools and hooks). It must never depend on `anvil-memory` or `anvil-telemetry`.

**`anvil-memory`** is cross-project memory and coding-style rules: rules filtered by the project's languages (`languages.rs`) and project templates (`templates.rs`). It makes no model calls; `anvil-cli` supplies the summarizer. It depends on `anvil-types` only. It must never depend on `anvil-core` or `anvil-compat`.

**`anvil-telemetry`** records content-free usage on the local machine and sums it up for `anvil stats`; nothing it writes is sent anywhere. It consumes telemetry event values and never calls into the agent loop, so it depends on `anvil-types` only. It must never depend on `anvil-core`.

**`anvil-tui`** is the palette and the small pure rendering helpers the full-screen UI draws with: `theme.rs` (`Theme`, `Shade`), `layout.rs` (text fitting, labels, `centered`), `widgets.rs` (panels, label-value rows, progress bars, status tokens), `wrap.rs` and the ANVIL `LOGO`. It never touches the terminal. It depends on no workspace crate except, if ever needed, `anvil-types`.

**`anvil-cli`** is the `anvil` binary: clap argument parsing and wiring only. It is the one place where the `anvil-*` libraries meet, and the only crate allowed to use `anyhow`. `src/status/` writes each session's `status.json` (a pure tracker fed by loop events and by wrappers around the prompt, the plan gate and the memory decider) and lists sessions for `anvil status`. `src/policy.rs` loads the managed policy from its fixed system path and applies the startup parts; `Wiring::with_policy` applies the rest in one place. `src/build/` is `anvil build`: it plans work items on the planner and runs each as one fresh session through the same function `anvil run` calls, so there is no second loop; an item is done only when its acceptance command and the project check pass.

The full-screen UI lives in `crates/anvil-cli/src/tui/` (ratatui + crossterm). Everything under `tui/` is pure except `runner.rs`, which owns the terminal: `App` consumes `UiMsg`s and draws into a frame, so tests drive it with messages and a `TestBackend`. Modules under `tui/` may import `anvil_core`, `anvil_types` and `anvil_tui` types, tokio channels and the UI crates, and nothing else from this crate: never `Wiring`, `anvil_memory`, `anvil_compat` or telemetry. `frontend.rs` gathers what the UI shows into `SessionInfo` and bridges prompts to it. Every color comes from `anvil_tui::theme`. Scripts and CI use `--plain` (or `--yes`, or no TTY) for the line renderer, which must stay byte-identical; `ANVIL_NO_ANIMATION` skips the startup logo animation.

## Repository layout

- `skills/`: Claude Code-format skills that ship with anvil (`skills/<name>/SKILL.md`).
- `SECURITY.md`: what anvil reads, stores and sends, for a reviewer.
- `docs/enterprise/`: data flow, threat model and pilot scope template; `policies/`: the example managed policies (`restricted.toml`, `standard.toml`), each parsed by a test; `docs/ADMIN_GUIDE.md`: installing and writing a policy; `docs/ROADMAP.md`: deferred work; `docs/CODE_TOUR.md`: the request path, file by file.
- `profiles/`: profiles that ship with anvil, such as `strict.toml`.
- Trunk is `main` and is protected. Changes reach it through pull requests with the required `ci` check (`.github/workflows/ci.yml`).
- Config: the user's `~/.anvil/config.toml` (plus an optional `--profile`). A repository's `.anvil/config.toml` and `.anvil/permissions.toml` may only add `deny` and `ask` rules (`anvil-types/src/project.rs`); its hooks, MCP servers, skills, commands, agents and instruction files load only once the user trusts it (`anvil-cli/src/trust.rs`, `~/.anvil/trust.toml`). `anvil config init [--global]` writes a template.
