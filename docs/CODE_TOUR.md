# Code tour: one request, start to finish

This walks through what happens when someone types `anvil "add a --dry-run flag"` in a repository, in the order the code runs. Each step names the file and the function, says in plain language what it does, and says why it is safe. Paths are relative to the repository root. The managed policy appears at every step where it changes something, so the same tour serves for personal and managed mode.

## 1. The command line

**`crates/anvil-cli/src/main.rs`: `main`, then `start`.**

`main` parses the arguments with clap (`crates/anvil-cli/src/cli.rs`) and calls `start`. Whatever goes wrong, it prints one error line and exits with code 1.

`start` does four things, in this order:

1. **Loads the managed policy**, `policy::load_system` in `crates/anvil-cli/src/policy.rs`, from the one fixed path `anvil_types::policy::system_path()`: `C:\ProgramData\anvil\policy.toml` on Windows, `/etc/anvil/policy.toml` on Linux and macOS. No environment variable or flag can change that path. `anvil_types::policy::load_checked` first runs `ownership::check` (root-owned and not group or other writable on Unix; only Administrators, SYSTEM or TrustedInstaller may own or change it on Windows), then parses the file. If the file exists but is unsafe or broken, `start` returns the error and anvil stops here, before it touches anything. *Safe because a policy that cannot be trusted or read is never silently skipped.*
2. **Loads the user's `.env` file**, `anvil_types::dotenv::env_files` and `load`. Only `~/.anvil/.env` is loaded, never the repository's. *Safe because a cloned repository cannot set a key, `ANVIL_HOME` or a proxy.*
3. Picks the full-screen UI or the plain line renderer (`UiKind::choose` in `frontend.rs`).
4. Starts logging into `.anvil/logs/` and the async runtime, then calls `dispatch`.

**`dispatch`** first calls **`policy::check_flags`**. Under a policy with `allow_yes` or `allow_allow_tool` off, `--yes` or `--allow-tool` stops anvil here with an error naming the policy key. Then it matches the subcommand; a bare task goes to `run`, which calls `project_wiring`.

## 2. Configuration, the repository, and trust

**`crates/anvil-cli/src/main.rs`: `project_wiring`.**

