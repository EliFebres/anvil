# Roadmap

What Anvil does not do yet, stated plainly so a reviewer does not assume it. Each item says why it matters. Nothing here is described elsewhere as a control; SECURITY.md says what is enforced today and docs/SECURITY_TESTS.md names the test behind each claim.

## Isolation

- **An operating-system sandbox for the host executor.** In `ask` and `allowlist` mode an approved command runs with the developer's full rights: it can read any file they can read and reach any host they can reach, and the deny categories only match command text. A sandbox (a restricted token or AppContainer on Windows, Seatbelt on macOS, namespaces or Landlock on Linux) would confine a command's files and network the way the file tools are already confined. Today the only boundary for commands is the container executor.
- **Hooks and MCP servers in the container executor.** Only the `shell` tool, checks and acceptance commands run in the container; hooks and stdio MCP servers still start on the host.
- **Refusing shell writes to the files that grant power.** File tools never write `.anvil/permissions.toml`, `.mcp.json` or the settings files, but an approved shell command can, on the host or in the container, whose workspace mount is writable; the trust digest catches the change before it takes effect. A read-only mount of those files inside the container would close this for the container executor.
- **Hard links.** The secret list and the protected-file checks look at names, so a hard link from an ordinary name to a protected or secret file gets past them. Detecting it needs the link count on every open, which the standard library exposes only on Unix.
- **A policy key for an administrator-owned git hooks path.** Anvil's own git calls turn hooks off, so organization-required commit hooks (secret scanning, commit message checks) do not run on `anvil build` commits; a policy key naming a hooks folder the administrator owns would let Anvil pass it as `core.hooksPath`.

## Cryptography and network

- **FIPS-validated cryptography.** Not attempted. What it would take with the current setup, checked against the rustls 0.23.46 manual (`manual::_06_fips`), crate features and `ClientConfig` documentation, the aws-lc-rs 1.19.0 and aws-lc-fips-sys 0.14.4 crate sources and READMEs, the aws-lc-rs platform pages, the reqwest 0.13.5 feature list, the vendored reqwest source and the NIST certificate page (2026-11-19):
  - rustls has a `fips` feature that turns on `aws-lc-rs/fips` and `webpki/aws-lc-rs-fips`. The provider is `rustls::crypto::default_fips_provider()`, and `ClientConfig::fips()` reports whether connections made with a configuration operate in FIPS mode, which covers its TLS settings as well as its algorithms. The feature does not set `require_ems` itself: with the `tls12` feature, `require_ems` defaults to `true` when the configured provider is FIPS-compliant.
  - reqwest 0.13 has no FIPS feature: `rustls` brings the aws-lc-rs provider and `rustls-no-provider` brings none. Its builder uses the process default `CryptoProvider` when one is installed and its own aws-lc-rs default otherwise. anvil would add rustls with `fips` to the workspace (feature unification then builds aws-lc-rs against `aws-lc-fips-sys`), install `default_fips_provider()` as the process default before `http_client` runs, and refuse to start if that fails. Checking `fips()` on the actual configuration needs a rustls `ClientConfig` built by anvil and passed with `tls_backend_preconfigured`, which reqwest documents as having no semver stability.
  - Certificate checks: without `ca_bundle`, reqwest verifies through `rustls-platform-verifier`, which hands chain checks to the operating system on Windows and macOS; with `ca_bundle` it uses webpki with the rustls provider. Which of the two fits the firm's FIPS boundary needs a decision.
  - Building `aws-lc-fips-sys` needs a C and C++ compiler, CMake, Go and Perl on every platform, and on Windows also Ninja and bindgen with libclang, plus NASM on `x86_64-pc-windows-msvc` (the `aarch64-pc-windows-msvc` build uses clang-cl instead). Those two MSVC targets are the only Windows targets with FIPS support; a Windows FIPS build produces DLLs that ship next to the binary.
  - Module version: aws-lc-rs 1.19.0 (the version in `Cargo.lock`) requires `aws-lc-fips-sys` `^0.14.2`, which is not in the lock today because `fips` is off. The newest match, 0.14.4, vendors AWS-LC with `AWSLC_VERSION_NUMBER_STRING` `4.2.0` and reports `AWS-LC FIPS 4.2.0` in FIPS mode; the aws-lc-rs 1.19.0 README says it binds AWS-LC-FIPS 4.x (branch `fips-2025-09-12-lts`), while the aws-lc-fips-sys 0.14.4 README still says 3.0.x. The vendored source is what gets built, so a FIPS build would get module 4.x unless `aws-lc-fips-sys` is pinned to an older release.
  - Certificate: the rustls manual cites FIPS 140-3 certificate #4816. The NIST page lists it as "AWS-LC Cryptographic Module (static)", FIPS 140-3, overall level 1, Active, first validated 2024-10-01, sunset 2029-09-30, with no module version stated. Whether module 4.x is covered by #4816 or by a later certificate (or is still in process) has to be checked against the NIST validated and in-process lists before any FIPS claim. A FIPS claim also needs the build and the operating environment to match the module's security policy, and the new crates must pass `cargo deny`.
