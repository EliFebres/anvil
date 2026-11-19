# Decisions

Choices made while hardening anvil for regulated, restricted-network environments, each with the reason. Where two options were reasonable, the more restrictive one was taken. Newest phase last.

## Taken with the repository owner before the run

- **The work continues on `enterprise/hardening` and draft pull request #43.** The branch already held the first round of hardening (removals, repository trust, the file jail, the scrubbed child environment, one HTTP module, the managed policy). Starting again from `main` would have redone that work for no gain.
- **The Ticket schema leaves anvil.** `anvil-types` no longer has `Ticket`, its validator, `ticket::dirs` or the `--schema` binary. The separate factory repository still imports them at a pinned commit, so it keeps building; before it moves its pin past this branch it must copy `Ticket` and `ticket::dirs` into its own types crate. Nothing in this repository was changed to accommodate it.
- **This plan overrides the decisions of 2026-11-17 where they conflict.** herdr reporting is deleted rather than kept behind a policy switch. HTTP MCP servers come back only behind the network allowlist of a later phase. Provider tolerance for self-hosted servers and the `anvil doctor` acceptance test stay on the roadmap.

## Phase 1: removals

- **`classify.rs` and `signal.rs` are removed with ticket drafting.** Their only consumer was `anvil ticket draft`: the session end classified records into `signals.jsonl`, and nothing else read that file. `anvil stats` reads the records directly and is unaffected.
- **The `[telemetry]` table is removed.** It only held the thresholds that turned statistics into ticket signals.
- **A configuration that still sets a removed table fails to load.** `[tickets]`, `[telemetry]` and `[integrations]` join `[update]` and `[notify]`: anvil stops and names the table and why it is gone, instead of ignoring it. Silently ignoring it would let someone believe a feature is still on.
- **`anvil memory` is gone, not only `anvil memory learn`.** `learn` was its only subcommand. Facts and standing rules stay and are reached as before (the end-of-session proposals, `rule: <text>`, `anvil rules`).
- **The managed policy's `summarizer_calls` now covers one call.** With the start-of-session rule proposals deleted, it switches only the end-of-session memory summarizer.
- **`anvil setup` writes one example provider named `endpoint`.** It is the same name `anvil setup --endpoint` writes, so running that command replaces the example in place. The example URL is `https://models.internal.example/v1`: the `.example` domain is reserved and never resolves, so an unedited example cannot reach a real host, and it uses https so it passes the egress rules unchanged. Its key variable is `ANVIL_ENDPOINT_KEY`. Under a managed policy, `anvil setup` without `--endpoint` is still refused, because the example would not be on the policy's allowlist.
- **`anvil config init --global` writes the same single example.** The template used to describe a rented GPU behind a vendor's proxy, which is a hosted service; it now describes a server on the organization's own network.
- **Hosted APIs are documented, never written.** The Anthropic and OpenAI protocol adapters stay, and README.md shows one hosted provider entry for a user who adds it by hand.
- **The build commit is read at compile time, without a build script.** `anvil --version` prints `<version> (<commit>)` when the release build sets `ANVIL_BUILD_SHA`, and the version alone for a build from source. A build script would run code at build time for no extra benefit. The release workflow has to set the variable; that change belongs to the supply chain phase.
- **`anvil scan-keys` still reads a repository's `.anvil/.env`.** It never loads it into the environment; it reads the values only to fail when one of them appears in a tracked file. Nothing a repository ships can set an environment variable of anvil.
- **The old-name check has no exceptions.** With the migration module gone, `just no-old-name` and the CI step search the whole tree. The CI step was edited in place, ahead of the supply chain phase, because leaving the exceptions would have pointed at files that no longer exist.