1. **The user's configuration**, `anvil_types::config::load_profile` in `crates/anvil-types/src/config.rs`. It reads `~/.anvil/config.toml` and, with `--profile`, `~/.anvil/profiles/<name>.toml`. It never reads the repository's config. A section that was removed (`[update]`, `[notify]`, `[tickets]`, `[integrations]`, the old table of ticket signal thresholds) fails with a sentence saying why. *Safe because providers, URLs, keys and settings only ever come from the user's own files.*
2. **The repository's rules**, `anvil_types::project::load` in `crates/anvil-types/src/project.rs`. From the repository's `.anvil/config.toml` and `.anvil/permissions.toml` it keeps `deny`, `ask` and `shell_programs` (plus `allow` from `permissions.toml`, used only once the repository is trusted); a key that would loosen anvil (providers, roles, `[tools]`, `[network]`, `[shell]`, `allow` in `config.toml`, `default`, `allow_tools`) is an error naming it, and any other key becomes a warning line. These rules always apply; under a policy, whether the repository can be trusted at all is `[projects] allow_trust`.
3. **Trust**, `trust::decide` in `crates/anvil-cli/src/trust.rs`. It looks the repository up in `~/.anvil/trust.toml`, where a decision to trust is stored with `fingerprint`, a SHA-256 digest of everything the repository ships; a changed digest counts as undecided. When the policy forbids trust (`allow_trust` false) it returns untrusted without asking. If the user never decided and a person is at the terminal, `shipped` lists everything the repository would load (each hook and MCP server with its command line) and asks once; the answer is saved. With `--plain`, `--yes` or no terminal, an undecided repository stays untrusted and nothing is recorded. *Safe because nothing the repository ships can run or steer the model before a person has seen the list and said yes.*
4. **The wiring**, `Wiring::new_in` in `crates/anvil-cli/src/wiring.rs`. This is the one place skills, commands, agents, hooks and MCP servers are discovered. It passes the repository root to each discovery only when the repository is trusted, otherwise `None`, so only the user's own files under `~` are found. It also records the user's `[tools] pass_env` for child processes, builds the unmanaged-mode `Egress` (only the hosts of the user's providers and HTTP MCP servers it found, https except loopback and the user's `allow_http_hosts`), and resolves the user's `[shell]` table into `ShellSettings` (`crates/anvil-cli/src/wiring/shell.rs`; `ask` on the host by default).
5. **The policy**, `Wiring::with_policy`, when one is installed. Before the wiring is built, `policy::check_config` refuses a user configuration or profile that conflicts with the policy (a provider host, provider or model the policy does not allow, plain-http hosts, `extra_dirs`, `allow_tools`, an allow rule the policy's `deny` or `ask` covers, a token limit above the ceiling, a looser `[shell] mode`, another executor, a user container, a `[tools] check` the shell mode would not run), naming the key. `with_policy` then reads top to bottom: the egress becomes the policy's `[network]` table (`Egress::from_policy`: its `host:port` list, `require_tls`, CA bundle, client certificate and proxy mode); hooks, MCP servers and skills the policy forbids (an HTTP MCP server also when its host is not allowed) are an error when the user's own files under `~/.anvil` or `~/.claude` define them and are left out with a note when a repository ships them; saved `always` rules are checked; the token budget is capped; the `[files] deny` globs are compiled; `ShellSettings::resolve` combines the user's `[shell]` with the policy's (the stricter mode, every allowlist, the policy's executor). Later, `Wiring::registry_without_delegate` adds the policy's `deny` and `ask` rules, its `allow_always` switch and the shell's allowlist layers, and removes `shell` when the mode is `off`; `Wiring::tool_context` adds its `[files]` settings and the shell's runner (direct start, container); subagents use the same two functions.

Back in `run`, `before_session` prints `policy: unmanaged` or the policy's path and hash (line renderer only; the full-screen UI shows it on its startup screen), deletes this repository's session folders older than the policy's `retention_days`, and `Wiring::load_mcp` starts the remaining MCP servers. Before that, `project_wiring` in `main.rs` calls `Wiring::check_endpoints`, which runs every provider `base_url` and HTTP MCP `url` through the egress rules, so a refused endpoint stops anvil at startup. `anvil doctor --egress` (`crates/anvil-cli/src/doctor/egress.rs`) prints the same rules as a list of destinations.

## 3. The session

**`crates/anvil-cli/src/commands.rs`: `run_task`, then `run_session_in`.**

`run_task` creates the session folder with `Wiring::new_session_dir`, which puts it under `Wiring::sessions_root()`: the policy's `transcript_dir` or the repository's `.anvil/sessions`. `run_session_in` then:

- opens the audit log with `SessionUsage::start`, which calls `SessionAudit::open` in `crates/anvil-cli/src/audit.rs`: `audit.jsonl` in the session folder and the policy's mirror, before anything else, so a required log that cannot be written stops the session here. `observe_permissions` then hands the wiring the permission observer, the tap subagents report to, and, when the log is required, the `CallGate` that every provider and registry the wiring builds checks before it works;
- starts the front end;
- builds the tool registry with `Wiring::registry` (step 5);
- builds the loop settings with `Wiring::loop_config`, including whether the repository's instruction files may load (`trusted_project`);
- runs the `SessionStart` hooks, of which there are none unless the user's or a trusted repository's settings define them and the policy allows hooks;
- builds the plan gate: under `--yes` plans are approved automatically, except when `Wiring::plans_need_a_person` says the policy requires a person;
- creates `AgentLoop::new` and calls `AgentLoop::run`.

## 4. The agent loop

**`crates/anvil-core/src/agent/mod.rs`: `AgentLoop::run`, `run_model_state`, `run_tools`.**

