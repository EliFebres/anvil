# Roadmap

Work that is deliberately not done yet, with one sentence on why each matters. SECURITY.md says what is enforced today.

## Next: self-hosted endpoint support (the pull request after the hardening)

- **Provider tolerance for self-hosted servers.** Fall back to a client-side token estimate when a server reports no usage, support both `max_tokens` and `max_completion_tokens`, accept a stream that ends without `finish_reason`, use a long request timeout, retry on connection refused while a model loads, and make `api_key_env` optional; without this, budgets and statistics read zero and some vLLM and llama.cpp servers fail on the first turn.
- **`anvil doctor` as the acceptance test for an endpoint.** A streaming round trip, one real tool call, a usage check, a warning when the server's context is smaller than the configured `context_window`, and latency; a reviewer then has one command that proves an endpoint works before a pilot starts.

## Isolation

- **Operating-system sandboxing of shell commands.** An approved command runs with the developer's rights today; a sandbox (a restricted token or AppContainer on Windows, Seatbelt on macOS, namespaces or Landlock on Linux) would confine its files and network the way the file tools are already confined.
- **A policy key for an administrator-owned git hooks path.** anvil's own git calls turn hooks off, so firm-required commit hooks (secret scanning, commit message checks) do not run on `anvil build` commits today; a policy key naming a hooks folder the administrator owns would let anvil pass that folder as `core.hooksPath` instead.

## Distribution and supply chain

- **Signed installers per operating system.** A signature proves who built the binary, which a hash from the same release cannot.
- **Distribution through the firm's own channel.** IT can then pin a reviewed version and roll it out with the policy file, instead of each developer downloading a release.
- **SBOM, `cargo audit`, `cargo deny` and vendored offline builds.** Reviewers need the dependency list and known advisories, and some firms build without network access.

## Records and secrets

- **Append-only, hash-chained audit log with export.** Transcripts are plain files today; a chained log lets a reviewer prove nothing was removed or changed after the fact.
- **Secret redaction in transcripts and prompts.** The deny list keeps the file tools away from credential stores, but a secret inside an ordinary file is still sent and stored as text.
- **Operating-system credential store for the endpoint key.** The key lives in the environment or `~/.anvil/.env` behind file permissions; the Windows Credential Manager, the macOS Keychain or the Secret Service would keep it out of files.

## Trust

- **Pinning of repository instruction files, with re-approval on change.** Trust is per repository path today, so a trusted repository can change its hooks or `CLAUDE.md` later without a new prompt.

## The model host

- **A hardening guide for the model server.** anvil does not secure the machine that runs the model; a short guide (network exposure, authentication, prompt retention, logging, updates for vLLM and llama.cpp) would help the team that operates it.
