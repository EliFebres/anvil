# Security notes

This page is for someone deciding whether anvil may run on their machines. It describes what the code in this repository does, with file references, so each claim can be checked. If you find a vulnerability, open a private security advisory on the GitHub repository rather than a public issue.

## In one paragraph

anvil is a single binary that sends your prompts, and the files and command output its tools read, to the model providers you configure, and to nothing else unless you turn something on. It has no telemetry upload, no self-update, no update check and no background process. It keeps transcripts, logs and content-free usage records as plain files in the project's `.anvil/` folder and in `~/.anvil/`. API keys are read from the environment and your own `~/.anvil/.env`, never from configuration files or from the repository. File writes and shell commands ask first unless one of your own permission rules says otherwise. **anvil does not trust the repository it runs in**: a repository can only add rules that make anvil stricter, and the hooks, MCP servers, skills and instruction files it ships load only after you trust it (see "Running in a repository you do not trust").

## Network destinations

Every HTTP request anvil makes is a request to a model provider, and every one is sent by a client built in one function, `http_client` in `crates/anvil-core/src/net.rs`. `clippy.toml` forbids building an HTTP client anywhere else, and `crates/anvil-core/tests/one_http_client.rs` fails if any file switches that check off. The client does not follow redirects, so a server cannot pass a request on to another host. Before a provider is used, `Egress::check` decides whether its `base_url` may be called: it must use https, unless the host is loopback or is listed in `[network] allow_http_hosts` in your own `~/.anvil/config.toml`. A refused endpoint gets no request, and the refusal is logged.

| destination | what triggers it | on by default |
|---|---|---|
| **Model providers**: each `[providers.<name>] base_url`. `anvil setup` writes `https://api.anthropic.com/v1`, `https://api.openai.com/v1` and `https://generativelanguage.googleapis.com/v1beta/openai`; there is no built-in default (`crates/anvil-types/src/config.rs`). Requests go to `{base_url}/messages` or `{base_url}/chat/completions` (`crates/anvil-core/src/provider/`). | Every model turn of `anvil run`, `chat`, `resume` and `build`. Also, inside a session and without a separate prompt: context compaction, the end-of-session memory summarizer, and at session start the summarizer call that proposes rules from files you edited after anvil wrote them (it sends a diff of those edits, at most 16 KiB). `anvil memory learn` and `anvil ticket draft` call the summarizer too. `anvil doctor` sends a one-token request to every configured provider. | Yes, whenever those commands run. They are the product. |
| **MCP servers** from `~/.anvil/mcp.toml` and, for a repository you trust, its `.mcp.json`, `.claude/settings.json` and `.claude/settings.local.json` (`crates/anvil-compat/src/mcp/mod.rs`). Only stdio servers are supported: a local program anvil starts. A definition with a `url` is refused, so no MCP server makes anvil contact a host. | Every server is started when a session starts (`run`, `chat`, `resume`, `build`), unless `.anvil/mcp-cache/` holds a fresh listing for it. Calling a server's tool asks first unless the server marks the tool read-only. | Only when such a file defines servers; a repository's servers only once you trust it. |
| **The ticket queue**: the git URL in `[tickets] queue`. A `git fetch` into the cache `~/.anvil/queue.git`, then `git push queue refs/heads/tickets/<id>`, with your own git credentials (`crates/anvil-telemetry/src/git.rs`). | `anvil ticket submit` and the submit key of `anvil ticket review` only. | No. The key is unset by default, and while it is unset `submit` says what to set and sends nothing. See "Tickets". |
| **Hooks**: shell commands from the `hooks` section of `~/.claude/settings.json` and `~/.anvil/settings.json` and, for a repository you trust, its `.claude/settings.json`, `.claude/settings.local.json` and `.anvil/settings.json` (`crates/anvil-compat/src/hooks.rs`). They can do anything, including network calls. | Automatic at session start and around tool calls once defined. | Only when such a file defines hooks; a repository's hooks only once you trust it. |
| **The `shell` tool**: whatever command the model runs. | A model turn; asks first by default. | Asks. |