The loop is a small state machine: Plan, Build, Verify, Review. In each state `run_model_state`:

1. builds the system prompt with `system_prompt_for`. Instruction files come from `crate::context::discover_instructions` (`crates/anvil-core/src/context/instructions.rs`), which reads the repository's `ANVIL.md`, `CLAUDE.md` and nested `CLAUDE.md` only for a trusted repository. The extra files and `@imports` come from `anvil_compat::instructions::discover_extra`, which refuses an import from a repository file that points outside the repository, so a `CLAUDE.md` cannot pull `~/.ssh` into the prompt;
2. compacts the conversation if it is close to the context window;
3. sends the request with `turn::complete_turn` (step 7);
4. hands the model's tool calls to `run_tools`.

In Plan, `run_tools` refuses any call that may write. In Verify, `AgentLoop::project_check` decides the check command once: a command the user configured runs as configured, but one detected from the repository's files (`just check`, `npm test`) is the repository's own code, so it goes through the permission rules like a shell call.

Every message is appended to the transcript by `push_message`, through `SessionLog::append` in `crates/anvil-core/src/context/session.rs`. `push_message` first redacts user text (the task, feedback with check output) through `ctx.guard`; the reviewer's brief is redacted the same way, with the diff and check output inside untrusted-data delimiters.

**Redaction and delimiters: `crates/anvil-core/src/guard.rs`, `crates/anvil-types/src/redact.rs`.** After a tool runs, `ToolRegistry::execute` passes its output through `DataGuard::redact` before anything else (truncation, the `tool-output/` file, the PostToolUse hook), then wraps it with `DataGuard::wrap` in `<untrusted-data id="NONCE" source="tool">` delimiters; hook text is redacted and wrapped on its own. The `Redactor` holds the built-in key shapes, the values of secret-looking environment variables and the policy's `[data] patterns`; `Wiring::new_in` and `Wiring::with_policy` build it once per session, and `Wiring::tool_context` hands the same guard, and so the same nonce, to the session and its subagents. *Safe because there is one choke point between a tool and the conversation, and what the provider receives is what the transcript stores.*

## 5. The permission check

**`crates/anvil-core/src/tools/mod.rs`: `ToolRegistry::execute`, then `check_permission`.**
**`crates/anvil-core/src/tools/permissions.rs`: `Permissions`.**

The rules were assembled in `Wiring::registry_without_delegate`: the user's own rules (`Wiring::user_permissions`), then `Permissions::tighten` with the repository's `deny`, `ask` and `shell_programs`, then with the policy's `deny` and `ask`, then `Permissions::limit_shell` with each `[shell] allow` list. Tightening rules are kept apart from the user's, so they can only make a call stricter.

For each call, `check_permission` asks in this order:

1. **`Permissions::shell_refusal`** (shell only). If any layer set an allowlist (`[shell] allow` in allowlist mode, or `shell_programs`), the command is split by `anvil_types::shell::split_command`, which refuses `;`, `&`, `|`, `<`, `>`, backticks, `$(`, parentheses, braces and line breaks, and its words must match a pattern of every layer. Then `shell::argument_refusal` checks the words a pattern's `*` left to the model: no argument may name a path outside the project, and git may take no option before its subcommand and none of the options that write a file, read outside the repository or run a program (`crates/anvil-core/src/tools/allowlist_tests.rs`). Nothing below can lift this.
2. **The deny categories** (`Tool::blocked_reason`, `deny_category` in `shell.rs`): network clients, git commands that reach or change remotes, package installs, privilege tools, `rm -rf` and friends. Only `Permissions::allows_by_name`, a rule or allowlist pattern whose text names the command, lifts one. This matches text, so it is a guard rail.
3. **The rules**: a `deny` from anywhere, then a tightening `ask`, then the user's `allow` and `ask`, then the default, then the tool's own default (reads run, writes ask).
4. **Session approvals and the prompt**, for calls that ask. An `always` answer is saved to the user's `~/.anvil/permissions/<repository id>.toml` (see `trust::repo_id`), never into the repository.

