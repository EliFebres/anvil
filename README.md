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

Forge remembers two things across sessions, both as plain files under `~/.forge/` you can edit by hand:

- `rules.toml` holds standing coding rules (`forge rules list|add|remove|check`). They enter every system prompt, the reviewer must cite a rule id for any violation, and rules with a `check` run on the session's changed files in Verify; a failure sends the loop back to Build. In `forge chat`, a line starting with `rule:` saves a new rule on the spot.
- `memory/` holds one fact per line in `tooling.md`, `workflow.md` and `projects/<repo>.md`. They load into the prompt under a 2,000-token cap, and at the end of every session the summarizer proposes new facts that you keep or drop one by one (`--yes` keeps all).

Run `just check` (fmt, clippy, test) before committing. See `CLAUDE.md` for conventions and crate boundaries.
