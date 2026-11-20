# Administrator guide

This guide is for the team that installs anvil on developer workstations and controls it with a managed policy. SECURITY.md says what each control does and which test proves it; this page says how to set it up.

## The policy file

anvil reads one policy file, at a path fixed in the binary:

| system | path |
|---|---|
| Linux and macOS | `/etc/anvil/policy.toml` |
| Windows | `C:\ProgramData\anvil\policy.toml` |

No environment variable, flag, profile or configuration key moves the file or turns it off. When the file is missing, anvil runs unmanaged and says `policy: unmanaged` at the start of every session. When the file exists, anvil refuses to run if:

- someone other than an administrator could change or delete the file or its folder;
- it cannot be read, is not UTF-8, does not parse, or contains a key anvil does not know;
- `version` is not `1`, `[network] allowed_hosts` is empty or holds an entry without a numeric port, or another value breaks a rule listed below.

A user setting, profile, flag or repository file that conflicts with the policy is an error naming the policy key, for example `the managed policy's [network] allowed_hosts forbids api.openai.com:443, the host of provider "public" in your configuration (...)`. The user fixes their configuration; nothing is silently overridden.

### Ownership

On Linux and macOS:

```sh
sudo install -d -o root -g root -m 755 /etc/anvil
sudo install -o root -g root -m 644 restricted.toml /etc/anvil/policy.toml
```

On Windows, from an elevated PowerShell. A new folder under `C:\ProgramData` inherits an entry that lets every user create files in it, so inheritance must be removed:

```powershell
New-Item -ItemType Directory -Force C:\ProgramData\anvil | Out-Null
Copy-Item restricted.toml C:\ProgramData\anvil\policy.toml
icacls C:\ProgramData\anvil /setowner "*S-1-5-32-544" /T
icacls C:\ProgramData\anvil /inheritance:r /grant:r "*S-1-5-32-544:(OI)(CI)F" "*S-1-5-18:(OI)(CI)F" "*S-1-5-32-545:(OI)(CI)RX" /T
```

`S-1-5-32-544` is Administrators, `S-1-5-18` is SYSTEM and `S-1-5-32-545` is Users; security identifiers are used because group names change with the system language. anvil accepts Administrators, SYSTEM and TrustedInstaller as owners and as holders of write, delete, change-permission and take-ownership rights; any other principal holding one of those rights makes anvil refuse to start.

Check the result before rollout:

```sh
anvil policy check --ownership /etc/anvil/policy.toml
anvil doctor
```

`anvil doctor` shows a `policy`, `policy:ownership` and `policy:sha256` row. The hash identifies the exact file in force; record it with each rollout.

### Rollout

The policy is a plain file, so roll it out with the configuration management the organization already uses (Group Policy file preferences, Intune, Ansible, Puppet, Chef, Jamf). Validate every change with `anvil policy check <file>` in the pipeline that publishes it. A broken file stops anvil on every machine that receives it, which is the intended failure: no machine runs with a policy it could not read.

## Keys

Two policies ship as starting points: `policies/restricted.toml` (one model server, no extensions, no trust in repositories, every write and command asks, commands limited to an allowlist and started without a shell) and `policies/standard.toml` (the shell in `ask` mode). Every key except `version` and `[network] allowed_hosts` is optional; the default of each is the restrictive choice.

