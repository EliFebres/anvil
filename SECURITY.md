# Security notes

This page is for someone deciding whether anvil may run on their machines. It describes what the code in this repository does, with file references, so each claim can be checked. If you find a vulnerability, open a private security advisory on the GitHub repository rather than a public issue.

## In one paragraph

anvil is a single binary that sends your prompts, and the files and command output its tools read, to the model providers you configure, and to nothing else unless you turn something on. It has no telemetry upload, no update check at startup and no background process. It keeps transcripts, logs and content-free usage records as plain files in the project's `.anvil/` folder and in `~/.anvil/`. API keys are read from environment variables and `.env` files, never from configuration files. File writes and shell commands ask first unless a permission rule says otherwise. **anvil trusts the project it runs in**: a repository's own `.anvil/` and `.claude/` files can change providers, permissions, environment variables, hooks and MCP servers (see "Running in a repository you do not trust").

## Network destinations

| destination | what triggers it | on by default |
|---|---|---|
| **Model providers**: each `[providers.<name>] base_url`. `anvil setup` writes `https://api.anthropic.com/v1`, `https://api.openai.com/v1` and `https://generativelanguage.googleapis.com/v1beta/openai`; there is no built-in default (`crates/anvil-types/src/config.rs`). Requests go to `{base_url}/messages` or `{base_url}/chat/completions` (`crates/anvil-core/src/provider/`). | Every model turn of `anvil run`, `chat`, `resume` and `build`. Also, inside a session and without a separate prompt: context compaction, the end-of-session memory summarizer, and at session start the summarizer call that proposes rules from files you edited after anvil wrote them (it sends a diff of those edits, at most 16 KiB). `anvil memory learn` and `anvil ticket draft` call the summarizer too. `anvil doctor` sends a one-token request to every configured provider. | Yes, whenever those commands run. They are the product. |
| **MCP servers** from `.mcp.json`, `.claude/settings.json`, `.claude/settings.local.json` (project) and `~/.anvil/mcp.toml` (`crates/anvil-compat/src/mcp/mod.rs`). A stdio server is a local program anvil starts; an HTTP server is a URL anvil calls, with headers whose `${VAR}` references are filled from the environment. SSE is refused. | Every server is started or contacted when a session starts (`run`, `chat`, `resume`, `build`), unless `.anvil/mcp-cache/` holds a fresh listing for it. There is no per-server confirmation. Calling a server's tool asks first unless the server marks the tool read-only. | Only when such a file defines servers. |
| **GitHub API**: `GET https://api.github.com/repos/<repo>/releases`, then the release asset's `browser_download_url` (`crates/anvil-cli/src/update.rs`). `ANVIL_GITHUB_API_URL` replaces the API base. | `anvil update` only. | No. Nothing checks for updates on its own. |
| **The ticket queue**: the git URL in `[tickets] queue`. A `git fetch` into the cache `~/.anvil/queue.git`, then `git push queue refs/heads/tickets/<id>`, with your own git credentials (`crates/anvil-telemetry/src/git.rs`). | `anvil ticket submit` and the submit key of `anvil ticket review` only. | No. The key is unset by default, and while it is unset `submit` says what to set and sends nothing. See "Tickets". |
| **A webhook** (Discord, Slack or ntfy, chosen by the URL): one POST per change into `needs input`, `blocked`, `done` or `failed`, carrying the first line of the task (`crates/anvil-cli/src/status/notify.rs`). | Automatic during a session once configured. | No: needs `[notify] webhook_env` in the config and that variable set to an http(s) URL. |
| **git clone** of a skill repository: `git clone --depth 1 <url>` (`crates/anvil-compat/src/skills/install.rs`). | `anvil skill add <url>` only. | No. |
| **Hooks**: shell commands from the `hooks` section of `.claude/settings.json`, `.claude/settings.local.json`, `.anvil/settings.json` (project) and `~/.claude/settings.json`, `~/.anvil/settings.json` (`crates/anvil-compat/src/hooks.rs`). They can do anything, including network calls. | Automatic around tool calls once defined. | Only when such a file defines hooks. |
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

The `.gitignore` of this repository keeps `.anvil/telemetry/`, `.anvil/sessions/`, `.anvil/logs/` and `.anvil/mcp-cache/` out of commits; other projects need the same lines.

## API keys