- **Proxy and TLS settings without a policy.** `[network] proxy`, `ca_bundle`, `client_cert` and `client_key` exist only in the managed policy, so an unmanaged developer behind a corporate proxy cannot use one (Anvil ignores `HTTPS_PROXY`); a user-level equivalent would serve them without giving a repository a way in.
- **HTTP MCP gaps.** When a server ends its session and answers 404, Anvil reports the error and the server stays unusable until the next session; the specification asks the client to start a new one. Anvil also does not open the optional GET stream, answers requests a server sends inside a response with "method not found", and does not support OAuth authorization (a definition's static headers only).
- **MCP protocol revision 2026-07-28.** Anvil speaks 2025-06-18 over stdio and HTTP. The newer revision removes protocol-level sessions and the GET stream and adds per-request headers (`Mcp-Method`, `Mcp-Name`); servers that support only it would need those changes.

## Identity and secrets

- **Operating-system keychain for the endpoint key.** The key lives in the environment or `~/.anvil/.env`, protected by file permissions; the Windows Credential Manager, the macOS Keychain or the Secret Service would keep it out of files.
- **Short-lived tokens from a vault.** Anvil reads one static key per provider. Fetching a short-lived token from the organization's vault (and refreshing it) would limit what a leaked key is worth.
- **Single sign-on identity in audit records.** Records name the operating-system user and host from environment variables, which the person running Anvil can set. An identity from the organization's single sign-on (or at least from the operating system's account database) would make the record attributable.
- **Redaction beyond known shapes.** Redaction catches the built-in key shapes, secret environment values and the policy's patterns; a password or connection string in an ordinary file is still sent and stored. Format-aware or entropy-based rules could catch more without flooding the model with false positives.
- **Owner-only access lists on Windows at creation.** On Unix Anvil creates its files 0600; on Windows they inherit the folder's access list and `anvil doctor` only reports a folder others can open.

## Records

- **Audit log export to syslog, the Windows Event Log and OpenTelemetry.** The audit log is files today, in the session folder and the policy's mirror; sending each record to the organization's collector as it is written would move the evidence off the workstation without a shared folder.
- **Shell-caused file changes in the audit log.** Only `write_file` and `edit_file` report the files they change; a snapshot of the tree before and after each command would record what a shell command changed.

## Distribution and assurance

- **Signed Windows installers, and signed binaries on every platform.** Releases carry checksums, an SBOM and build provenance attestations, but no publisher code signature, so Windows SmartScreen and macOS Gatekeeper treat the binaries as unsigned, and Windows Smart App Control can block them.
- **Distribution through the organization's own channel.** A package (MSI, deb, rpm, pkg) that configuration management installs would let IT pin a reviewed version instead of copying a file.
- **An independent penetration test.** Every control is backed by a test in this repository, written by the same people who wrote the code. No outside party has tested Anvil or assessed it against any standard.
- **Replace `chrono` with `jiff` or `time`.** RUSTSEC-2026-0543 marks chrono unmaintained; `deny.toml` ignores that advisory with a reason until the switch is made.

## Self-hosted endpoints

- **Provider tolerance for self-hosted servers.** Fall back to a client-side token estimate when a server reports no usage, support both `max_tokens` and `max_completion_tokens`, accept a stream that ends without `finish_reason`, use a long request timeout, retry on connection refused while a model loads, and make `api_key_env` optional. Without this, budgets and statistics read zero and some vLLM and llama.cpp servers fail on the first turn.
- **`anvil doctor` as the acceptance test for an endpoint.** A streaming round trip, one real tool call, a usage check, a warning when the server's context is smaller than the configured `context_window`, and latency, so a reviewer has one command that proves an endpoint works before a pilot starts.
- **A hardening guide for the model server.** Anvil does not secure the machine that runs the model; a short guide (network exposure, authentication, prompt retention, logging, updates for vLLM and llama.cpp) would help the team that operates it.

## Documentation

- **Screenshots of the current UI.** `docs/ui-startup.png` predates the startup screen's `policy` row and shows hosted model names; the other three screenshots show a hosted model name in the status line. They should be retaken against a self-hosted endpoint.
- **The startup tagline.** The full-screen startup screen still reads "Eli's personal coding agent that constantly improves itself" (`TAGLINE` in `crates/anvil-cli/src/tui/welcome.rs`), which predates the one-line description; changing it is a code change with snapshot updates, left out of the documentation phase.
