# Code tour: one request, start to finish

This walks through what happens when someone types `anvil "add a --dry-run flag"` in a repository, in the order the code runs. Each step names the file and the function, says in plain language what it does, and says why it is safe. Paths are relative to the repository root. The managed policy appears at every step where it changes something, so the same tour serves for personal and managed mode.

## 1. The command line

**`crates/anvil-cli/src/main.rs`: `main`, then `start`.**

`main` parses the arguments with clap (`crates/anvil-cli/src/cli.rs`) and calls `start`. Whatever goes wrong, it prints one error line and exits with code 1.

`start` does five things, in this order:

1. **Loads the managed policy**, `policy::load_system` in `crates/anvil-cli/src/policy.rs`, from the one fixed path `anvil_types::policy::system_path()`: `C:\ProgramData\Anvil\policy.toml` on Windows, `/etc/anvil/policy.toml` on Linux. No environment variable or flag can change that path. If the file exists but is broken, `start` returns the error and anvil stops here, before it touches anything. *Safe because a policy that cannot be read is never silently skipped.*
2. Moves directories left by the old name of the program (`legacy.rs`).
3. **Loads the user's `.env` file**, `anvil_types::dotenv::env_files` and `load`. Only `~/.anvil/.env` is loaded, never the repository's. *Safe because a cloned repository cannot set a key, `ANVIL_HOME` or a proxy.*
4. Picks the full-screen UI or the plain line renderer (`UiKind::choose` in `frontend.rs`).
5. Starts logging into `.anvil/logs/` and the async runtime, then calls `dispatch`.

**`dispatch`** first calls **`policy::check_flags`**. Under a policy that does not allow skipping prompts, `--yes`, `--allow-tool` and `--profile` stop anvil here with an error naming the policy file. Then it matches the subcommand; a bare task goes to `run`, which calls `project_wiring`.

## 2. Configuration, the repository, and trust

**`crates/anvil-cli/src/main.rs`: `project_wiring`.**

