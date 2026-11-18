# Security notes

This page is for someone deciding whether anvil may run on their machines. It describes what the code in this repository does, with file references, so each claim can be checked. If you find a vulnerability, open a private security advisory on the GitHub repository rather than a public issue.

## In one paragraph

anvil is a single binary on the developer's machine. It never runs a model: it sends prompts, and the files and command output its tools read, over HTTPS to the model endpoints configured in the user's own `~/.anvil/config.toml`, and to nothing else unless the user turns something on. It has no telemetry upload, no self-update, no update check and no background process. A repository it runs in can only make it stricter, and the hooks, MCP servers, skills and instruction files a repository ships load only after the user trusts it. Its file tools stay inside the project and never open credential stores; the programs it starts do not inherit API keys. An administrator can install a managed policy that limits all of this further and that the user cannot override. **A shell command the user approves runs with the user's own rights; anvil does not sandbox it at the operating system level.**

## Who is responsible for what

- **anvil** is responsible for what it does on the developer's machine: which files its tools open, what the programs it starts inherit and may run, which servers it talks to, and what a cloned repository may change. Everything below is enforced in-process, in the same way on Windows, macOS and Linux.
- **The model host** (an internal model server, a rented GPU, or an API vendor) is responsible for that machine: its patching, network exposure, authentication, logging and retention of prompts, and who can read what it receives. anvil sends it the task, the instruction files, the files and command output the tools read, and the conversation so far. Treat everything a session reads as sent to the host.
- **The firm** decides who may run anvil, on which repositories, with which policy, and what wraps it (an operating system sandbox, a separate account, endpoint monitoring, review of every change through pull requests). `docs/enterprise/pilot-scope.md` is a template for a first pilot.

## What is enforced, and where it is tested

