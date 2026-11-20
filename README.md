# Anvil

Anvil is a coding agent harness for open-weight models in regulated, restricted-network environments.

## What Anvil is, and who it is for

Anvil is a terminal program, one binary written in Rust, that turns a model on your own infrastructure into a coding agent. Point it at a repository and give it a task: it plans, edits files, runs the project's checks and has the change reviewed, with a real tool set (files, search, shell) under limits you set. It talks to a self-hosted, OpenAI-compatible endpoint (vLLM, llama.cpp, an internal gateway); a hosted API is a configuration option, never a default. It reads Claude Code's conventions (`CLAUDE.md`, `SKILL.md`, `.claude/commands`, `.claude/agents`, hooks, MCP servers), has no server component, no background process, no account and no self-update.

It is written for organizations that must answer for what a coding agent does on a workstation: banks, asset managers, defense contractors and others where an IT security team and a platform team decide what runs. The model and the machine it runs on are secured separately by whoever hosts them. Anvil's job is everything above the model: **what the agent may read, run, send and store on the machine where Anvil runs, who controls those limits, and what record is left behind.**

Anvil holds no certification and has had no independent assessment. Every control below is enforced in code and proven by a named test; [docs/SECURITY_TESTS.md](docs/SECURITY_TESTS.md) lists them, and a test fails the build when one it names no longer exists.

## Security posture

| control | where it is enforced | test that proves it |
|---|---|---|
| Requests go only to allowed `host:port` pairs (the policy's `[network] allowed_hosts`, or the user's configured endpoints), over https except loopback, with no redirects (`NET-1` to `NET-3`) | `HttpRequest::send` and `Egress` in `crates/anvil-core/src/net.rs` | `a_host_outside_the_allowed_set_is_refused_before_any_connection`, `a_redirect_to_a_second_server_is_not_followed` |
| One HTTP client in the whole workspace; no other crate may build one (`NET-7`) | `http_client` in `net.rs`, `clippy.toml`, `deny.toml` | `only_the_net_module_builds_an_http_client` |
| An API key is sent only to its own provider's `host:port` and is redacted from error bodies and logs (`NET-4`, `KEY-2`) | `HttpRequest::api_key` in `net.rs`, `send_attempt` in `crates/anvil-core/src/provider/http.rs` | `an_api_key_never_reaches_another_host`, `a_server_that_echoes_the_key_cannot_put_it_in_output_or_logs` |
| A managed policy at a fixed, administrator-owned path outranks every other layer; a conflict is an error naming the key (`POL-1` to `POL-4`) | `crates/anvil-types/src/policy/`, `crates/anvil-cli/src/policy.rs`, `Wiring::with_policy` | `a_policy_file_with_bad_ownership_stops_anvil`, `a_profile_cannot_loosen_the_policy` |
| File tools stay inside the workspace and never open credential or key files (`FS-1` to `FS-3`) | `ToolContext::checked_path`, `holds_secrets` in `crates/anvil-core/src/tools/workspace.rs` | `boundary_symlink_escape_is_refused`, `secret_paths_are_refused_inside_the_workspace_and_skipped_by_searches` |
| File tools never write the files that grant power or anything in `~/.anvil`, even under `--yes` (`FS-5`) | `grants_power` in `workspace.rs` | `tool_writes_to_files_that_grant_power_are_refused` |
| The shell can be off, limited to listed programs started without a shell, or run in a container with no network (`SH-1` to `SH-6`) | `crates/anvil-core/src/tools/shell.rs`, `Permissions::shell_refusal` | `the_shell_is_off_under_a_policy_unless_the_policy_turns_it_on`, `allowlist_mode_starts_the_program_without_a_shell`, `the_container_arguments_drop_network_root_capabilities_and_host_mounts` |
| Programs Anvil starts get a cleared environment without keys (`KEY-4`) | `crates/anvil-types/src/child_env.rs`, `run_command` | `every_non_test_command_starts_from_a_cleared_environment`, `a_child_process_that_prints_its_environment_shows_no_key` |
| A cloned repository can only tighten rules; what it ships loads only after the user trusts that exact content (`TRUST-1` to `TRUST-3`) | `crates/anvil-types/src/project.rs`, `crates/anvil-cli/src/trust.rs` | `a_project_config_that_changes_base_url_stops_anvil`, `a_hook_added_after_trust_does_not_run_until_trusted_again` |
| MCP servers are allowlisted under a policy, and a changed tool listing stops the server (`TRUST-7`) | `crates/anvil-compat/src/mcp/approval.rs` | `under_a_policy_a_changed_listing_is_refused_and_a_pinned_one_loads` |
| Secrets in tool output are redacted before they reach the model or disk, and that output is marked as untrusted data (`RED-1`, `RED-6`) | `crates/anvil-types/src/redact.rs`, `DataGuard` in `crates/anvil-core/src/guard.rs` | `a_secret_in_tool_output_does_not_reach_the_provider_or_the_transcript`, `content_cannot_close_the_element_or_open_a_fake_one` |
| The policy decides how much of a session is kept on disk (`DATA-1`) | `SessionRecord::kept_as` in `crates/anvil-core/src/context/session.rs` | `a_metadata_transcript_keeps_sizes_and_digests_but_no_text` |
| Every session writes a hash-chained audit log, mirrored where the policy says, and stops when a required log cannot be written (`AUD-1` to `AUD-5`) | `crates/anvil-audit/src/audit/`, `crates/anvil-cli/src/audit.rs` | `the_audit_chain_detects_edit_removal_and_reorder`, `a_required_audit_sink_that_fails_stops_the_session` |
| Releases are built only from version tags, locked, with checksums, an SBOM, vendored source and provenance attestations (`SUP-1` to `SUP-4`) | `.github/workflows/release.yml` | `release_runs_only_on_version_tags_and_builds_locked`, `release_publishes_checksums_an_sbom_vendored_source_and_provenance` |