| key | default | meaning |
|---|---|---|
| `version` | required | Must be `1`. |
| `[network] allowed_hosts` | required | The only `host:port` pairs a provider's `base_url` or an HTTP MCP server's `url` may name, such as `models.internal.example:443`. The port is required; the host is compared without regard to case. Every request is checked against it when it is sent, and redirects are never followed. |
| `[network] require_tls` | `true` | Plain http only to a loopback address. With `false`, plain http to the allowed hosts. |
| `[network] proxy` | `"none"` | `"none"`: no proxy, and `HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY` and `NO_PROXY` are ignored. `"system"`: those variables, or the system settings when they are unset. A URL such as `"http://proxy.internal.example:3128"`: every request goes through that proxy. A proxy URL may not carry a user name or password, because the policy file is readable by every user. |
| `[network] ca_bundle` | the system store | An absolute path to a PEM file of certificate authorities. When set, server certificates are verified against these alone, not the system's store; include your internal root and, if anvil must reach a public endpoint, its public roots too. |
| `[network] client_cert`, `client_key` | none | Absolute paths to a PEM certificate chain and its private key, presented for mutual TLS. Set both or neither. Make the key readable by the developers who run anvil and no one else. |
| `[models] providers` | any | Provider names (`[providers.<name>]`) the user's configuration may define. |
| `[models] models` | any | Model names (`model = ...`) the user's configuration may use. |
| `[permissions] deny` | none | Rules (`tool` or `tool:glob`) that refuse a call whatever else allows it. |
| `[permissions] ask` | none | Rules that make a call ask a person whatever the user allows. A user or profile `allow` rule for the same tool and pattern is an error. |
| `[permissions] allow_yes` | `false` | Whether `--yes` may be used, and whether a user's configuration, profile or saved rules file may set `[permissions] default = "allow"`, which approves every call no rule names without a person. Either one is an error when this is off. |
| `[permissions] allow_allow_tool` | `false` | Whether `--allow-tool` and `permissions.allow_tools` may be used. |
| `[permissions] allow_always` | `false` | Whether the `always` prompt answer saves a rule, and saved rules under `~/.anvil/permissions/` are read. A saved rules file is an error when this is off. |
| `[permissions] require_plan_approval` | `false` | A person approves every plan, even under `--yes`; in `anvil build`, the top-level plan. |
| `[files] extra_roots` | none | Folders outside the workspace the file tools may read, never write. The user's `[tools] extra_dirs` is an error under a policy. |
| `[files] deny` | none | Path globs (such as `**/*.kdbx`) the file tools refuse and searches skip, inside the workspace too. |
| `[features] mcp` | `"off"` | `"off"`, `"allowlist"` or `"on"`. |
| `[features] mcp_servers` | none | With `"allowlist"`: entries `{ name = "...", command = "..." }` for a local server or `{ name = "...", url = "https://..." }` for an HTTP server; the name and the program or URL (a trailing `/` aside) must match the definition. An HTTP server's `host:port` must also be in `allowed_hosts`, with `"on"` too. An entry may add `listing_sha256 = "<64 hex digits>"`: then only that tool listing is accepted from the server (see "MCP listing approval"). |
| `[features] hooks` | `false` | Whether hooks run. |
| `[features] skills` | `"off"` | `"off"`, `"bundled"` (skills that ship with anvil) or `"local"` (also the user's and a trusted repository's). |
| `[features] memory` | `false` | Whether remembered facts are read and the end-of-session summarizer proposes new ones. When off, nothing under `~/.anvil/memory` is read or written. |
| `[features] build` | `false` | Whether `anvil build` may run. |
| `[features] delegation` | `false` | Whether the `delegate` tool (subagents) is available. |
| `[shell] mode` | `"off"` | `"off"` (no `shell` tool; `anvil build` refuses to start), `"allowlist"` or `"ask"`. A user mode that is looser is an error; a stricter one applies. |
| `[shell] allow` | none | With `"allowlist"`: argument-prefix patterns such as `"cargo test *"` (`*` only as the last word). A command must be one program with plain arguments, matching a pattern here and in the user's own `[shell] allow` and `shell_programs` if they set them. A pattern that names a deny-category command (`"git push origin *"`) allows it. |
| `[shell] executor` | `"host"` | `"host"` or `"container"`. The user cannot choose another. |
| `[shell.container] runtime` | `"docker"` | `"docker"` or `"podman"`, found on the `PATH` anvil passes to programs. |
| `[shell.container] image` | required with `"container"` | The image commands run in; pin it by digest. anvil starts it with `--pull=never`, so it must already be on each workstation: pre-load it (`docker pull` or `docker load`) as part of the rollout. A missing image is an error, never a download. |
| `[shell.container] user` | `"65534:65534"` | The user inside the container, never root. Commands can write the workspace only if this user may write its files on the host. |
| `[shell.container] memory`, `pids_limit` | `"2g"`, `256` | The container's memory and process limits. |
| `[projects] allow_trust` | `false` | Whether a repository can be trusted, so its hooks, MCP servers, skills, commands, agents, instruction files and `.anvil/permissions.toml` `allow` rules take effect. Its `deny` and `ask` rules apply either way. |
| `[data] transcript_dir` | the repository's `.anvil/sessions` | Where session folders go, one subfolder per repository, named `<slug>-<12 hex digits of the SHA-256 of its path>`. |
| `[data] retention_days` | kept | Session folders older than this are deleted when a session starts. A developer can also run `anvil purge --older-than <days>`. |
| `[data] redaction` | `true` | Whether secrets are redacted from tool results, file contents, MCP results, hook output and prompts before they are sent or stored. Only the policy can turn it off; anvil then prints a note at startup. |
| `[data] patterns` | none | Extra patterns to redact, as `[{ kind = "employee_id", regex = '\bEMP-[0-9]{6}\b' }]` (Rust `regex` syntax; the kind is lowercase letters, digits and `_`). A match becomes `[REDACTED:employee_id]`. |
| `[data] transcript` | `"off"` | `"full"`: the session log keeps every message as the provider received it. `"metadata"`: roles, tool names, sizes, SHA-256 digests, token counts, timings, states and redaction counts, with no message text, file contents or command output. `"off"`: no session log. Below `"full"`, no untruncated tool output or compaction summary is written, sessions cannot be resumed, and the end-of-session memory step has nothing to read. `plan.md` and `summary.md` are still written. |
| `[audit] required` | `true` | A session runs only while its audit log can be written: one whose `audit.jsonl` or mirror cannot be opened does not start, and a failed write stops it with a non-zero exit. Unmanaged sessions write the log without requiring it. |
| `[audit] mirror_dir` | none | An absolute folder every session's records are also written to, as `<session id>.audit.jsonl`, and every build's decisions as `<build id>.audit.jsonl`. A mirror always holds the whole chain: one that is missing when a session resumes gets a copy of the session file first, and one that differs from it stops a required session from starting. Use a folder developers can append to but not rewrite, and collect it; see "Collecting audit logs". |
| `[audit] record` | `"digest"` | `"digest"`: prompts and tool arguments as size and SHA-256. `"full"`: their text after redaction, which puts file paths, commands and the developer's prompts into the log. |
| `[limits] session_tokens` | none | The most tokens one session may use; a larger user value is an error, and the default is capped. |
| `[limits] build_tokens`, `build_usd` | none | The most an `anvil build` may spend; a larger flag is an error, and without a flag the ceiling is the cap. |