1. **The user's configuration**, `anvil_types::config::load_profile` in `crates/anvil-types/src/config.rs`. It reads `~/.anvil/config.toml` and, with `--profile`, `~/.anvil/profiles/<name>.toml`. It never reads the repository's config. A section that was removed (`[update]`, `[notify]`) fails with a sentence saying why. *Safe because providers, URLs, keys and settings only ever come from the user's own files.*
2. **The repository's rules**, `anvil_types::project::load` in `crates/anvil-types/src/project.rs`. From the repository's `.anvil/config.toml` and `.anvil/permissions.toml` it keeps only `deny`, `ask` and `shell_programs`; everything else becomes a warning line naming the key. Under a policy with `[project] config = false` this step is skipped entirely.
3. **Trust**, `trust::decide` in `crates/anvil-cli/src/trust.rs`. It looks the repository up in `~/.anvil/trust.toml`. If the user never decided and a person is at the terminal, `shipped` lists everything the repository would load (each hook and MCP server with its command line) and asks once; the answer is saved. With `--plain`, `--yes` or no terminal, an undecided repository stays untrusted and nothing is recorded. *Safe because nothing the repository ships can run or steer the model before a person has seen the list and said yes.*
4. **The wiring**, `Wiring::new_in` in `crates/anvil-cli/src/wiring.rs`. This is the one place skills, commands, agents, hooks and MCP servers are discovered. It passes the repository root to each discovery only when the repository is trusted, otherwise `None`, so only the user's own files under `~` are found. It also records the user's `[tools] pass_env` for child processes and builds the personal-mode `Egress` (https only, plus the user's `allow_http_hosts`).
5. **The policy**, `Wiring::with_policy`, when one is installed. This one function is every effect the policy has on a session, and it reads top to bottom: the egress becomes the policy's allowed endpoints, hooks are dropped unless allowed, MCP servers are filtered to the allowed commands, skills come only from the policy's folders, herdr is turned off unless allowed, and the configuration's `allow_tools` stops counting. Later, `Wiring::user_permissions` swaps in the policy's forced profile.

Back in `run`, `prune_old_sessions` deletes this repository's session folders older than the policy's `retention_days`, and `Wiring::load_mcp` starts the remaining MCP servers.

## 3. The session

**`crates/anvil-cli/src/commands.rs`: `run_task`, then `run_session_in`.**

`run_task` creates the session folder with `Wiring::new_session_dir`, which puts it under `Wiring::sessions_root()`: the policy's `transcript_dir` or the repository's `.anvil/sessions`. `run_session_in` then:

- starts the front end;
- runs `learn::at_session_start`, the one automatic summarizer call at the start, only if `Wiring::summarizer_calls_allowed` says so (always in personal mode, only when the policy turns it on);
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

Every message is appended to the transcript by `push_message`, through `SessionLog::append` in `crates/anvil-core/src/context/session.rs`.

## 5. The permission check

**`crates/anvil-core/src/tools/mod.rs`: `ToolRegistry::execute`, then `check_permission`.**
**`crates/anvil-core/src/tools/permissions.rs`: `Permissions`.**

The rules were assembled in `Wiring::registry_without_delegate`: the user's own rules (`Wiring::user_permissions`, or the policy's forced profile with the user's denies on top), then `Permissions::tighten` with the repository's `deny`, `ask` and `shell_programs`. Tightening rules are kept apart from the user's, so they can only make a call stricter.

For each call, `check_permission` asks in this order:

1. **`Permissions::shell_refusal`** (shell only). If any layer set `shell_programs`, the command must be one listed program with plain arguments; `&&`, `;`, `|`, `>`, `$(`, backticks, PowerShell's `(` and the rest are refused. Nothing below can lift this.
2. **The blocklist** (`rm -rf` and friends), a second layer an explicit `allow` rule can lift.
3. **The rules**: a `deny` from anywhere, then a tightening `ask`, then the user's `allow` and `ask`, then the default, then the tool's own default (reads run, writes ask).
4. **Session approvals and the prompt**, for calls that ask. An `always` answer is saved to the user's `~/.anvil/permissions/<repository>.toml`, never into the repository.

*Safe because the order is fixed, every layer below the user can only tighten, and the strongest refusals come first.*

## 6. Running the tool

**File tools: `ToolContext::checked_path` in `crates/anvil-core/src/tools/mod.rs`, and `crates/anvil-core/src/tools/workspace.rs`.**

Every file tool calls `checked_path` before it opens anything. It resolves the path the way the file system will, symlinks and `..` included (`permissions::resolve_path`). The result must not hold secrets (`workspace::holds_secrets`: `.env`, `.ssh`, cloud credentials, browser profiles, keychains), and it must lie inside the project, the session folder or a `[tools] extra_dirs` folder; skill folders may be read. Directory walks (`glob::walk`) skip the same secret paths. *Safe because a crafted path, a link or a `../` cannot reach outside, and credential stores are refused even inside the workspace.*

**Shell: `shell::run_command` in `crates/anvil-core/src/tools/shell.rs`.**

The command runs through the configured shell with `env_clear()` followed by `anvil_types::child_env::vars()`: `PATH`, the home and temp folders, the locale, what Windows needs to start programs, the Rust toolchain folders, and the user's `pass_env`. Hooks, MCP servers and rule checks start their programs the same way. *Safe because a command cannot read the provider key or a token from its environment. Not a sandbox: an approved command still runs with the user's rights.*

## 7. The provider call

**`crates/anvil-core/src/provider/mod.rs`: `from_config`, `build`.**
**`crates/anvil-core/src/net.rs`: `Egress::check`, `http_client`.**
**`crates/anvil-core/src/provider/openai.rs`, `anthropic.rs`, `http.rs`.**

When the wiring builds a provider (`Wiring::provider`), `from_config` first calls **`Egress::check`** on the provider's `base_url`. It must use https unless the host is loopback or named, and under a policy it must lie under an allowed base URL. A refused endpoint never sees a request. Then `read_api_key` reads the key from the environment, and `build` creates the client with **`net::http_client`**, the only function allowed to build one (`clippy.toml` and `crates/anvil-core/tests/one_http_client.rs` enforce that). That client does not follow redirects.

`OpenAiProvider::complete` (or the Anthropic one) posts the request and hands the response to `http::start`, which retries connection failures and 5xx responses, applies the deadline, and decodes the server-sent events into text and tool calls. When the server answers with an error, `http::send_attempt` replaces any copy of the key in the error body with `[redacted]` before the error goes anywhere. The provider is wrapped in `Budgeted`, which stops the session at its token budget.

*Safe because there is one way out of the process, it is checked before use, it cannot be redirected, and the key cannot leak through an error message.*

## 8. The transcript

**`crates/anvil-core/src/context/session.rs`: `SessionLog`, `session_dir`, `load_session`, `list_sessions`.**

`SessionLog::open` creates `session.jsonl` in the session folder chosen in step 3, and `append` writes one JSON line per record: the task, each message, state changes, the final status. Full tool output goes to `tool-output/` next to it, and `status.json` keeps the session's state for `anvil status`. `anvil resume`, `anvil sessions` and `anvil status` read from the same `sessions_root`, so they find the sessions wherever the policy put them.

At the end, `memory::finish_session` runs the end-of-session summarizer only if `Wiring::summarizer_calls_allowed`, and the Stop hooks run if any are loaded.

## Where to look when demoing

- Every effect of the policy on a session: `Wiring::with_policy` in `crates/anvil-cli/src/wiring.rs`.
- Every attack a repository could try, refused: `crates/anvil-cli/src/wiring/hostile_tests.rs` and `crates/anvil-cli/tests/hostile_repo.rs`.
- Every policy key enforced: `crates/anvil-cli/src/wiring/policy_tests.rs`.
- The one HTTP client: `crates/anvil-core/src/net.rs`.
- What the reviewer should read first: SECURITY.md, then `docs/enterprise/`.
