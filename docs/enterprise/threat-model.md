# Threat model

One page. For each threat: what stops it today, what does not, and what is on the roadmap (`docs/ROADMAP.md`). File references are in SECURITY.md.

## 1. Malicious repository content

A cloned repository tries to make anvil act against the developer: send the API key to its own server, run commands without asking, plant instructions, or read the developer's secrets.

**Stopped today**
- The repository's `.anvil/config.toml` can only add `deny`, `ask` and `shell_programs`. Providers, `base_url`, roles, tool, network and shell settings, `allow` rules, defaults and pre-approved tools are errors that stop anvil and name the key. `.anvil/permissions.toml` may also hold `allow` rules, which apply only once the developer trusts the repository.
- Trust is pinned to a SHA-256 digest of everything the repository ships that loosens anvil or steers the model; any change asks again.
- anvil's file tools never write `.anvil/config.toml`, `.anvil/permissions.toml`, the hook settings files, `.mcp.json` or anything in `~/.anvil`, so the model cannot raise its own permissions through them.
- The repository's `.anvil/.env` is never loaded, so it cannot set keys, `ANVIL_HOME` or proxies.
- Hooks, MCP servers, skills, commands, agents and instruction files load only after the developer trusts the repository, from a prompt that lists each of them with its command line. Without a terminal, nothing loads. A policy can forbid repository content entirely.
- A repository instruction file cannot `@import` a file outside the repository.
- A check command detected from the repository's files is asked about before it runs.
- anvil's own git calls in the project run no hooks and none of the programs a repository's `.git/config` can name (file system monitor, pager, editor, external diff, text conversion, SSH command); a repository whose config names a filter or signing program is refused.
- Tests build a hostile repository that tries each of these and assert it is refused.

**Not stopped**
- Text in ordinary source files can still try to persuade the model (prompt injection). The model's tool calls still go through the permission rules, the workspace boundary, the deny categories and the shell mode, but an approved command does what it says.
- After trust is given, a repository can change its hooks or instructions and the new versions load.

**Roadmap:** pin instruction files and hooks by content, with re-approval on change.

## 2. Data leaving the firm

Source code, secrets or customer data reach a server the firm did not approve.

**Stopped today**
- Every request is checked when it is sent: it goes only to a host the configuration names, over https except to loopback unless a host is named, without following redirects; under a policy only to the `host:port` pairs in `[network] allowed_hosts`. An API key is sent only to its own provider's host. Proxy variables are ignored unless the policy sets `proxy = "system"`. Only one module can build an HTTP client, enforced by a lint and a test.
- No telemetry upload, update check or analytics exists. Webhooks and skill downloads were removed. HTTP MCP servers go through the same checked client, and under a policy only when allowed by name and URL.
- File tools cannot read outside the workspace or open `.env` files, key and certificate files, credentials files, `.git/config`, SSH, cloud and GPG credentials, browser profiles or keychains.
- Programs anvil starts do not inherit API keys or tokens.
- The API key is redacted from error bodies and never logged.
- Known secret shapes, private key blocks, JWTs, the values of secret-looking environment variables and the policy's own patterns are redacted from everything the tools return before it is sent or stored.

**Not stopped**
- Everything a session reads is sent to the model endpoint by design. Choose the endpoint and its host accordingly.
- An approved shell command on the host can reach the network directly when it gets past the deny categories, which match text only. `allowlist` mode narrows this to what the allowed programs themselves do (a build script or test the model wrote can still open a connection); the container executor (`--network none`) closes it.
- Secrets in a format redaction does not know (a password in a configuration file, a connection string, customer data) are sent and stored like any other text. A policy can keep transcripts to `metadata` or `off`.

**Roadmap:** redaction beyond known shapes; operating-system sandboxing of shell commands with network control; an append-only, hash-chained audit log with export.

## 3. Supply chain

A tampered anvil binary, a malicious dependency, or a compromised build.

**Stopped today**
- anvil cannot update itself or download code. Skills install only from a local folder.
- Release builds run on GitHub-hosted runners with a pinned toolchain; the only workflow with write access is the release workflow, and a test checks every workflow's permissions.
- Each release lists the SHA256 of each binary.

**Not stopped**
- Binaries are not signed, and the hash comes from the same release as the binary.
- Dependencies are not audited in CI (`cargo audit`, `cargo deny`), and there is no SBOM.

**Roadmap:** signed installers per operating system; distribution through the firm's own channel; SBOM, `cargo audit`, `cargo deny` and vendored offline builds. Until then, build from source at a reviewed commit.

## 4. Model mistakes

The model misunderstands the task, deletes or breaks files, runs a harmful command, or writes insecure code.

**Stopped today**
- Writes and shell commands ask first unless the developer's own rules allow them; a policy can force a strict profile and require a person to approve plans (in `anvil build`, only the top-level plan).
- `[shell] mode = "allowlist"` (and `shell_programs`) limits commands to listed programs started without a shell, refusing chaining, redirection and substitution, arguments that name a path outside the project, and git options that change config, write files or run programs (`shell::argument_refusal`, `git::allowlisted`); an allowed program's own behaviour is not limited; `"off"` removes the tool; no answer or `--yes` lifts either.
- File tools cannot write outside the workspace, and never write inside `.git`, so a session cannot plant a git hook or change the repository's git config. A tool write to a file that grants power (a settings file that defines hooks or MCP servers, `.mcp.json`, the repository's permission rules) is refused, even under `--yes`.
- The loop plans before it builds, runs the project's checks, and has the change reviewed before it finishes. Every change is an ordinary diff in the working tree.

**Not stopped**
- An approved command runs as approved. An allowed program can run code it is given (`cargo build` runs build scripts).
- Code that passes the checks can still be wrong or insecure.

**Mitigation by process:** all output goes through the firm's normal pull request review (see `docs/enterprise/pilot-scope.md`).
