# Roadmap

Work that is deliberately not done yet, with one sentence on why each matters. SECURITY.md says what is enforced today.

## Next: self-hosted endpoint support (the pull request after the hardening)

- **Provider tolerance for self-hosted servers.** Fall back to a client-side token estimate when a server reports no usage, support both `max_tokens` and `max_completion_tokens`, accept a stream that ends without `finish_reason`, use a long request timeout, retry on connection refused while a model loads, and make `api_key_env` optional; without this, budgets and statistics read zero and some vLLM and llama.cpp servers fail on the first turn.
- **`anvil doctor` as the acceptance test for an endpoint.** A streaming round trip, one real tool call, a usage check, a warning when the server's context is smaller than the configured `context_window`, and latency; a reviewer then has one command that proves an endpoint works before a pilot starts.

## Managed policy keys still to come

- **`[audit]`**: a required audit sink, a mirror directory, and full or digested arguments.

## Network

- **Proxy and TLS settings without a policy.** `[network] proxy`, `ca_bundle`, `client_cert` and `client_key` exist only in the managed policy, so an unmanaged developer behind a corporate proxy cannot use one (anvil ignores `HTTPS_PROXY`); a user-level equivalent would serve them without giving a repository a way in.
- **HTTP MCP sessions that expire.** When a server ends its session and answers 404, anvil reports the error and the server stays unusable until the next session; the specification asks the client to start a new session. anvil also does not open the optional GET stream, answers requests a server sends inside a response with "method not found", and does not support OAuth authorization (a definition's static headers only).
- **MCP protocol revision 2026-07-28.** anvil speaks 2025-06-18 over stdio and HTTP. The current revision removes protocol-level sessions and the GET stream and adds per-request headers (`Mcp-Method`, `Mcp-Name`); servers that support only it would need those changes.
- **FIPS-validated cryptography.** Not attempted. What it would take with the current setup, checked against the rustls 0.23.46 manual (`manual::_06_fips`), crate features and `ClientConfig` documentation, the aws-lc-rs 1.19.0 and aws-lc-fips-sys 0.14.4 crate sources and READMEs, the aws-lc-rs platform pages, the reqwest 0.13.5 feature list, the vendored reqwest source and the NIST certificate page (2026-11-19):
  - rustls has a `fips` feature that turns on `aws-lc-rs/fips` and `webpki/aws-lc-rs-fips`. The provider is `rustls::crypto::default_fips_provider()`, and `ClientConfig::fips()` reports whether connections made with a configuration operate in FIPS mode, which covers its TLS settings as well as its algorithms. The feature does not set `require_ems` itself: with the `tls12` feature, `require_ems` defaults to `true` when the configured provider is FIPS-compliant.
  - reqwest 0.13 has no FIPS feature: `rustls` brings the aws-lc-rs provider and `rustls-no-provider` brings none. Its builder uses the process default `CryptoProvider` when one is installed and its own aws-lc-rs default otherwise. anvil would add rustls with `fips` to the workspace (feature unification then builds aws-lc-rs against `aws-lc-fips-sys`), install `default_fips_provider()` as the process default before `http_client` runs, and refuse to start if that fails. Checking `fips()` on the actual configuration needs a rustls `ClientConfig` built by anvil and passed with `tls_backend_preconfigured`, which reqwest documents as having no semver stability.
  - Certificate checks: without `ca_bundle`, reqwest verifies through `rustls-platform-verifier`, which hands chain checks to the operating system on Windows and macOS; with `ca_bundle` it uses webpki with the rustls provider. Which of the two fits the firm's FIPS boundary needs a decision.
  - Building `aws-lc-fips-sys` needs a C and C++ compiler, CMake, Go and Perl on every platform, and on Windows also Ninja and bindgen with libclang, plus NASM on `x86_64-pc-windows-msvc` (the `aarch64-pc-windows-msvc` build uses clang-cl instead). Those two MSVC targets are the only Windows targets with FIPS support; a Windows FIPS build produces DLLs that ship next to the binary.
  - Module version: aws-lc-rs 1.19.0 (the version in `Cargo.lock`) requires `aws-lc-fips-sys` `^0.14.2`, which is not in the lock today because `fips` is off. The newest match, 0.14.4, vendors AWS-LC with `AWSLC_VERSION_NUMBER_STRING` `4.2.0` and reports `AWS-LC FIPS 4.2.0` in FIPS mode; the aws-lc-rs 1.19.0 README says it binds AWS-LC-FIPS 4.x (branch `fips-2025-09-12-lts`), while the aws-lc-fips-sys 0.14.4 README still says 3.0.x. The vendored source is what gets built, so a FIPS build would get module 4.x unless `aws-lc-fips-sys` is pinned to an older release.
  - Certificate: the rustls manual cites FIPS 140-3 certificate #4816. The NIST page lists it as "AWS-LC Cryptographic Module (static)", FIPS 140-3, overall level 1, Active, first validated 2024-10-01, sunset 2029-09-30, with no module version stated. Whether module 4.x is covered by #4816 or by a later certificate (or is still in process) has to be checked against the NIST validated and in-process lists before any FIPS claim. A FIPS claim also needs the build and the operating environment to match the module's security policy, and the new crates must pass `cargo deny`.

## Isolation

- **Operating-system sandboxing of shell commands.** An approved command runs with the developer's rights today; a sandbox (a restricted token or AppContainer on Windows, Seatbelt on macOS, namespaces or Landlock on Linux) would confine its files and network the way the file tools are already confined.
- **A policy key for an administrator-owned git hooks path.** anvil's own git calls turn hooks off, so firm-required commit hooks (secret scanning, commit message checks) do not run on `anvil build` commits today; a policy key naming a hooks folder the administrator owns would let anvil pass that folder as `core.hooksPath` instead.

## Distribution and supply chain

- **Signed installers per operating system.** A signature proves who built the binary, which a hash from the same release cannot.
- **Distribution through the firm's own channel.** IT can then pin a reviewed version and roll it out with the policy file, instead of each developer downloading a release.
- **SBOM, `cargo audit`, `cargo deny` and vendored offline builds.** Reviewers need the dependency list and known advisories, and some firms build without network access.

- **Replace `chrono` with `jiff` or `time`.** RUSTSEC-2026-0543 marks chrono unmaintained; `deny.toml` ignores that advisory with a reason until the switch is made.

## Records and secrets

- **Append-only, hash-chained audit log with export.** Transcripts are plain files today; a chained log lets a reviewer prove nothing was removed or changed after the fact.
- **Redaction beyond known shapes.** Redaction catches the built-in key shapes, secret environment values and policy patterns; a password or connection string in an ordinary file is still sent and stored, and detecting those needs format-aware or entropy-based rules that do not flood the model with false positives.
- **Owner-only access lists on Windows at creation.** On Unix anvil creates its files 0600; on Windows they inherit the folder's access list and `anvil doctor` only reports a folder others can open.
- **Operating-system credential store for the endpoint key.** The key lives in the environment or `~/.anvil/.env` behind file permissions; the Windows Credential Manager, the macOS Keychain or the Secret Service would keep it out of files.

## Trust

- **Refusing shell writes to the files that grant power.** File tools never write `.anvil/permissions.toml`, `.mcp.json` or the settings files, but an approved shell command can, on the host or in the container executor, whose workspace mount is writable; the trust digest catches the change before it takes effect. A read-only mount of those files inside the container would close this for the container executor.
- **Hooks and MCP servers in the container executor.** Only the `shell` tool, checks and acceptance commands run in the container; hooks and stdio MCP servers still start on the host.

## The model host

- **A hardening guide for the model server.** anvil does not secure the machine that runs the model; a short guide (network exposure, authentication, prompt retention, logging, updates for vLLM and llama.cpp) would help the team that operates it.