| control | code | test |
|---|---|---|
| Requests go only to configured endpoints, over https (loopback and named hosts aside), without redirects; under a policy only to `allowed_base_urls` | `crates/anvil-core/src/net.rs` | `net.rs` tests; `crates/anvil-cli/src/wiring/policy_tests.rs` |
| Only `net.rs` builds an HTTP client | `clippy.toml` `disallowed-methods` | `crates/anvil-core/tests/one_http_client.rs` |
| A key echoed by a server is redacted before it is shown, logged or stored | `send_attempt` in `crates/anvil-core/src/provider/http.rs` | `crates/anvil-cli/tests/doctor.rs` |
| A repository can only add `deny`, `ask` and `shell_programs`; its `.env` is never loaded | `crates/anvil-types/src/project.rs`, `crates/anvil-types/src/dotenv.rs` | `crates/anvil-cli/src/wiring/hostile_tests.rs`, `crates/anvil-cli/tests/hostile_repo.rs`, `crates/anvil-cli/tests/cli.rs` |
| Hooks, MCP servers, skills, commands, agents and instruction files of a repository load only once it is trusted | `crates/anvil-cli/src/trust.rs`, `Wiring::new_in` | the two hostile repository test files |
| Instruction files of a repository cannot `@import` files outside it | `crates/anvil-compat/src/instructions.rs` | `instructions.rs` tests, `hostile_tests.rs` |
| File tools stay in the workspace, reads included, and never open credential stores; symlinks and Windows junctions are resolved first | `crates/anvil-core/src/tools/workspace.rs`, `ToolContext::checked_path` | `crates/anvil-core/src/tools/mod.rs` tests, including `a_junction_that_points_outside_is_refused` |
| File tools never write inside a git directory: `.git` and everything under it (config, hooks, info), the `.git` file and git directory of a worktree or submodule, and bare or separate git directories | `inside_git_dir` in `crates/anvil-core/src/tools/workspace.rs` | `writes_to_git_config_and_hooks_are_refused`, `writes_to_worktree_and_submodule_git_directories_are_refused`, `writes_to_bare_and_separate_git_directories_are_refused` in `tools/mod.rs` |
| anvil's own git calls in a project run no hook, file system monitor, pager, editor, external diff, text conversion or remote command, and refuse a repository whose own config names a filter or signing program | `crates/anvil-core/src/git.rs` | `git.rs` tests; `crates/anvil-core/tests/one_git_entry.rs` keeps every project git call on that path |
| Programs anvil starts get an explicit environment without keys | `crates/anvil-types/src/child_env.rs` | `child_env.rs`, `shell.rs` and `hostile_repo.rs` tests |
| `shell_programs` limits `shell` to single listed programs without chaining or redirection | `Permissions::shell_refusal` | `permissions.rs` and `tools/mod.rs` tests |
| A detected check command (the repository's own code) is asked about like any shell call | `AgentLoop::project_check` in `crates/anvil-core/src/agent/mod.rs` | `a_detected_check_does_not_run_without_permission_for_shell` |
| A managed policy is a floor, and a broken one stops anvil | `crates/anvil-types/src/policy.rs`, `crates/anvil-cli/src/policy.rs`, `Wiring::with_policy` | `policy_tests.rs`, `policy.rs` tests, `main.rs` tests |

## What is not enforced

- **Approved shell commands are not sandboxed.** They run with the user's rights, can read any file the user can read and reach any host the user can reach. The file jail applies to anvil's file tools, not to programs a command starts. `shell_programs` narrows what may start, but an allowed program can still run code it is given (`cargo build` runs build scripts, `just` runs recipes, `npm test` runs scripts). Operating system sandboxing is on the roadmap (`docs/ROADMAP.md`) and can be added around anvil today.
- **Everything a session reads goes to the model host.** There is no redaction of secrets inside file contents or command output before they are sent or written to the transcript; the credential-store deny list keeps the file tools away from the usual places, but a secret in an ordinary source file is sent like any other text. Redaction is on the roadmap.
- **Trust is per repository path, not per content.** A trusted repository can change its hooks or instruction files later and the new versions load. Pinning with re-approval on change is on the roadmap.
- **The substring blocklist** (`rm -rf`, `Remove-Item -Recurse`, `git push --force`, `git reset --hard`) is a second layer and is easy to get around; it is not a control.
- **Binaries are not signed**, and there is no SBOM, `cargo audit` or `cargo deny` gate yet. See "Release builds".
- **The API key sits in the user's environment or `~/.anvil/.env`**, protected by file permissions, not by an operating system credential store.
- **Transcripts are plain files** without an append-only or hash-chained audit log.

## Network destinations

Every HTTP request anvil makes goes to a model endpoint, through a client built in one function, `http_client` in `crates/anvil-core/src/net.rs`. That client does not follow redirects, so a server cannot pass a request on to another host. Before a provider is used, `Egress::check` decides whether its `base_url` may be called: it must use https unless the host is loopback or listed in `[network] allow_http_hosts` in the user's own configuration, and under a managed policy it must lie under one of the policy's `allowed_base_urls` (same scheme, host and port, and a path at or below the allowed one). A refused endpoint gets no request, and the refusal is logged.

| destination | what triggers it | on by default |
|---|---|---|
| **Model providers**: each `[providers.<name>] base_url`. `anvil setup` writes `https://api.anthropic.com/v1`, `https://api.openai.com/v1` and `https://generativelanguage.googleapis.com/v1beta/openai`; `anvil setup --endpoint <url>` writes one provider for the given server; there is no built-in default (`crates/anvil-types/src/config.rs`). Requests go to `{base_url}/messages` or `{base_url}/chat/completions` (`crates/anvil-core/src/provider/`). | Every model turn of `anvil run`, `chat`, `resume` and `build`. Also, inside a session and without a separate prompt: context compaction; the end-of-session memory summarizer; and at session start the summarizer call that proposes rules from files the user edited after anvil wrote them (it sends a diff of those edits, at most 16 KiB). Under a managed policy the two summarizer calls are off unless the policy turns them on. `anvil memory learn` and `anvil ticket draft` call the summarizer too. `anvil doctor` sends a one-token request to every configured provider. | Yes, whenever those commands run. They are the product. |
| **The model list of an endpoint being set up**: `GET {url}/models` (and `{url}/v1/models` when the URL has no `/v1`), with the key as a bearer token when it is set (`crates/anvil-core/src/provider/models.rs`). Same egress rules as providers. | `anvil setup --endpoint <url>` only. | No. |
| **The ticket queue**: the git URL in `[tickets] queue`. A `git fetch` into `~/.anvil/queue.git`, then `git push queue refs/heads/tickets/<id>`, with the user's own git credentials (`crates/anvil-telemetry/src/git.rs`). | `anvil ticket submit` and the submit key of `anvil ticket review` only. A managed policy refuses it unless `[tickets] queue = true`. | No. Unset by default; while unset, `submit` says what to set and sends nothing. |
| **MCP servers**: local programs only. Definitions with a `url` are refused, so no MCP server makes anvil contact a host itself. Sources: `~/.anvil/mcp.toml` and, for a trusted repository, its `.mcp.json` and `.claude/settings*.json` (`crates/anvil-compat/src/mcp/mod.rs`). | Started when a session starts unless `.anvil/mcp-cache/` holds a fresh listing. Calling a tool asks first unless the server marks it read-only. Under a policy only listed commands start. | Only when such a file defines servers. |
| **Hooks**: shell commands from `~/.claude/settings.json`, `~/.anvil/settings.json` and, for a trusted repository, its `.claude/settings*.json` and `.anvil/settings.json` (`crates/anvil-compat/src/hooks.rs`). They can do anything a shell command can. | At session start and around tool calls once defined. Off under a policy unless `[extensions] hooks = true`. | Only when such a file defines hooks. |
| **The `shell` tool**: whatever command the model runs, once approved. | A model turn; asks first by default. | Asks. |

herdr reporting (`[integrations] herdr = true`) runs a local program and makes no network call. There is no web search or web fetch tool. The removed features (self-update, webhooks, remote MCP servers, downloading skills) are gone from the code; a configuration that still sets `[update]` or `[notify]` fails to load and says why.

## Files anvil writes

Nothing anvil records leaves the machine on its own. There is no upload code; the only way a record's content leaves is a ticket the user submits, and `anvil ticket draft` sends signals and usage records to the summarizer provider to write a draft.

| what | where | content |
|---|---|---|
| Session transcripts | `<project>/.anvil/sessions/<id>/`, or under a policy's `transcript_dir` (one subfolder per repository) | `session.jsonl` holds the full conversation, including file contents and command output the tools returned; `tool-output/` the untruncated outputs; `status.json` the session's state. Kept until deleted, or for the policy's `retention_days`. |
| Usage records | `<project>/.anvil/telemetry/<date>.jsonl` and a copy in `~/.anvil/telemetry/` | Content-free: provider, model, tokens, latency, retries, stop reasons, tool names with argument and result sizes and digests, permission decisions, check results by program name and command digest, provider error kinds. The project path is stored as a digest. The text of a `/ticket` note is kept after the ticket boundary check. |
| Signals | `<project>/.anvil/telemetry/signals.jsonl` | What the classifier derived from the records. |
| Logs | `<project>/.anvil/logs/` | Structured logs; `ANVIL_LOG` sets the level. Requests are logged by URL and attempt, never with headers. |
| Memory and rules | `~/.anvil/memory/`, `~/.anvil/rules.toml` | Facts and rules the user chose to keep. |
| Files anvil left | `~/.anvil/memory/corrections/<repo>.toml` | The text of files anvil changed at the end of a session (at most 32 KiB each, 128 KiB in all), to compare with later edits. |
| Trust decisions | `~/.anvil/trust.toml` | Each repository path the user answered the trust prompt for, and the answer. |
| Saved permission rules | `~/.anvil/permissions/<repository>.toml` | The rules an `always` answer saved. |
| Tickets | `~/.anvil/tickets/draft/`, `~/.anvil/tickets/open/`, `~/.anvil/queue.git` | Drafts, submitted tickets, and a bare clone of the queue made on the first submit. |
| MCP listings | `<project>/.anvil/mcp-cache/` | Tool lists the servers returned. |

The `.gitignore` of this repository keeps `.anvil/telemetry/`, `.anvil/sessions/`, `.anvil/logs/` and `.anvil/mcp-cache/` out of commits; other projects need the same lines.

## API keys

- Each provider names the environment variable that holds its key (`api_key_env`); the key is read from the process environment when the provider is built (`crates/anvil-core/src/provider/mod.rs`). Keys never appear in configuration files.
- At startup anvil fills the environment from `~/.anvil/.env` without overriding anything already set (`crates/anvil-types/src/dotenv.rs`). A repository's `.anvil/.env` is never loaded, so a repository cannot set a key, `ANVIL_HOME`, a proxy or any other variable.
- Provider definitions, and so the variable a key is read from and the URL it is sent to, come only from the user's own configuration or profile; a repository cannot name them.
- The key goes only to the endpoint of the provider that names it. It is not written to logs or transcripts, and an error body that repeats it is redacted.
- Programs anvil starts (shell commands, checks, hooks, MCP servers, rule checks) do not receive the key; see "What anvil's tools can reach".
- `anvil setup` writes `~/.anvil/.env` from placeholders and makes it private: mode 600 on Unix; on Windows it inspects the ACL with `icacls` and prints the command that removes a grant to other users (`crates/anvil-cli/src/setup.rs`).
- `anvil scan-keys` (part of `just check` and CI) fails when a tracked file holds something shaped like a key (`sk-`, `AIza`, `ghp_`, `github_pat_`, `hf_`) or any value of 12 or more characters from the user's `.env` files; it reports the file and line, never the value (`crates/anvil-cli/src/secrets.rs`).

## Permissions

- Every tool call is decided by the user's own rules: the `[permissions]` table of `~/.anvil/config.toml` or of a `--profile`, or else the user's file for the repository, `~/.anvil/permissions/<repository>.toml` (`crates/anvil-core/src/tools/permissions.rs`, `Wiring::user_permissions`). A managed policy can replace them with a forced profile; the user's `deny`, `ask` and `shell_programs` still apply on top. The repository adds its `deny`, `ask` and `shell_programs` from `.anvil/config.toml` and `.anvil/permissions.toml`; it cannot add `allow` rules or a `default`.
- A rule is `tool` or `tool:glob`; the glob matches the path for file tools and the command line for `shell`. A `deny` from any layer wins, then a repository or policy `ask`, then the user's `allow`, then the user's `ask`, then the `default`, then the tool's own default.
- Without rules, reading tools (`read_file`, `glob`, `grep`, `list_dir`, `outline`, `delegate`, `skill`, read-only MCP tools) run; `write_file`, `edit_file`, `shell` and every other MCP tool ask.
- `shell_programs`, when any layer sets it, limits a `shell` call to one program from the list, named as found on `PATH`, with plain arguments: `&&`, `||`, `;`, `|`, `&`, `>`, `<`, backticks, `$(`, `${`, `@(`, `(` and line breaks are refused, covering sh, PowerShell and cmd. No rule, answer or `--yes` lifts it, and each layer can only narrow the list (`Permissions::shell_refusal`). `profiles/strict.toml` ships it with `default = "ask"`; copy it to `~/.anvil/profiles/` and run with `--profile strict`, or have the policy force it.
- The substring blocklist (`rm -rf`, `Remove-Item -Recurse`, `git push --force`, `git reset --hard`) refuses those commands even under `--yes` unless an `allow` rule names them (`crates/anvil-core/src/tools/shell.rs`). It is a second layer, not the control.
- A prompt offers yes (this call), no, session (this tool for the rest of the session; for path tools only inside the project root, symlinks resolved) and, for `shell` only, always (saves `shell:<first word> *` to the user's own file for the repository, never into the repository).
- `--yes` answers every prompt with yes and approves plans; deny rules, `shell_programs` and the blocklist still apply. `--allow-tool <name>` and `permissions.allow_tools` pre-approve a tool for one session. Under a managed policy these and `--profile` are refused unless the policy allows them, and the policy can require a person to approve plans (see `require_plan_approval` below for exactly which).
- In the Plan state a tool that may write is refused. The Verify state runs the project's check: a command the user set in `[tools] check` runs as set; a command anvil detects from the repository's files (`just check`, `cargo test`, `npm test`, `python -m pytest`) runs the repository's own code, so it is asked about like any `shell` call.

## What anvil's tools can reach

- **Files.** Every file tool (`read_file`, `write_file`, `edit_file`, `glob`, `grep`, `list_dir`, `outline`) resolves its path, symlinks and `..` included, before opening it, and refuses anything outside the project root, the session folder and the folders listed in `[tools] extra_dirs`; installed skill folders may be read (`ToolContext::checked_path`, `crates/anvil-core/src/tools/workspace.rs`). Wherever they are, `.env` and `.env.*` files (not `.env.example`), `.ssh`, `.aws`, `.azure`, `.config/gcloud`, `.kube`, `.docker/config.json`, `.gnupg`, `.netrc`, `.git-credentials`, `~/.anvil/.env`, Chrome, Edge and Firefox profiles, and the macOS, Linux and Windows keychains and credential stores are refused, and directory searches skip them. Writes are also refused inside any git directory (`inside_git_dir`): a `.git` folder or file at any depth, which covers the git directories of worktrees and submodules under the main `.git`, and any folder git itself would take for a git directory because it holds `HEAD`, `objects` and `refs` (a bare repository, or one made with `--separate-git-dir`). A tool can therefore not plant a hook, change `.git/config` or `.git/info/attributes`, or repoint a worktree. Reading `.git` stays allowed.
- **Environment.** Programs anvil starts for the model or the repository (`shell` commands, check commands, hooks, MCP servers, rule checks) do not inherit anvil's environment. They receive `PATH`, the home, user and temp variables, the locale (`LANG`, `LC_*` without secret-looking names), `TERM`, `NO_COLOR`, the variables Windows needs to start programs (`SystemRoot`, `windir`, `ComSpec`, `PATHEXT`, `SystemDrive`, `HOMEDRIVE`, `HOMEPATH`, `APPDATA`, `LOCALAPPDATA`, `ProgramData`, the `ProgramFiles` and `CommonProgramFiles` folders, `NUMBER_OF_PROCESSORS`, `PROCESSOR_ARCHITECTURE`, `PROCESSOR_IDENTIFIER`, `OS`, `PSModulePath`), `CARGO_HOME` and `RUSTUP_HOME`, and the names listed in `[tools] pass_env` (`crates/anvil-types/src/child_env.rs`). Provider keys and tokens are not passed.
- **anvil's own git calls.** anvil runs git in the project itself to snapshot and diff a session's changes, to commit the items of `anvil build`, and to list changed files for `anvil rules check` and tracked files for `anvil scan-keys`. The model does not choose these commands, and they keep the user's full environment, because git needs the user's identity and settings. The repository's own config (`.git/config`, what it includes, `config.worktree`) is not the user's, so every one of these calls goes through `anvil_core::git::command` (`crates/anvil-core/src/git.rs`), which passes these as `-c` settings, which beat every config file:
  - `core.hooksPath=/dev/null`: no hook runs, including on build commits and checkouts (the user's own hooks in `.git/hooks` do not run on these commits either);
  - `core.fsmonitor=false`: no file system monitor program;
  - `--no-pager`, `core.editor=:` and `sequence.editor=:`: no pager or editor (git treats `:` as no editor);
  - `protocol.allow=never`: no remote is contacted, so `core.sshCommand`, credential helpers and proxies never run.

  Diffs also pass `--no-ext-diff --no-textconv`, so `diff.external` and the diff drivers named in `.gitattributes` never run. Aliases need nothing: git never lets an alias replace a built-in command, and anvil runs built-in commands only. Clean and smudge filters and commit signing programs cannot be switched off from the command line without also switching off the user's own (Git LFS, a signing key). So when the repository's own config names one (`filter.<name>.clean`, `smudge` or `process`, `gpg.program` or `gpg.<format>.program`), anvil refuses to run git there and names the key to remove; the same keys in the user's or the system's config are the user's choice and are used. This needs git 2.26 or later (`git config --show-scope`). A test fails the build if non-test code starts git in any other way.
- **The ticket queue's git calls** run in anvil's own bare cache, `~/.anvil/queue.git`, never in the project, and keep the full environment so the user's credential helper or SSH agent can push.
- **Not confined.** An approved `shell` command, see "What is not enforced".

## Running in a repository you do not trust

A repository you clone cannot change how anvil treats you:

- Its `.anvil/config.toml` and `.anvil/permissions.toml` may only add `deny`, `ask` and `shell_programs`. Every other key (providers, `base_url`, roles, limits, `[tools] shell` and `check`, `allow`, `default`, `allow_tools`) is ignored with a warning that names it (`crates/anvil-types/src/project.rs`).
- Its `.anvil/.env` is never loaded.
- Its hooks, MCP servers, skills, slash commands, agents and instruction files (`ANVIL.md`, `CLAUDE.md`, `.claude/CLAUDE.md`, `CLAUDE.local.md`, nested `CLAUDE.md` files) do not load until the user trusts the repository (`crates/anvil-cli/src/trust.rs`). The first interactive session lists every item, with the command line of each hook and MCP server, and asks once. The answer is kept in `~/.anvil/trust.toml`, keyed by the repository's full path. With `--plain`, `--yes`, no terminal, or a command that never prompts (`doctor`, `build`), an undecided repository is untrusted: nothing it ships loads and nothing is recorded. `anvil trust` shows the list and asks again; `anvil trust --revoke` withdraws trust. A policy with `[project] config = false` ignores the repository's `.anvil/` and everything it ships, whatever the user decided.
- Even in a trusted repository, an `@path` import in one of its instruction files may only name a file inside the repository, so a `CLAUDE.md` cannot pull `~/.ssh/...` into the prompt (`crates/anvil-compat/src/instructions.rs`).

## Managed policy

An administrator can install a policy file that the user cannot override (`crates/anvil-types/src/policy.rs`, `crates/anvil-cli/src/policy.rs`). `docs/enterprise/policy.example.toml` lists every key with its default.

- **Location.** `C:\ProgramData\Anvil\policy.toml` on Windows, `/Library/Application Support/Anvil/policy.toml` on macOS, `/etc/anvil/policy.toml` on Linux. The path is fixed in the code. No environment variable or flag points anvil at another file, and the Windows path is not read from `%ProgramData%`, which a user can change for their own processes. On Windows a non-administrator can create `C:\ProgramData\Anvil` while it does not exist; that user can only restrict themselves, but the administrator should create the folder with an access list that lets only administrators write.
- **Precedence.** The policy is a floor. The user's configuration and the repository can add restrictions and cannot remove one.
- **Failure.** A policy file that exists but cannot be read or is invalid stops anvil before it does anything else, with the reason. Without a policy file anvil runs in personal mode.
- **Defaults.** Only `[endpoints] allowed_base_urls` is required. Every other key defaults to the restrictive choice: with a policy installed, hooks, MCP servers, skills, herdr reporting, the ticket queue, the automatic summarizer calls and every flag that skips a prompt are off until the policy turns them on.
- **Keys.**
  - `[endpoints] allowed_base_urls`, `allow_http_hosts`: the only endpoints, and the only plain-http hosts.
  - `[project] config`: `false` ignores the repository's `.anvil/` and everything it ships.
  - `[permissions] allow_skip_prompts`: whether `--yes`, `--allow-tool`, `permissions.allow_tools` and `--profile` may be used; when not, the flags stop anvil with an error naming the policy and the configuration's list is ignored.
  - `[permissions] profile`: a profile in the policy's own folder whose `[permissions]` table replaces the user's rules.
  - `[permissions] require_plan_approval`: a person approves the plan of every `anvil run` and chat session, even under `--yes`. In `anvil build` it covers only the top-level build plan: once a person approves it, the sessions build runs for each item approve their own plans without asking. Those item sessions still ask before writes and shell commands unless rules or `--yes` allow them.
  - `[extensions] hooks`, `mcp_commands`, `skill_dirs`, `herdr`: what may load and run besides the built-in tools.
  - `[tickets] queue`: whether `anvil ticket submit` may push.
  - `[sessions] transcript_dir`, `retention_days`, `summarizer_calls`: where transcripts go, how long they are kept, and whether the automatic summarizer calls run.
- **Review.** `anvil doctor` prints the policy's path, its forced profile and transcript folder, and one line per thing it disables.
- **Where it is applied.** The keys that change a session are applied in one function, `Wiring::with_policy` in `crates/anvil-cli/src/wiring.rs`; the flags, `project.config`, the summarizer calls, the queue, plan approval and retention at startup in `crates/anvil-cli/src/main.rs`.

## Tickets

`anvil ticket draft` asks the summarizer provider to write drafts from telemetry signals and keeps them in `~/.anvil/tickets/draft/`. `anvil ticket submit` does nothing until `[tickets] queue` names a git repository; a stock install has no queue and never pushes anywhere. With a queue set, it validates the draft against the ticket boundary in `crates/anvil-types/src/boundary.rs` (no paths outside the repository, at most 40 lines of code in `repro` or `spec`), fetches the queue into `~/.anvil/queue.git`, and pushes the ticket as a `tickets/<id>` branch with the user's own git credentials. The current project's remotes are never used. The push carries only the ticket file, and only when the user runs the command.

## Release builds

- `.github/workflows/release.yml` runs on every push to `main`. It builds `anvil-cli` in release mode on GitHub-hosted runners for `x86_64-unknown-linux-gnu`, `x86_64-unknown-linux-musl` and `x86_64-pc-windows-msvc`, with the toolchain pinned in `rust-toolchain.toml`. A second job tags the commit `YYYY-MM-DD-<sha7>`, writes release notes with the changelog and a `## SHA256` section from `sha256sum`, and publishes the binaries as a GitHub release. It is the only workflow with write access (`contents: write`); `ci.yml` is read-only, and a test checks both.
- anvil has no self-update. A newer binary comes from whoever distributes anvil to the user.
- The hash and the binary come from the same GitHub release, so checking it proves the download is intact, not who built it. There is no signature, SBOM or dependency audit gate yet; a firm should build from source or take binaries from its own distribution channel until those exist (`docs/ROADMAP.md`).
