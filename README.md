# anvil

Two Rust binaries that share one types crate and a file-based ticket queue.

- `forge` is a CLI coding agent: agent loop, tool registry, provider adapters, and Claude Code compatibility (skills, CLAUDE.md, commands, hooks, MCP).
- `factory` is the "Dark Factory": an unattended service that claims tickets, works on `factory/<ticket-id>` branches, gets multi-model review, and merges through pull requests.
- `crates/forge-types` is the only code both sides share: Ticket, ProviderConfig, SkillManifest, telemetry references, and the validators that guard them.
- `tickets/open/*.toml` is the handoff: forge's telemetry drafts tickets, humans or the factory pick them up. The validator refuses any ticket that carries project code or paths outside this repo.
- Models are config, not code: `~/.forge/config.toml` and `.forge/config.toml` declare providers (OpenAI- or Anthropic-protocol) and map roles (coder, summarizer, reviewer) onto them.
- `crates/eval` scores a model or a build against a fixed task set and is used by both binaries.
- Forge-side crates: `forge-core`, `forge-compat`, `forge-memory`, `forge-telemetry`, `forge-cli`.
- Factory-side crates: `factory-core`, `factory-review`, `factory-cli`.
- `skills/` holds Claude Code-format skills that ship with forge.

Run `just check` (fmt, clippy, test) before committing. See `CLAUDE.md` for conventions and crate boundaries.