herdr reporting (`[integrations] herdr = true`) runs a local program and makes no network call. There is no web search or web fetch tool.

## Telemetry and other files anvil writes

Nothing anvil records leaves the machine on its own. There is no upload code; the only way a record's content leaves is a ticket you submit yourself, and `anvil ticket draft` sends signals and usage records to your summarizer provider to write a draft.

| what | where | content |
|---|---|---|
| Usage records | `<project>/.anvil/telemetry/<date>.jsonl` and a copy in `~/.anvil/telemetry/` | Content-free: provider, model, tokens, latency, retries, stop reasons, tool names with argument and result sizes and digests, permission decisions, check results by program name and command digest, provider error kinds. The project path is stored as a digest. The text of a `/ticket` note is kept after the ticket boundary check. There is no switch to turn recording off; `[telemetry]` only sets the signal thresholds. |
| Signals | `<project>/.anvil/telemetry/signals.jsonl` | What the classifier derived from the records. |
| Session transcripts | `<project>/.anvil/sessions/<id>/session.jsonl` and `status.json` | The full conversation, including file contents and command output the tools returned. |
| Logs | `<project>/.anvil/logs/` | Structured logs; `ANVIL_LOG` sets the level. |
| Memory and rules | `~/.anvil/memory/`, `~/.anvil/rules.toml` | Facts and rules you chose to keep. |
| Files anvil left | `~/.anvil/memory/corrections/<repo>.toml` | The text of files anvil changed at the end of a session (at most 32 KiB each, 128 KiB in all), to compare with your later edits. |
| Tickets | `~/.anvil/tickets/draft/` and `~/.anvil/tickets/open/` | Drafts, and the tickets you submitted. Drafts an older anvil left in `<project>/tickets/draft/` are moved here by the next `anvil ticket` command. |
| Ticket queue cache | `~/.anvil/queue.git` | A bare clone of the queue, made on the first submit. |
| MCP listings | `<project>/.anvil/mcp-cache/` | Tool lists the servers returned. |
| Trust decisions | `~/.anvil/trust.toml` | Each repository path you answered the trust prompt for, and the answer. |
| Your permission rules per repository | `~/.anvil/permissions/<repository>.toml` | The rules an `always` answer saved. |

The `.gitignore` of this repository keeps `.anvil/telemetry/`, `.anvil/sessions/`, `.anvil/logs/` and `.anvil/mcp-cache/` out of commits; other projects need the same lines.

## API keys

- Each provider names the environment variable that holds its key (`api_key_env`); the key is read from the process environment when a request is built (`crates/anvil-core/src/provider/mod.rs`). Keys never appear in configuration files.
- At startup anvil fills the environment from `~/.anvil/.env` without overriding anything already set (`crates/anvil-types/src/dotenv.rs`). A repository's `.anvil/.env` is never loaded, so a repository cannot set a key, `ANVIL_HOME`, a proxy or any other variable. `ANVIL_HOME` in your own environment replaces the home directory.
- The key is sent only to the endpoint of the provider that names it. It is not written to logs or transcripts: requests are logged by URL and attempt number only, and if a server's error body repeats the key, anvil replaces it with `[redacted]` before the error is shown, logged or recorded (`send_attempt` in `crates/anvil-core/src/provider/http.rs`; tested in `crates/anvil-cli/tests/doctor.rs`).
- `anvil setup` writes `~/.anvil/.env` from a template of placeholders and makes it private: mode 600 on Unix; on Windows it inspects the ACL with `icacls` and prints the command that removes a grant to other users (`crates/anvil-cli/src/setup.rs`).
- `anvil scan-keys` (part of `just check` and CI) fails when a tracked file holds something shaped like a key (`sk-`, `AIza`, `ghp_`, `github_pat_`, `hf_`) or any value of 12 or more characters from your `.env` files; it reports the file and line, never the value (`crates/anvil-cli/src/secrets.rs`).

## Permissions

