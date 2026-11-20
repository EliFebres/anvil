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

Two policies ship as starting points: `policies/restricted.toml` (one model server, no extensions, no trust in repositories, every write and command asks) and `policies/standard.toml`. Every key except `version` and `[network] allowed_hosts` is optional; the default of each is the restrictive choice.

| key | default | meaning |
|---|---|---|
| `version` | required | Must be `1`. |
| `[network] allowed_hosts` | required | The only `host:port` pairs a provider's `base_url` may name, such as `models.internal.example:443`. The port is required; the host is compared without regard to case. |
| `[network] require_tls` | `true` | Plain http only to a loopback address. With `false`, plain http to the allowed hosts. |
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
| `[features] mcp_servers` | none | With `"allowlist"`: entries `{ name = "...", command = "..." }`; both must match a server's definition. |
| `[features] hooks` | `false` | Whether hooks run. |
| `[features] skills` | `"off"` | `"off"`, `"bundled"` (skills that ship with anvil) or `"local"` (also the user's and a trusted repository's). |
| `[features] memory` | `false` | Whether remembered facts are read and the end-of-session summarizer proposes new ones. When off, nothing under `~/.anvil/memory` is read or written. |
| `[features] build` | `false` | Whether `anvil build` may run. |
| `[features] delegation` | `false` | Whether the `delegate` tool (subagents) is available. |
| `[projects] allow_trust` | `false` | Whether a repository can be trusted, so its hooks, MCP servers, skills, commands, agents, instruction files and `.anvil/permissions.toml` `allow` rules take effect. Its `deny` and `ask` rules apply either way. |
| `[data] transcript_dir` | the repository's `.anvil/sessions` | Where session folders go, one subfolder per repository. |
| `[data] retention_days` | kept | Session folders older than this are deleted when a session starts. |
| `[limits] session_tokens` | none | The most tokens one session may use; a larger user value is an error, and the default is capped. |
| `[limits] build_tokens`, `build_usd` | none | The most an `anvil build` may spend; a larger flag is an error, and without a flag the ceiling is the cap. |

Hooks, MCP servers and skills the policy forbids are errors when the user's own files define them, under `~/.anvil` or `~/.claude` (Claude Code's folder) alike, so a developer who uses Claude Code hooks or skills must remove them before anvil starts, or you allow them. The same items shipped by a trusted repository are left out with a note naming the key.

## Inspecting a machine

- `anvil policy show` prints every key of the policy schema, set or not, and whether the policy file or the default set it; a key left unset shows what that means, such as `[limits] build_usd = (default: no ceiling)`. It then prints every user setting the policy constrains (providers, roles, `[network] allow_http_hosts`, `[tools] extra_dirs`, the `[permissions]` keys, `[memory] always_summarize`, `[limits] session_tokens`) with the layer that set it.
- `anvil policy check <file> [--ownership]` validates a file and exits 1 with the reasons.
- `anvil doctor` checks the policy, keys, providers and tools, one row per check.
