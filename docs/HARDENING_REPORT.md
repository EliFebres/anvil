# Hardening report

This report covers the run that repositioned Anvil as "a coding agent harness for open-weight models in regulated, restricted-network environments" and hardened it for review by an IT security team. All work is on the branch `enterprise/hardening` (draft pull request #43). Every choice made along the way, with its reason, is in `docs/DECISIONS.md`; this report summarizes the results and points to the decisions a reviewer is most likely to question.

## How the run was done

Each phase was implemented, then attacked by a separate agent that had not seen the implementation and was given only the phase's claims. Every break it found became a failing test first and a fix second. A final reviewer then attacked the whole branch end to end with a hostile repository and a scripted model endpoint, and another checked every document against the code. The breaks those reviews found, and their fixes, are listed per phase below.

`just check` (old-name check, `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test --all`, `anvil scan-keys`) passed before every commit, and `cargo deny check` from Phase 7 on. On the development machine, Windows Smart App Control can block freshly built test binaries; the checks were run with the build output outside the repository, and no run reported a blocked binary. CI runs the same checks on Ubuntu and Windows for every push.

## Numbers

| | before (`main`, ab9c71d) | start of this run (c614dd9) | end of this run |
|---|---|---|---|
| Test functions (`#[test]` and `#[tokio::test]` in `crates/`) | 772 | 851 | 1044 |
| Tests run by `cargo test --all` on Windows | not recorded | not recorded | 1019 passed, 0 failed, 2 ignored (some of the 1044 are Unix only) |
| Packages in `Cargo.lock` | 427 | 427 | 427 |

The package count did not change: every crate the removals stopped using directly is still pulled in by another crate. Direct dependencies removed: `schemars`, `ulid` and `insta` from `anvil-types`; `similar` and `sha2` from `anvil-memory`; `toml`, `ulid` and `insta` from the former `anvil-telemetry`; test-only dependencies moved to `[dev-dependencies]` in `anvil-compat` (`futures-util`, `tempfile`) and `anvil-cli` (`ulid`, a duplicate `tempfile`). Added: `tokio-rustls` as a dev-dependency for the TLS tests (already in the lock file), and existing workspace crates (`sha2`, `ignore`) in crates that now need them. No new third-party crate entered the lock file.

## Phase 1: Remove

**Done.** Deleted from the binary, with their config keys, tests, snapshots and documentation: self-update; tickets, ticket drafting, the queue and git push code, the Ticket schema, its validator and the `--schema` binary; failure classification and signals (used only by ticket drafting); webhook notifications; herdr reporting; learning from the owner's edits and the whole `anvil memory` command; remote skill install; the legacy migration (the old-name check now has no exceptions). A repository's `.env` is never loaded. `anvil setup` and `anvil config init` write one self-hosted OpenAI-compatible endpoint; hosted providers are documented, not written. A configuration that still sets a removed table stops Anvil with an error naming it.

**Review breaks fixed.** A trusted repository's MCP definitions could expand provider keys into a server's arguments or environment, and the trust prompt printed the expanded value. Expansion now reads only variables a child process may receive, and the prompt shows definitions as written.

**Not done.** Nothing in scope. The separate repository that consumed the Ticket schema still builds at its pinned commit; it must copy `Ticket` and `ticket::dirs` into its own types crate before it moves its pin past this branch.

## Phase 2: Managed policy

**Done.** A policy file at a fixed path (`/etc/anvil/policy.toml` on Linux and macOS, `C:\ProgramData\anvil\policy.toml` on Windows), schema version 1, with ownership checks (root and no group or other write on Unix; no write right for non-administrators on Windows, read as SDDL). A broken, unreadable or badly owned file stops Anvil; no file means "unmanaged", shown at startup and in every audit record. The policy beats user config, profiles, environment variables, flags and project files, and a conflicting lower layer is an error naming the policy key. Project files can no longer loosen anything; trust is pinned to a SHA-256 of what the repository ships; tools never write the files that grant power. `anvil policy show`, `anvil policy check`, doctor rows, and `policies/restricted.toml` and `policies/standard.toml`.

**Review breaks fixed.** An unreadable policy path ran unmanaged; `default = "allow"` acted as `--yes`; forbidden items in `~/.claude` were only noted; a symlinked skill escaped the trust digest; Windows name folding (trailing dot or space, `::$DATA`) let a tool create a protected file; scripts run by trusted hooks could be edited; `policy show` omitted unset keys; a null DACL passed the ownership check (final review).

## Phase 3: Tool confinement

**Done.** Every file tool resolves through the symlink-aware resolver and refuses paths outside the workspace and extra roots; a built-in secret-path list (extended by the policy) is refused by reads and skipped by searches. Every process Anvil starts begins from a cleared environment plus a short allowlist and `[tools] pass_env`; a source scan fails the build otherwise. Shell modes `off`, `allowlist` (argument vector, no shell interpreter, metacharacters refused, argument-prefix rules, path arguments outside the workspace refused, git limited to a fixed subcommand list) and `ask` (with deny categories in every mode). A container executor (`--network none`, workspace as the only mount, `.git` read-only, read-only root, non-root user, `--cap-drop ALL`, `no-new-privileges`, limits, `--pull=never`), proven by a real Docker test on the Linux CI runner. `--yes` and `anvil build` obey the policy; subagents get the same boundary, environment and policy.

**Review breaks fixed.** Folded Windows names on reads; doctor and setup started children with the full environment; allowlisted git could be steered into running programs, writing or reading anywhere (`-c alias`, `--output`, `--no-index`, then `rebase --exec`, `bisect run`, `--upload-pack` and others in the final review); quoting hid commands from the deny categories; `git fetch` reached the network; the container could run as root under another spelling.

**Not done (stated in SECURITY.md and ROADMAP.md).** Matching text in `ask` mode is a guard rail, not a boundary. An allowed program's own behaviour (build scripts, test code) is not confined on the host; the container executor is the boundary for that. Hard links to secret files are not detected. Hooks and stdio MCP servers do not run in the container. No OS-native sandbox for the host executor.

## Phase 4: Network egress

**Done.** One HTTP client in `anvil-core/src/net.rs`; clippy forbids the client types elsewhere, other HTTP crates are banned in `deny.toml`, and a test refuses socket types outside `net.rs`. Redirects off; CA bundle, mutual TLS and proxy mode (`none` by default) applied; every request's `host:port` checked before connecting; plain `http://` only for loopback unless configured, checked at load and per request; an API key is attached only to its configured host; HTTP MCP returns behind the allowlist; `anvil doctor --egress`.

**Review breaks fixed.** Two spellings created a second, unchecked client; HTTP MCP error bodies could carry a sent header value; the container engine could pull an image from a registry.

**Not done.** FIPS-validated cryptography (what it would take is in ROADMAP.md). HTTP MCP session renewal, GET streams and OAuth.

## Phase 5: Data protection

**Done.** One redaction module for every tool result, file read, user prompt and log line, so the provider and the transcript see the same text; untrusted content wrapped in delimiters with a per-session nonce, documented as a mitigation for prompt injection; MCP tool listings approved by a SHA-256 over every field the client acts on, with changes refused until approved (under a policy, only the administrator can approve, by pinning the digest); owner-only files and folders on Unix and a Windows doctor check; transcript modes, `anvil purge`, retention at session start; `[features] memory` gates every memory read and write.

**Review breaks fixed.** Key bodies read without their header; hook stderr logged raw; lookalike delimiters; the read-only hint outside the listing digest; changed listings unseen while cached; purge of live sessions; repository keys that collided; files created with default modes.

**Not done.** Secrets in unknown formats or encodings are not detected (documented). Windows folder ACLs are checked, not narrowed.

## Phase 6: Audit log

**Done.** "Telemetry" is gone from the product: the crate is `anvil-audit` and usage records live in `.anvil/usage/`. Each session writes a hash-chained `audit.jsonl` (sequence, UTC time, session, user, host, version, commit, binary SHA-256, policy hash or `unmanaged`) with every event type the plan lists, mirrored to the policy's directory; subagents are audited under the parent session; `anvil audit verify` and `show`; a required sink that fails stops the session before anything else runs.

**Review breaks fixed.** A mirror added mid-session never verified; the canonical form was described but not enforced; the documented Python check disagreed with Anvil; a tool ran after its decision record failed to write; hooks ran ungated; a deleted log went unnoticed; the agent's own file tools could erase earlier sessions' logs (final review).

**Not done (stated in SECURITY.md).** A chain on one disk cannot show that its end was cut off or that the whole file was rewritten; the mirror exists for that. Files changed by shell commands are not recorded as file changes. The build planner's first request and acceptance command output are outside every log. Syslog, Windows Event Log and OpenTelemetry export are on the roadmap.

## Phase 7: Supply chain and releases

**Done.** Every GitHub Action pinned to a full commit SHA; `release.yml` runs on `v*` tags only, builds with `--locked`, and publishes `SHA256SUMS`, a CycloneDX SBOM, build provenance attestations and a vendored source archive, with an arm64 macOS build; `deny.toml` and a `cargo deny` CI job; Dependabot; `CODEOWNERS`; version 0.2.0 and `CHANGELOG.md`.

**Not verified.** No tag has been pushed, so the release workflow (macOS build, attestation signing, release upload) has never run. macOS builds are released but not tested in CI.

## Phase 8: Documentation

**Done.** README.md and SECURITY.md rewritten; `docs/THREAT_MODEL.md` (OWASP Top 10 for Agentic Applications 2026, read from the official document), `docs/CONTROLS.md` (NIST SP 800-53 Rev. 5 and SP 800-171 Rev. 3, identifiers checked against the publications, with a plain statement that Anvil holds no certification and has had no independent assessment), `docs/ADMIN_GUIDE.md`, `docs/DEPLOYMENT.md`, `docs/SECURITY_TESTS.md` (every claim with its tests; a test fails when a named test does not exist, is not a test function, or lacks its platform marker), `docs/ROADMAP.md`, `docs/CODE_TOUR.md`, `docs/USAGE.md`, `docs/PILOT_SCOPE.md`, and the updated `CLAUDE.md` with the rule that a security claim needs a test.

**Flagged, not changed.** The four screenshots in `docs/` no longer match the UI: `ui-startup.png` has no `policy` row and shows hosted models, and the other three show a hosted model in the status line. They are kept and listed in ROADMAP.md.

## Decisions a reviewer might question

All are in `docs/DECISIONS.md` with their reasons.

- The policy path on macOS is `/etc/anvil/policy.toml`, as the plan says, not `/Library/Application Support`. On Windows the path is a literal (`C:\ProgramData\anvil`), never read from `%ProgramData%`.
- With a policy installed, almost everything optional is off until the policy turns it on, including the shell (a policy without `[shell]` removes the tool), project trust, hooks, MCP, skills, memory and transcripts (`transcript = "off"`).
- Conflicts in Anvil's own files are errors, but items a trusted repository or `~/.claude` ships that the policy forbids are left out with a visible note naming the key (`~/.claude` items are errors when they are the user's own). Making every repository with a `.mcp.json` unusable under a policy seemed worse than a named note.
- A trusted repository's `allow` rules apply, but as the weakest rule.
- Writes to files that grant power, to Anvil's records, and to the folders of scripts that hooks run are refused outright, never asked about. A hook script in the project root therefore closes the whole project to the file tools.
- Allowlisted git runs a fixed list of subcommands; a pattern naming another subcommand (for example `git push origin *`) no longer runs anything.
- Git calls Anvil makes itself start from a cleared environment plus identity and config-location variables.
- Proxy and TLS settings exist only in the policy, so an unmanaged user loses `HTTPS_PROXY`.
- Redaction applies to the user's own prompt text, and only a policy can turn redaction off.
- The first MCP listing is recorded without a prompt; any later change is refused until approved.
- A review that gives no verdict counts as a request for changes.
- Repository ids now carry a digest, so existing trust records and saved permission rules stop applying once, and the user is asked again.
- chrono's "unmaintained" advisory is ignored in `deny.toml` with a reason; replacing chrono is on the roadmap.

## What only the repository owner can do

- Set the repository description to the one-line description and add topics.
- Enable secret scanning, Dependabot alerts and private vulnerability reporting (private vulnerability reporting is disabled today, and SECURITY.md points to it).
- Check the branch protection on `main` still requires the `ci` check, since the CI workflow gained a `deny` job.
- Create the `v0.2.0` tag when the pull request is merged, then confirm the release workflow's first run (macOS build, attestations, SBOM, vendored archive).
- Retake the four screenshots in `docs/`.
- Before the separate repository that consumed the Ticket schema moves its pin past this branch, copy `Ticket` and `ticket::dirs` into its own types crate.
- Remove a `[tickets]`, `[telemetry]`, `[integrations]`, `[update]` or `[notify]` section from any existing `~/.anvil/config.toml` (the development machine's own config has `[tickets]`); Anvil refuses to start until it is gone.
- Commission the independent penetration test listed on the roadmap.