*Safe because the order is fixed, every layer below the user can only tighten, and the strongest refusals come first.*

## 6. Running the tool

**File tools: `ToolContext::checked_path` in `crates/anvil-core/src/tools/mod.rs`, and `crates/anvil-core/src/tools/workspace.rs`.**

Every file tool calls `checked_path` before it opens anything. It resolves the path the way the file system will, symlinks, `..`, junctions, drive and UNC prefixes and letter case included (`permissions::resolve_path`), after refusing any spelling Windows would fold onto another name (`workspace::win32_fold`: a trailing dot or space, or `:`), for reads as well as writes. The result must not hold secrets (`workspace::holds_secrets`: `.env`, key and certificate files, `credentials*`, `.npmrc`, `.pypirc`, `.git/config`, `.ssh`, cloud credentials, browser profiles, keychains, plus the policy's `[files] deny`), and it must lie inside the project, the session folder or a `[tools] extra_dirs` folder; skill folders and the policy's `extra_roots` may be read. Directory walks (`glob::walk`) and `list_dir` skip the same secret paths. `crates/anvil-core/src/tools/boundary_tests.rs` tries every tool against every spelling. *Safe because a crafted path, a link or a `../` cannot reach outside, and credential stores are refused even inside the workspace.*

**Shell: `shell::run_command` in `crates/anvil-core/src/tools/shell.rs`.**

`registry.execute` forces `Runner::direct` for the shell tool whenever the rules hold an allowlist, so what was matched is what starts. `prepare` then builds the command: in direct mode it runs `argument_refusal` again, then starts the words from `split_command` with no shell, git through `git::allowlisted` (anvil's safe git settings, diff programs off, and a refusal when the repository's config defines aliases or programs or the subcommand is an alias) (on Windows the program is looked up on the passed `PATH` with `PATHEXT`); otherwise the configured shell; with a container, the runtime program with `container_args` (no network, the workspace as the only mount with its `.git` read-only, read-only root, non-root user, no capabilities, limits). In every case `env_clear()` comes first, followed by `anvil_types::child_env::vars()`: `PATH`, the home and temp folders, the locale, what Windows needs to start programs, the Rust toolchain folders, and the user's `pass_env`. Checks, `anvil build` acceptance commands, hooks, MCP servers, rule checks and anvil's git calls (`git::command`, plus `GIT_VARS`) start their programs from the same list. *Safe because a command cannot read the provider key or a token from its environment, and in allowlist mode no shell can chain a second program. Not a sandbox on the host: an approved command still runs with the user's rights; the container executor is the sandbox.*

## 7. The provider call

**`crates/anvil-core/src/provider/mod.rs`: `from_config`, `build`.**
**`crates/anvil-core/src/net.rs`: `Egress`, `http_client`, `HttpRequest::send`.**
**`crates/anvil-core/src/provider/openai.rs`, `anthropic.rs`, `http.rs`.**

When the wiring builds a provider (`Wiring::provider`), `from_config` first calls **`Egress::check`** on the provider's `base_url`, so a refused endpoint fails before the key is read. Then `read_api_key` reads the key from the environment, and `build` creates the client with **`net::http_client`**, the only function allowed to build one (`clippy.toml` and `crates/anvil-core/tests/one_http_client.rs` enforce that). It turns redirects off, applies the CA bundle, client certificate and proxy mode of the `Egress` (no proxy unless the policy names one), and returns an `HttpClient` that never hands out the reqwest client inside it.

`OpenAiProvider::complete` (or the Anthropic one) prepares the request with `client.post(url)` and adds the key with `api_key(header, value, base_url)`. **`HttpRequest::send`** builds the request, runs `Egress::check` on its final URL (the `host:port` must be allowed, plain http only to loopback unless a rule says otherwise), and only then adds the key, and only when the URL's `host:port` is that of the configured `base_url`. A refused request opens no connection. HTTP MCP servers (`crates/anvil-compat/src/mcp/http.rs`) send through the same client, with the headers their definition sets bound to their own URL. `send_attempt` hands the response to `http::start`, which retries connection failures and 5xx responses, applies the deadline, and decodes the server-sent events into text and tool calls. When the server answers with an error, `http::send_attempt` replaces any copy of the key in the error body with `[redacted]` before the error goes anywhere. The provider is wrapped in `Budgeted`, which stops the session at its token budget.

*Safe because there is one way out of the process, every request is checked when it is sent, it cannot be redirected, the key goes only to its own host, and the key cannot leak through an error message.*

## 8. The transcript

**`crates/anvil-core/src/context/session.rs`: `SessionLog`, `session_dir`, `load_session`, `list_sessions`.**

`AgentLoop::new` opens the log in the policy's transcript mode (`ToolContext::transcript`): none at all under `off`, and under `metadata` `SessionRecord::kept_as` replaces the task and every message with sizes and SHA-256 digests before `append` writes it. `append` writes one JSON line per record: the task, each message, each completion's tokens and timing, redaction counts, state changes, the final status. Every file and folder is created through `anvil_types::private_fs`, owner-only on Unix. Full tool output goes to `tool-output/` next to it, and `status.json` keeps the session's state for `anvil status`. `anvil resume`, `anvil sessions` and `anvil status` read from the same `sessions_root`, so they find the sessions wherever the policy put them.

At the end, `memory::finish_session` runs the end-of-session summarizer only if `Wiring::memory_allowed` and the transcript is `full`, and the Stop hooks run if any are loaded.

MCP servers are loaded before the session in `Wiring::load_mcp`, through `McpTools::load` and `McpStore` (`crates/anvil-compat/src/mcp/approval.rs`): every server is listed live, the listing is compared with the policy's `listing_sha256` pin or the approved SHA-256 in `~/.anvil/mcp-approvals.toml` (recorded with a digest of the definition), and a changed one drops the server. There is no tool cache.

## 9. The audit log

**`crates/anvil-cli/src/usage.rs` (`AuditTap`, `AuditedGate`), `crates/anvil-cli/src/audit.rs`, `crates/anvil-audit/src/audit/`.**

The loop's `EventSink` carries a tap (`EventTap` in `crates/anvil-core/src/agent/events.rs`) that sees every event on the thread that emits it, before it is queued for the front end. `AuditTap` turns model completions, tool calls, tool results and file changes (`LoopEvent::FileChanged`, from `write_file` and `edit_file`) into records; the permission observer adds each decision with the `DecisionSource` that `ToolRegistry::check_permission` reports, `AuditedGate` adds plan answers, and `SessionUsage::prompt` adds prompts. A subagent's sink comes from `SubagentEnv::events`, so its records land in the same chain under its name. `AuditLog::append` seals each record (`chain::seal`) and writes it to the session file and the mirror; a failed write under `[audit] required` sets the gate, and `drive` in `commands.rs` stops the loop. `anvil audit verify` and `anvil audit show` read a file back through `chain::verify` and `show::render`. `verify` re-serializes every record and requires the same bytes; `--mirror` also compares the mirrored copy. `AuditLog::open` seeds a missing mirror with the session file and refuses one that differs, and each append first checks that the file is still the one it opened. A build records its acceptance decisions in its own log through `Env::open_build_log` and `Wiring::permit_shell`.

## Where to look when demoing

- Every effect of the policy on a session: `Wiring::with_policy` in `crates/anvil-cli/src/wiring.rs`.
- Every attack a repository could try, refused: `crates/anvil-cli/src/wiring/hostile_tests.rs` and `crates/anvil-cli/tests/hostile_repo.rs`.
- Every policy key enforced: `crates/anvil-cli/src/wiring/policy_tests.rs`.
- The one HTTP client: `crates/anvil-core/src/net.rs`.
- What the reviewer should read first: SECURITY.md, then `docs/enterprise/`.
