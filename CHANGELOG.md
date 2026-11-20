# Changelog

All notable changes to anvil are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). The release
workflow publishes the section of the tagged version as the release notes.

## [0.2.0] - unreleased

Anvil is a coding agent harness for open-weight models in regulated,
restricted-network environments. This release reworks it for review by an IT
security team: what the agent may read, run, send and store on the developer's
machine, who controls those limits, and what record is left behind.

### Removed

- `anvil update` (self-update), its `[update]` configuration and the release
  lookup behind it. A newer binary comes from whoever distributes anvil.
- Everything that served the separate repository that consumed the ticket
  schema: `anvil ticket` and its
  subcommands, the `/ticket` chat command, ticket drafting, the ticket queue
  and its git push, the Ticket schema and its `--schema` binary, and the
  `[tickets]` configuration.
- Webhook notifications (`[notify]`).
- The herdr terminal multiplexer integration (`[integrations]`).
- Learning rules from the owner's edits to files anvil wrote
  (`anvil memory learn` and the session-start step that ran it).
- Installing skills from a remote URL; `anvil skill add` takes a local
  directory only.
- Migration from the program's previous name and directories.
- Loading a project's `.env` file. Keys come from the real environment and
  `~/.anvil/.env` only.
- Hosted API providers as setup defaults. `anvil setup` and
  `anvil config init` write one self-hosted OpenAI-compatible endpoint; the
  Anthropic and OpenAI protocol adapters remain and hosted providers are a
  documented option.

### Added

- A managed policy file at a fixed system path that the user, the project,
  profiles, environment variables and flags cannot override.
- Confinement of anvil's own tools: a workspace boundary for every file tool,
  a deny list for secret paths, a scrubbed environment for every program anvil
  starts, and stricter shell modes.
- One HTTP client for the whole workspace, with redirects off and an egress
  allowlist.
- `anvil setup --endpoint <url>` for a single self-hosted model server.

### Changed

- Repositories are now named in anvil's records by `<slug>-<12 hex digits>`,
  the hex being the start of the SHA-256 of the repository's canonical path,
  so `/w/my-app`, `/w/my_app` and `/w/my/app` no longer share one name. This
  names the folder under a policy's `transcript_dir`, the file under
  `~/.anvil/permissions/` and the key in `~/.anvil/trust.toml`. Trust
  decisions and saved `always` rules recorded under the old names stop
  applying: anvil asks again, and sessions already under `transcript_dir`
  stay in their old folders.
- MCP tool listings are approved together with the server's definition, and
  the digest covers every tool annotation, so earlier approvals no longer
  match and the next listing is recorded again. The MCP tool cache is gone.

### Release and supply chain

- Releases are cut from a version tag (`v*`) only, no longer from every push
  to `main`, and are built with `--locked`.
- Each release publishes `SHA256SUMS`, a CycloneDX SBOM, a source archive with
  vendored dependencies for offline builds, and GitHub artifact attestations
  (SLSA build provenance) for every asset.
- A macOS build for Apple silicon (`aarch64-apple-darwin`).
- Every GitHub Action is pinned to a full commit SHA, and a test fails on an
  unpinned one.
- `cargo deny check` in CI: RustSec advisories, a license allowlist, a ban on
  `openssl-sys`, `openssl` and `native-tls`, and crates.io as the only source.
- Dependabot for Cargo and GitHub Actions, and a `CODEOWNERS` file.
- The workspace crates are marked `publish = false`.

## [0.1.0]

The first version, released continuously from `main` under date tags.