- Every tool call is decided by your own rules: the `[permissions]` table of `~/.anvil/config.toml` or of a profile given with `--profile <name>`, or else your file for the repository, `~/.anvil/permissions/<repository>.toml` (`crates/anvil-core/src/tools/permissions.rs`, `crates/anvil-cli/src/wiring.rs`). The repository adds its `deny` and `ask` rules from `.anvil/config.toml` and `.anvil/permissions.toml` on top; it cannot add `allow` rules or a `default` (`crates/anvil-types/src/project.rs`). A rule is `tool` or `tool:glob`; the glob matches the path for file tools and the command line for `shell`. A deny rule from anywhere wins, then a repository `ask`, then your `allow`, then your `ask`, then your `default`, then the tool's own default.
- Without rules: reading tools (`read_file`, `glob`, `grep`, `list_dir`, `outline`, `delegate`, `skill`, read-only MCP tools) are allowed; `write_file`, `edit_file`, `shell` and every other MCP tool ask.
- These commands are refused whatever the answer, `--yes` and `default = "allow"` included, unless an allow rule names them explicitly: anything containing `rm -rf`, `Remove-Item -Recurse`, `git push --force` or `git reset --hard` (`crates/anvil-core/src/tools/shell.rs`). This is a substring check and easy to get around (`rm -r -f`, a script); treat it as a second layer, not as the control.
- The control for `shell` is `shell_programs`. When your rules, a profile or the repository set it, a command must be one program from the list, named as found on `PATH`, with plain arguments. `&&`, `||`, `;`, `|`, `&`, `>`, `<`, backticks, `$(`, `${`, `@(`, `(` and line breaks are refused, which covers chaining, redirection and substitution in sh, PowerShell and cmd. No rule, answer or `--yes` lifts it, and each layer can only narrow the list (`Permissions::shell_refusal` in `crates/anvil-core/src/tools/permissions.rs`). `profiles/strict.toml` in this repository ships it with `default = "ask"`: copy it to `~/.anvil/profiles/` and run with `--profile strict`. An allowed program can still run code it is given (`cargo build` runs build scripts, `just` runs recipes), so keep the list short.

## What anvil's tools can reach

