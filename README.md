# anvil

Two Rust binaries that share one types crate and a file-based ticket queue.

- `anvil` is a CLI coding agent: agent loop, tool registry, provider adapters, and Claude Code compatibility (skills, CLAUDE.md, commands, hooks, MCP).
- `factory` is the "Dark Factory": an unattended service that claims tickets, works on `factory/<ticket-id>` branches, gets multi-model review, and merges through pull requests.
- `crates/anvil-types` is the only code both sides share: Ticket, ProviderConfig, SkillManifest, telemetry references, and the validators that guard them.
- `tickets/open/*.toml` is the handoff: anvil's telemetry drafts tickets, humans or the factory pick them up. The validator refuses any ticket that carries project code or paths outside this repo.
- Models are config, not code: `~/.anvil/config.toml` and `.anvil/config.toml` declare providers (OpenAI- or Anthropic-protocol) and map roles (coder, summarizer, reviewer) onto them.
- `crates/eval` scores a model or a build against a fixed task set and is used by both binaries.
- Anvil-side crates: `anvil-core`, `anvil-compat`, `anvil-memory`, `anvil-telemetry`, `anvil-cli`.
- Factory-side crates: `factory-core`, `factory-review`, `factory-cli`.
- `skills/` holds Claude Code-format skills that ship with anvil.

Anvil remembers two things across sessions, both as plain files under `~/.anvil/` you can edit by hand:

- `rules.toml` holds standing coding rules (`anvil rules list|add|remove|check`). They enter every system prompt, the reviewer must cite a rule id for any violation, and rules with a `check` run on the session's changed files in Verify; a failure sends the loop back to Build. In `anvil chat`, a line starting with `rule:` saves a new rule on the spot.
- `memory/` holds one fact per line in `tooling.md`, `workflow.md` and `projects/<repo>.md`. They load into the prompt under a 2,000-token cap, and at the end of every session the summarizer proposes new facts that you keep or drop one by one (`--yes` keeps all).

Anvil also records what it does, never what it works on: `.anvil/telemetry/<date>.jsonl` (with a copy under `~/.anvil/telemetry/`) holds sizes, digests, token counts, latencies and outcomes, and no tool arguments, results or model text. `anvil stats --since 7d` sums it up, with cost estimates when a provider has a `price_per_mtoken`. At the end of each session a classifier turns the records into signals (a tool the model asked for that does not exist, a review loop that never converged, a slow provider, a `/ticket <text>` you typed in chat). The daily habit is `anvil ticket draft`, which has the summarizer write ticket drafts from the signals alone, then `anvil ticket review` or `anvil ticket submit --all`, which validates each draft, moves it to `tickets/open/` and pushes it on a `tickets/<id>` branch. That push is the only thing that ever leaves the machine toward the factory.

Run `just check` (fmt, clippy, test) before committing. See `CLAUDE.md` for conventions and crate boundaries.