What Anvil does **not** enforce matters as much: in the default `ask` shell mode on the host, an approved command runs with the user's rights and is not sandboxed, and everything a session reads is sent to the model host. SECURITY.md ("What is not enforced") lists every such limit.

## Quick start against a self-hosted endpoint

Install a release binary (verify it first: [docs/ADMIN_GUIDE.md](docs/ADMIN_GUIDE.md), "Verifying a release") or build from source with the Rust toolchain in `rust-toolchain.toml`:

```sh
git clone https://github.com/EliFebres/anvil
cd anvil
cargo build --release --locked      # target/release/anvil
```

Point Anvil at your model server. Setup reads the server's model list, writes one `endpoint` provider with the context size the server reports, and uses it for every role:

```sh
export ANVIL_ENDPOINT_KEY=...       # any value if your server needs no key
anvil setup --endpoint https://models.internal.example/v1 [--model <id>]
anvil doctor                        # keys, provider, tools; --egress lists every destination
```

The key is read from the environment or `~/.anvil/.env`, never from a configuration file or the repository. A non-loopback endpoint must use https unless you list its host in `[network] allow_http_hosts` (unmanaged only). The written configuration (`~/.anvil/config.toml`) looks like this:

```toml
[providers.endpoint]
protocol = "openai"
base_url = "https://models.internal.example/v1"
api_key_env = "ANVIL_ENDPOINT_KEY"
model = "<model name as served>"
context_window = 131072
supports_tools = true
supports_reasoning = false

[roles]
coder = "endpoint"
summarizer = "endpoint"
```

Then work on a task in any repository:

```sh
cd ~/some-project
anvil "add a --json flag to the list command"   # plan, build, verify, review
anvil chat                                       # a conversation with tools
```

File writes and shell commands ask first. The first session in a repository that ships hooks, MCP servers, skills or instruction files asks whether to trust it. [docs/USAGE.md](docs/USAGE.md) covers every command, the full-screen UI, long builds and the rest.

## Administrator quick start with a policy file

1. Start from `policies/restricted.toml` (one model server, no extensions, no trust in repositories, every write and command asks, commands limited to an allowlist started without a shell, metadata-only transcripts, a required audit log). Set `[network] allowed_hosts` to your endpoint's `host:port`, and `[audit] mirror_dir` to a folder you collect.
2. Validate it: `anvil policy check restricted.toml`.
3. Install it owned by root (Linux, macOS) or Administrators (Windows), not writable by anyone else:

   ```sh
   sudo install -d -o root -g root -m 755 /etc/anvil
   sudo install -o root -g root -m 644 restricted.toml /etc/anvil/policy.toml
   ```

   On Windows the path is `C:\ProgramData\anvil\policy.toml`, and inherited access must be removed; ADMIN_GUIDE.md has the `icacls` commands.