- Each provider names the environment variable that holds its key (`api_key_env`); the key is read from the process environment when a request is built (`crates/anvil-core/src/provider/mod.rs`). Keys never appear in configuration files.
- At startup anvil fills the environment from `.env` files without overriding anything already set, first definition wins: the real environment, then `~/.anvil/.env`, then the project's `.anvil/.env` (`crates/anvil-types/src/dotenv.rs`). `ANVIL_HOME` replaces the home directory.
- `anvil setup` writes `~/.anvil/.env` from a template of placeholders and makes it private: mode 600 on Unix; on Windows it inspects the ACL with `icacls` and prints the command that removes a grant to other users (`crates/anvil-cli/src/setup.rs`).
- `anvil scan-keys` (part of `just check` and CI) fails when a tracked file holds something shaped like a key (`sk-`, `AIza`, `ghp_`, `github_pat_`, `hf_`) or any value of 12 or more characters from your `.env` files; it reports the file and line, never the value (`crates/anvil-cli/src/secrets.rs`).

## Permissions

- Every tool call is decided by rules in `<project>/.anvil/permissions.toml`, or by a profile's `[permissions]` table when `--profile <name>` is given (`crates/anvil-core/src/tools/permissions.rs`). A rule is `tool` or `tool:glob`; the glob matches the path for file tools and the command line for `shell`. A deny rule wins, then allow, then ask, then the file's `default`, then the tool's own default.
- Without rules: reading tools (`read_file`, `glob`, `grep`, `list_dir`, `outline`, `delegate`, `skill`, read-only MCP tools) are allowed; `write_file`, `edit_file`, `shell` and every other MCP tool ask.
- These commands are refused whatever the answer, `--yes` and `default = "allow"` included, unless an allow rule names them explicitly: anything containing `rm -rf`, `Remove-Item -Recurse`, `git push --force` or `git reset --hard` (`crates/anvil-core/src/tools/shell.rs`).
- A prompt offers: yes (this call), no, session (this tool for the rest of the session, and for path tools only inside the project's root, symlinks resolved), and for `shell` only, always (saves `shell:<first word> *` to the project's permissions file).
- `--yes` answers every prompt with yes and approves plans; deny rules and the refused commands still apply. `--allow-tool <name>` or `permissions.allow_tools` pre-approves a tool for one session. In the Plan state a tool that may write is refused.

## Running in a repository you do not trust

anvil reads configuration from the project it runs in and does not ask whether you trust it. A repository you clone can ship:

- `.anvil/permissions.toml` that allows `shell` or every tool, so commands run without asking;
- `.anvil/config.toml`, which overrides your global configuration key by key, including a provider's `base_url`, so requests that carry your API key go to another server;
- `.anvil/.env`, which sets any variable you have not set yourself, such as `ANVIL_GITHUB_API_URL`, which `anvil update` uses to find releases;
- hooks in `.claude/settings.json`, which run shell commands around tool calls;
- MCP servers in `.mcp.json`, which anvil starts or contacts when the session starts.

Read those files before running anvil in a repository you did not write, or run it there with a profile whose `[permissions]` table you control.

## Tickets

`anvil ticket draft` asks your summarizer provider to write drafts from telemetry signals and keeps them in `~/.anvil/tickets/draft/`. `anvil ticket submit` does nothing until `[tickets] queue` names a git repository; a stock install has no queue and never pushes anywhere. With a queue set, it validates the draft against the ticket boundary in `crates/anvil-types/src/boundary.rs` (no paths outside the repository, at most 40 lines of code in `repro` or `spec`), fetches the queue into `~/.anvil/queue.git`, and pushes the ticket as a `tickets/<id>` branch with your own git credentials. The current project's remotes are never used. The push carries only the ticket file, and only when you run the command.

## Release builds and `anvil update`

- `.github/workflows/release.yml` runs on every push to `main`. It builds `anvil-cli` in release mode on GitHub-hosted runners for `x86_64-unknown-linux-gnu`, `x86_64-unknown-linux-musl` and `x86_64-pc-windows-msvc`, with the toolchain pinned in `rust-toolchain.toml`. The build bakes in the commit (`ANVIL_BUILD_SHA`) and the repository (`ANVIL_RELEASE_REPO`). A second job tags the commit `YYYY-MM-DD-<sha7>`, writes release notes with the changelog and a `## SHA256` section from `sha256sum`, and publishes the binaries as a GitHub release. It is the only workflow with write access (`contents: write`); `ci.yml` is read-only, and a test checks both.
- `anvil update` lists the repository's releases, skips drafts and prereleases, takes the newest one, downloads the asset for its platform, and computes its SHA-256. It refuses to install when the release notes have no hash for the asset or the hash differs, and only then swaps the binary (the old one is kept as `.old`). A build from source has no baked-in repository; it uses `[update] repo` from the configuration or refuses.
- The hash and the binary come from the same GitHub release, so the check proves the download is intact, not who built it. There is no signature.