A user `[tools] check` command is an error when `[shell] mode = "off"`, or when the mode is `"allowlist"` and no pattern allows it.

Hooks, MCP servers and skills the policy forbids are errors when the user's own files define them, under `~/.anvil` or `~/.claude` (Claude Code's folder) alike, so a developer who uses Claude Code hooks or skills must remove them before anvil starts, or you allow them. The same items shipped by a trusted repository are left out with a note naming the key.

### Choosing a shell mode

- `"off"` suits review-only or documentation work: the model reads and edits files and runs nothing.
- `"allowlist"` limits which programs start and with which arguments; it does not limit what an allowed program then does. It guarantees, on the workstation:
  - only a command that matches a pattern in every layer starts, as one program named on `PATH` with an argument vector and no shell, so chaining, redirection and substitution cannot happen (`Permissions::shell_refusal` in `crates/anvil-core/src/tools/permissions.rs`; test `shell_programs_allow_one_listed_program_with_plain_arguments`);
  - no argument, no value after `=` and no attached short-option value names a path outside the project: one that starts with `~`, or is absolute, a drive or UNC path or climbs with `..` and resolves outside is refused (`shell::argument_refusal`; test `arguments_that_reach_outside_the_workspace_are_refused_for_every_program` in `crates/anvil-core/src/tools/allowlist_tests.rs`);
  - git takes no option before its subcommand except `--no-pager`, `-P`, `--no-optional-locks`, `--literal-pathspecs`, `--no-replace-objects`, `--version` and `--help`, and never `--output`, `-o`, `--no-index`, `--ext-diff`, `--textconv`, `--open-files-in-pager` or `-O` (tests `git_options_before_the_subcommand_are_refused`, `git_options_that_write_read_outside_or_run_programs_are_refused`);
  - git runs through `anvil_core::git::allowlisted`, with hooks, the file system monitor, pagers, editors and remotes off, external diff and text conversion off for diff and blame commands, and refuses a repository whose own config defines an alias, a diff, merge or credential program, a filter, a signing program or `core.worktree`, and any subcommand that is an alias from any config file (tests `allowlisted_git_refuses_repository_aliases_and_programs`, `under_git_star_an_alias_cannot_run_and_output_cannot_land_outside`).

  It does not guarantee anything about an allowed program's own behaviour: `cargo build` and `cargo test` run build scripts, procedural macros and tests the model can write, `just` runs recipes, and a program may read or write files the project's own configuration names. Programs and settings in the user's own git config (a global alias, `diff.external`, a signing program) are the user's choice and still run. Keep the list to what the work needs, prefer narrow patterns (`cargo test *`, not `cargo *`), and use the container executor where code the model writes must not touch the workstation.