4. Check a workstation: `anvil policy check --ownership /etc/anvil/policy.toml`, `anvil doctor` (rows `policy`, `policy:ownership`, `policy:sha256`) and `anvil doctor --egress`.
5. Roll it out with your configuration management, and record the policy's SHA-256: every audit record carries it.

From then on nothing a user, profile, flag or repository sets can loosen it, and a conflict stops Anvil with an error naming the policy key. Every key and its default is in [docs/ADMIN_GUIDE.md](docs/ADMIN_GUIDE.md).

## Documents

| document | for | what it holds |
|---|---|---|
| [SECURITY.md](SECURITY.md) | Security reviewers | The boundaries, who sets the limits, every network destination and stored file, what is not enforced, release builds, how to report a vulnerability. |
| [docs/SECURITY_TESTS.md](docs/SECURITY_TESTS.md) | Reviewers, assessors | Every security claim next to the test that proves it. |
| [docs/THREAT_MODEL.md](docs/THREAT_MODEL.md) | Security reviewers | Assets, trust boundaries, actors, and the OWASP Top 10 for Agentic Applications 2026 mapped to mitigations, remaining risk and owner. |
| [docs/CONTROLS.md](docs/CONTROLS.md) | Compliance, assessors | Which NIST SP 800-53 Rev. 5 and SP 800-171 Rev. 3 controls Anvil's features support, with the test for each. |
| [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md) | Platform team | Reference layouts, the data flow diagram, the shared responsibility table. |
| [docs/ADMIN_GUIDE.md](docs/ADMIN_GUIDE.md) | Platform team | Verifying a release, air-gapped installation, every policy key with its default, rollout, collecting audit logs, upgrading by hand. |
| [docs/PILOT_SCOPE.md](docs/PILOT_SCOPE.md) | The pilot's owner | A template for a first, time-boxed pilot. |
| [docs/ROADMAP.md](docs/ROADMAP.md) | Everyone | What is not done yet, including the sandbox, FIPS and signing. |
| [docs/CODE_TOUR.md](docs/CODE_TOUR.md) | Engineers reading the code | A reading order through the security code, with a test to run at each stop. |
| [docs/USAGE.md](docs/USAGE.md) | Developers | Every command, the full-screen UI, workers, long builds, rules and templates. |
| [docs/DECISIONS.md](docs/DECISIONS.md) | Maintainers, reviewers | Every design choice made while hardening Anvil, with the reason. |
| [docs/HARDENING_REPORT.md](docs/HARDENING_REPORT.md) | Reviewers | What the hardening run did per phase, what it did not, the numbers, and what only the repository owner can do. |
| [CHANGELOG.md](CHANGELOG.md) | Everyone | What changed in each version. |
| `policies/`, `profiles/` | Administrators, developers | Example managed policies (`restricted.toml`, `standard.toml`) and the `strict.toml` profile, each parsed by a test. |

## Development

The workspace has seven crates: `anvil-types` (shared types, the policy, validation, redaction), `anvil-core` (the agent loop, tools, providers, the one HTTP client), `anvil-compat` (Claude Code compatibility and MCP), `anvil-memory`, `anvil-audit` (usage records and the audit log), `anvil-tui` and `anvil-cli` (the binary). [CLAUDE.md](CLAUDE.md) is the contract: conventions, which crate may depend on which, and the rule that a security claim needs a test listed in docs/SECURITY_TESTS.md.

With [`just`](https://github.com/casey/just) installed:

```sh
just check     # cargo fmt --check, clippy -D warnings, cargo test --all, anvil scan-keys, the old-name check
just deny      # cargo deny: advisories, licenses, banned crates, sources
```

CI runs the same on Ubuntu and Windows (`.github/workflows/ci.yml`), and `main` changes only through pull requests. Prompts and tool schemas are pinned with `insta` snapshots; run `cargo insta review` after a deliberate change. Releases are built by `.github/workflows/release.yml` when a version tag is pushed. Report a vulnerability privately as described in SECURITY.md.

## License

MIT or Apache-2.0, at your option: see [LICENSE-MIT](LICENSE-MIT) and [LICENSE-APACHE](LICENSE-APACHE). Contributions are accepted under the same terms.