- **Files.** Every file tool (`read_file`, `write_file`, `edit_file`, `glob`, `grep`, `list_dir`, `outline`) resolves its path, symlinks and `..` included, before opening it, and refuses anything outside the project root, the session folder and the folders you list in `[tools] extra_dirs`; installed skill folders may be read (`ToolContext::checked_path` in `crates/anvil-core/src/tools/mod.rs`, `crates/anvil-core/src/tools/workspace.rs`). Reads are confined too. Wherever they are, `.env` and `.env.*` files (not `.env.example`), `.ssh`, `.aws`, `.azure`, `.config/gcloud`, `.kube`, `.docker/config.json`, `.gnupg`, `.netrc`, `.git-credentials`, `~/.anvil/.env`, Chrome, Edge and Firefox profiles, and the macOS, Linux and Windows keychains and credential stores are refused, and directory searches skip them.
- **Environment.** Programs anvil starts for the model or the repository (`shell` commands, check commands, hooks, MCP servers, rule checks) do not inherit anvil's environment. They receive `PATH`, the home, user and temp variables, the locale (`LANG`, `LC_*` without secret-looking names), `TERM`, the variables Windows needs to start programs (`SystemRoot`, `ComSpec`, `PATHEXT`, `APPDATA`, `LOCALAPPDATA`, `ProgramFiles` and friends, `NUMBER_OF_PROCESSORS`, `PSModulePath`), `CARGO_HOME` and `RUSTUP_HOME`, and the names you list in `[tools] pass_env` (`crates/anvil-types/src/child_env.rs`). Provider keys and tokens in your environment or `~/.anvil/.env` are not passed. anvil's own `git` calls keep the full environment so your credential helper works for them.
- **What is not confined.** A `shell` command you approve runs with your own rights. It can read any file you can read and reach any host you can reach; the file jail does not apply to it. anvil does not sandbox it at the operating system level. That is on the roadmap, and it is something a firm can add around anvil (a container, a separate account, or an endpoint agent).
- A prompt offers: yes (this call), no, session (this tool for the rest of the session, and for path tools only inside the project's root, symlinks resolved), and for `shell` only, always (saves `shell:<first word> *` to your own file for the repository under `~/.anvil/permissions/`, never into the repository).
- `--yes` answers every prompt with yes and approves plans; deny rules and the refused commands still apply. `--allow-tool <name>` or `permissions.allow_tools` pre-approves a tool for one session. In the Plan state a tool that may write is refused.
- The Verify state runs the project's check command. A command you set in `[tools] check` runs as you set it. A command anvil detects from the repository's files (`just check`, `cargo test`, `npm test`, `python -m pytest`) runs the repository's own code, so it is asked about like any `shell` call (`crates/anvil-core/src/agent/mod.rs`).

## Running in a repository you do not trust

A repository you clone cannot change how anvil treats you:

- Its `.anvil/config.toml` and `.anvil/permissions.toml` may only add `deny` and `ask` rules. Every other key (providers, `base_url`, roles, limits, `[tools] shell` and `check`, `allow`, `default`, `allow_tools`) is ignored with a warning that names it (`crates/anvil-types/src/project.rs`). Providers, keys and every setting come from your own `~/.anvil/`.
- Its `.anvil/.env` is never loaded.
- Its hooks, MCP servers, skills, slash commands, agents and instruction files (`ANVIL.md`, `CLAUDE.md`, `.claude/CLAUDE.md`, `CLAUDE.local.md` and nested `CLAUDE.md` files) do not load until you trust the repository (`crates/anvil-cli/src/trust.rs`). The first interactive session shows every item, with the command line of each hook and MCP server, and asks once. The answer is kept in `~/.anvil/trust.toml`, keyed by the repository's full path. With `--plain`, `--yes`, no terminal, or a command that never prompts (`doctor`, `build`), an undecided repository is untrusted: nothing it ships loads and nothing is recorded. `anvil trust` shows the list and asks again; `anvil trust --revoke` withdraws trust.
- Even in a trusted repository, an `@path` import in one of its instruction files may only name a file inside the repository, so a `CLAUDE.md` cannot pull `~/.ssh/...` into the prompt (`crates/anvil-compat/src/instructions.rs`).

Trust is all or nothing and is not tied to the files' content: a repository you trusted can change its hooks later and they run. Pinning instruction files with re-approval on change is on the roadmap.

## Tickets

`anvil ticket draft` asks your summarizer provider to write drafts from telemetry signals and keeps them in `~/.anvil/tickets/draft/`. `anvil ticket submit` does nothing until `[tickets] queue` names a git repository; a stock install has no queue and never pushes anywhere. With a queue set, it validates the draft against the ticket boundary in `crates/anvil-types/src/boundary.rs` (no paths outside the repository, at most 40 lines of code in `repro` or `spec`), fetches the queue into `~/.anvil/queue.git`, and pushes the ticket as a `tickets/<id>` branch with your own git credentials. The current project's remotes are never used. The push carries only the ticket file, and only when you run the command.

## Release builds

- `.github/workflows/release.yml` runs on every push to `main`. It builds `anvil-cli` in release mode on GitHub-hosted runners for `x86_64-unknown-linux-gnu`, `x86_64-unknown-linux-musl` and `x86_64-pc-windows-msvc`, with the toolchain pinned in `rust-toolchain.toml`. A second job tags the commit `YYYY-MM-DD-<sha7>`, writes release notes with the changelog and a `## SHA256` section from `sha256sum`, and publishes the binaries as a GitHub release. It is the only workflow with write access (`contents: write`); `ci.yml` is read-only, and a test checks both.
- anvil has no self-update. A newer binary comes from whoever distributes anvil to you. A configuration file that still has an `[update]` section fails to load and says so.
- The hash and the binary come from the same GitHub release, so checking it proves the download is intact, not who built it. There is no signature.