- `"ask"` runs commands in the user's shell after a person (or `--yes`) approves them. The built-in deny categories (network clients such as `curl`, `wget`, `Invoke-WebRequest` and `ssh`; `git push`, `git clone`, `git remote add` and `set-url`; `pip`, `npm`, `cargo` and `gem` installs; `sudo`, `runas`, `doas`; `rm -rf` and three other destructive patterns) apply in every mode, but they match command text, and a shell can hide a command from a text match. Treat them as a guard rail, not a security boundary.
- `executor = "container"` is the other boundary: each command runs through docker or podman with `--network none`, the workspace as the only host folder (its `.git` read-only), a read-only root file system with an in-memory `/tmp`, a user and group that are not root in any spelling (`0`, `00` and `root` are refused), `--cap-drop ALL`, `--security-opt no-new-privileges` and the process and memory limits. It combines with either mode. Provide an image with the project's toolchain, and make sure the container user can write the workspace (for example, the developers' shared group id) if commands need to.

### MCP listing approval

A server may start only when the policy's `[features] mcp` allows it by name and program or URL. What it offers is approved separately. Every session start lists every server live and hashes the listing: each tool's name, title, description, input schema and all its annotations (`readOnlyHint`, which lets a call run without a prompt, `destructiveHint`, `idempotentHint`, `openWorldHint`, `title`).

- With `listing_sha256` on the server's `[features] mcp_servers` entry, only that listing is accepted; any other stops the server with an error that prints the digest it saw.
- Without it, the first listing is recorded in the user's `~/.anvil/mcp-approvals.toml` together with a digest of the definition (command and arguments, or URL). A later listing that differs, or a changed definition, stops the server.

Under a policy `anvil mcp approve` is refused, so a developer cannot accept a changed listing; you accept it by pinning. To find the digest, run `anvil mcp list` on a workstation with the server: it prints `mcp <name>: listing_sha256 = "<digest>"` for each server that answered, accepted or not. Review the listing the same command prints, then add the digest to the entry. The approvals file belongs to the user, so without a pin it detects changes a server makes, not changes the user makes (deleting the file makes the next listing count as the first); a pin does not depend on that file. To stop a server for everyone, remove it from `mcp_servers`.

### Collecting audit logs

Each session writes `audit.jsonl` in its session folder (`<project>/.anvil/sessions/<id>/`, or under `[data] transcript_dir`), whatever `[data] transcript` says. One line is one record; SECURITY.md ("Audit log") lists the fields and the canonical form.

- A chain on the developer's disk shows a record edited, removed or moved inside the file, but not records cut from the end or a file rewritten whole by someone who can write it. Set `mirror_dir` to a folder outside the developer's control that they can append to (for example a share or a local folder with an append-only access list), collect it, and compare the mirror with the session folder: a mirror with more records than the session's own file shows a cut.
- `anvil audit verify <file>` checks a collected file and exits 1 at the first break; `anvil audit verify <session file> --mirror <mirror file>` also exits 1 when the two differ, naming the first line that does. `anvil audit show <file>` prints it readably. `crates/anvil-audit/src/audit/verify_audit.py` applies the same rules without anvil (`python verify_audit.py <file>`); a test keeps the two in agreement.
- `[data] retention_days` and `anvil purge` delete session and build folders, the `audit.jsonl` in them included, but not while the session or build is running; they never touch the mirror.
- Syslog, Windows Event Log and OpenTelemetry export are not built (docs/ROADMAP.md); collect the files instead.

## Inspecting a machine

- `anvil policy show` prints every key of the policy schema, set or not, and whether the policy file or the default set it; a key left unset shows what that means, such as `[limits] build_usd = (default: no ceiling)`. It then prints every user setting the policy constrains (providers, roles, `[network] allow_http_hosts`, `[tools] extra_dirs`, the `[permissions]` keys, `[memory] always_summarize`, `[limits] session_tokens`) with the layer that set it.
- `anvil policy check <file> [--ownership]` validates a file and exits 1 with the reasons.
- `anvil doctor` checks the policy, keys, providers and tools, one row per check. Its `data:~/.anvil` and `data:.anvil` rows fail when anyone besides the user (and SYSTEM and Administrators on Windows) can open those folders, and print the `chmod` or `icacls` command that fixes it.
- `anvil doctor --egress` lists, without sending anything, every `host:port` anvil's HTTP client can reach under the configuration, with the provider or MCP server that uses it, each configured endpoint the policy refuses, the plain-http rule, the proxy mode (and which proxy variables are set, never their values), the trust roots and the client certificate. Local MCP servers, hooks and shell commands are programs that make their own connections; it does not list those. Run it on a pilot workstation to confirm the policy matches the firewall rules.
