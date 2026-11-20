# Code tour: the security code, in reading order

A reading order for someone learning Anvil's security code. It follows the order a session meets each control: the policy first, then configuration and trust, then the checks every request and tool call passes, then the records. Each stop names the file, says what it does, names the one function to read first, and gives a test to run to see it work. Paths are relative to the repository root.

Run a test with `cargo test -p <crate> <test name>`; the crate is the folder after `crates/`. Every test named here is also listed in [SECURITY_TESTS.md](SECURITY_TESTS.md) against the claim it proves. CLAUDE.md says which crate may depend on which.

## 1. The policy file

**`crates/anvil-types/src/policy/mod.rs`, `crates/anvil-types/src/policy/ownership.rs`**

The managed policy's schema, its validation and its loading. `system_path` returns a literal path per system, so nothing a user controls can move the file. `load_checked` treats only a missing file as unmanaged; any other failure (unsafe ownership, unreadable, unknown key, a broken rule) is an error that stops Anvil. `ownership::check` requires root ownership and no group or other write on Unix, and only Administrators, SYSTEM or TrustedInstaller with write rights on Windows, read as SDDL.

- Read: `load_checked`, then `ownership::check`.
- Run: `cargo test -p anvil-types broken_policies_are_errors`

## 2. Applying the policy at startup

**`crates/anvil-cli/src/policy.rs`**

Loads the policy from the system path and applies the parts that must hold before anything else runs. `check_flags` refuses `--yes` and `--allow-tool` unless the policy allows them; `check_config` refuses a user configuration or profile that conflicts with the policy (a provider host, a looser shell mode, a token limit above the ceiling), always naming the policy key. It also holds `prune_sessions` and `prune_all`, the retention code behind `retention_days` and `anvil purge`.

- Read: `check_config`.
- Run: `cargo test -p anvil-cli skip_flags_are_refused_with_the_policy_key`

## 3. One place where the policy shapes a session

**`crates/anvil-cli/src/wiring.rs`**

`Wiring` is where the libraries meet: it discovers skills, hooks and MCP servers, builds the egress rules, the redactor, the tool registry and the tool context. `Wiring::with_policy` applies every in-session effect of the policy, top to bottom, in one function: the network table, forbidden extensions, saved rules, budgets, file deny globs and the shell settings. `registry_without_delegate` and `tool_context` build what the session and its subagents share, so a subagent cannot get looser rules.

- Read: `Wiring::with_policy`, then `registry_without_delegate`.
- Run: `cargo test -p anvil-cli a_profile_cannot_loosen_the_policy`

## 4. What a repository may say

**`crates/anvil-types/src/project.rs`**

Reads a repository's `.anvil/config.toml` and `.anvil/permissions.toml`. It keeps only rules that tighten (`deny`, `ask`, `shell_programs`) plus `allow` rules that wait for trust, and turns every key that would loosen Anvil (providers, roles, tools, network, shell, `default`, `allow_tools`) into an error naming it. This is why a cloned repository cannot point Anvil at its own server.

- Read: `load`.
- Run: `cargo test -p anvil-types every_loosening_key_is_an_error_that_names_it`

## 5. Repository trust

**`crates/anvil-cli/src/trust.rs`**

Decides whether what a repository ships (hooks, MCP servers, skills, commands, agents, instruction files, `allow` rules) may load. `fingerprint` computes a SHA-256 digest over all of it, including scripts its hooks and servers run, and `decide` compares it with the stored decision, so any change asks again. A repository that links a skill, command or agent folder is never trusted, and the policy's `allow_trust` switch is a parameter of the same path.

- Read: `decide`, then `fingerprint`.
- Run: `cargo test -p anvil-cli any_change_to_what_the_repository_ships_asks_again`

## 6. The one way out of the process

**`crates/anvil-core/src/net.rs`**

The only module that builds an HTTP client. `http_client` turns redirects off and applies the policy's CA bundle, client certificate and proxy mode, and hands out an `HttpClient` that never exposes the client inside. `HttpRequest::send` runs `Egress::check` on the final URL before any connection and only then adds the key, and only when the request's `host:port` matches the one the key was configured for. `crates/anvil-core/tests/one_http_client.rs` and `clippy.toml` keep every other crate and module from building a client.

- Read: `HttpRequest::send`, then `Egress::check`.
- Run: `cargo test -p anvil-core a_host_outside_the_allowed_set_is_refused_before_any_connection`

## 7. The permission check

**`crates/anvil-core/src/tools/mod.rs`, `crates/anvil-core/src/tools/permissions.rs`**

Every tool call goes through `ToolRegistry::execute`, which calls `check_permission`. The order is fixed: the shell allowlist refusal, the built-in deny categories, then the rules (a `deny` from any layer, a tightening `ask`, the user's `allow` and `ask`, the default, a trusted repository's `allow`, the tool's own default), then session approvals and the prompt. Layers below the user are kept apart so they can only tighten, and each decision reports its source to the audit log.

- Read: `ToolRegistry::check_permission`, then `Permissions::ruling`.
- Run: `cargo test -p anvil-core a_projects_allow_only_decides_what_no_other_rule_decides`

## 8. The file boundary

**`crates/anvil-core/src/tools/workspace.rs`, `ToolContext::checked_path` in `crates/anvil-core/src/tools/mod.rs`**

Every file tool calls `checked_path` before it opens anything. It refuses spellings Windows would fold onto another name (`win32_fold`), resolves symlinks, `..`, junctions and letter case, and requires the result to lie in the workspace or an allowed folder. `holds_secrets` refuses credential and key files even inside the workspace, `inside_git_dir` refuses writes into any git directory, and `grants_power` refuses writes to the files that would raise the agent's own permissions. `crates/anvil-core/src/tools/boundary_tests.rs` tries every tool against every spelling.

- Read: `ToolContext::checked_path`, then `grants_power`.
- Run: `cargo test -p anvil-core boundary_symlink_escape_is_refused` (Unix only; it matches no test on Windows). On Windows run `cargo test -p anvil-core boundary_windows_drive_paths_are_refused` instead; `cargo test -p anvil-core boundary_dot_dot_traversal_is_refused` runs on every system.

## 9. Running a command

**`crates/anvil-types/src/shell.rs`, `crates/anvil-core/src/tools/shell.rs`**

`split_command` is the conservative parser of allowlist mode: it refuses every shell metacharacter and returns an argument vector. `run_command` is the one way the shell tool, checks and acceptance commands start a program: directly (allowlist), through the shell (`ask`), or in a container. `deny_category` recognises network clients, pushes, installs, privilege tools and destructive patterns; `argument_refusal` refuses arguments that reach outside the project; `container_args` builds the `docker run` line with no network, no root and the workspace as the only mount.

- Read: `run_command`, then `container_args`.
- Run: `cargo test -p anvil-core the_container_arguments_drop_network_root_capabilities_and_host_mounts`

## 10. Anvil's own git calls

**`crates/anvil-core/src/git.rs`**

Anvil runs git in the project for snapshots, diffs and build commits. `command` is the only way it does so: a cleared environment and `-c` settings that turn off hooks, the file system monitor, pagers, editors and remotes, plus a refusal when the repository's own config names a filter or signing program. `allowlisted` is the stricter variant for git commands the model runs in allowlist mode. `crates/anvil-core/tests/one_git_entry.rs` fails when any other code starts git in a project.

- Read: `command`, then `allowlisted`.
- Run: `cargo test -p anvil-core hooks_planted_in_the_repository_do_not_run`

## 11. What child processes inherit

**`crates/anvil-types/src/child_env.rs`**

The one list of environment variables a program Anvil starts may receive: `PATH`, home and temp folders, the locale, what Windows needs to start programs, the Rust toolchain folders, and the names in `[tools] pass_env`. Anything that looks like a key, token, secret or password is left out unless named. Every `Command` in non-test code calls `env_clear()` and then adds `vars()`; `crates/anvil-core/tests/children_start_clean.rs` scans the source for any that does not.

- Read: `vars`.
- Run: `cargo test -p anvil-core a_child_process_that_prints_its_environment_shows_no_key`

## 12. Redaction and untrusted data

**`crates/anvil-types/src/redact.rs`, `crates/anvil-core/src/guard.rs`**

`Redactor` holds the built-in secret shapes, the values of secret-looking environment variables and the policy's patterns, and replaces each match with `[REDACTED:<kind>]`. `DataGuard` is the one choke point between a tool and the conversation: `ToolRegistry::execute` passes every result through `DataGuard::redact` before anything else, then `DataGuard::wrap` puts it inside `<untrusted-data id="NONCE">` delimiters, escaping anything in the content that looks like a marker. The provider and the transcript therefore receive the same text.

- Read: `DataGuard::wrap`, then `Redactor::redact`.
- Run: `cargo test -p anvil-core a_secret_in_tool_output_does_not_reach_the_provider_or_the_transcript`

## 13. MCP listing approval

**`crates/anvil-compat/src/mcp/approval.rs`**

Every session start lists each MCP server live. `McpStore::check` hashes the listing (names, titles, descriptions, input schemas, all annotations) together with a digest of the server's definition and compares it with the approved one in `~/.anvil/mcp-approvals.toml`, or with the policy's `listing_sha256` pin. A listing that changed stops the server, so a server cannot quietly turn a tool read-only (no prompt) or rewrite its description.

- Read: `McpStore::check`.
- Run: `cargo test -p anvil-compat a_changed_read_only_hint_is_detected`

## 14. What stays on disk

**`crates/anvil-core/src/context/session.rs`, `crates/anvil-types/src/private_fs.rs`**

`SessionLog` writes the transcript in the mode the policy sets; `SessionRecord::kept_as` turns every record into sizes and digests under `metadata`, and `off` writes no log at all. `private_fs` creates every file Anvil stores data in 0600 and every folder 0700 on Unix, and narrows existing ones.

- Read: `SessionRecord::kept_as`.
- Run: `cargo test -p anvil-core a_metadata_transcript_keeps_sizes_and_digests_but_no_text`

## 15. The audit log

**`crates/anvil-audit/src/audit/chain.rs`, `crates/anvil-audit/src/audit/log.rs`, `crates/anvil-cli/src/audit.rs`**

`seal` computes each record's SHA-256 over its canonical JSON, which includes the previous record's hash; `verify` re-serializes every line, requires the same bytes and checks the chain. `AuditLog::append` writes to the session file and the mirror and notices a file that something else removed or changed. `SessionAudit::open` opens the log before the first model request and, when the policy requires it, sets the gate that stops every later call after a failed write.

- Read: `seal` and `verify`, then `AuditLog::append`.
- Run: `cargo test -p anvil-audit the_audit_chain_detects_edit_removal_and_reorder`

## 16. Tests that guard the structure

These tests read the source tree or the repository rather than calling a function, so a structural rule cannot be broken by accident.

- `crates/anvil-core/tests/one_http_client.rs`: only `net.rs` builds an HTTP client (`cargo test -p anvil-core only_the_net_module_builds_an_http_client`).
- `crates/anvil-core/tests/children_start_clean.rs`: every program starts from a cleared environment (`cargo test -p anvil-core every_non_test_command_starts_from_a_cleared_environment`).
- `crates/anvil-core/tests/one_git_entry.rs`: every project git call goes through `git.rs` (`cargo test -p anvil-core only_the_git_module_starts_git_in_a_project`).
- `crates/anvil-types/tests/workflows.rs`: least-privilege, pinned workflows and the release assets (`cargo test -p anvil-types every_action_is_pinned_to_a_commit_sha`).
- `crates/anvil-types/tests/docs_match_code.rs`: the tests named in SECURITY_TESTS.md exist, and ADMIN_GUIDE.md lists every policy key (`cargo test -p anvil-types every_test_named_in_security_tests_exists`).

To see a hostile repository refused end to end, read `crates/anvil-cli/tests/hostile_repo.rs` and `crates/anvil-cli/src/wiring/hostile_tests.rs`; every policy key enforced is in `crates/anvil-cli/src/wiring/policy_tests.rs`.
