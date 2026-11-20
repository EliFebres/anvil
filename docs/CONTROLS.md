# Control mapping

Which NIST SP 800-53 and SP 800-171 controls Anvil's features can support, the feature that supports each, and the claim in [SECURITY_TESTS.md](SECURITY_TESTS.md) whose test proves the feature.

**Anvil holds no certification, has had no independent assessment, and does not by itself satisfy any control.** A control is satisfied by an organization's system as a whole: its policies, people, processes and technology. Anvil is one component on a workstation, and this page says which control objectives its features can contribute to, so an assessor knows what to look at. Whether a control is met in a given system is the organization's determination.

## Sources and revisions

- **NIST SP 800-53 Revision 5**, "Security and Privacy Controls for Information Systems and Organizations", <https://doi.org/10.6028/NIST.SP.800-53r5> (the publication page <https://csrc.nist.gov/pubs/sp/800/53/r5/upd1/final> lists it as published September 2020, including updates as of December 10, 2020, and notes release 5.2.0 of the control catalog on August 27, 2025). Identifiers and titles below were checked against the Revision 5 PDF on 2026-11-19. Titles are written in title case here; the publication prints them in capitals.
- **NIST SP 800-171 Revision 3**, "Protecting Controlled Unclassified Information in Nonfederal Systems and Organizations", May 2024, <https://doi.org/10.6028/NIST.SP.800-171r3> (publication page <https://csrc.nist.gov/pubs/sp/800/171/r3/final>). Identifiers, titles and each requirement's source control were checked against the Revision 3 PDF on 2026-11-19.
- **This page maps to SP 800-171 Revision 3.** Revision 3 numbers requirements `03.FF.RR` (for example `03.01.02`) and has 17 families, including Supply Chain Risk Management (`03.17`). Revision 2 numbered them `3.F.R` with a different breakdown, so a Revision 2 identifier cannot be read off this table.

## Families covered

| SP 800-53 family | SP 800-171 Rev. 3 family | Anvil's part |
|---|---|---|
| AC: Access Control | 03.01 Access Control | Permission rules, the file boundary, tightening-only layers, least functionality of the agent. |
| AU: Audit and Accountability | 03.03 Audit and Accountability | The hash-chained audit log, its mirror, fail-closed writes, retention. |
| CM: Configuration Management | 03.04 Configuration Management | The managed policy, its ownership check, allowlisted programs, removed features. |
| IA: Identification and Authentication | 03.05 Identification and Authentication | Handling of the endpoint key; mutual TLS. |
| SC: System and Communications Protection | 03.13 System and Communications Protection | Egress allowlist, TLS, no redirects, the container executor. |
| SI: System and Information Integrity | 03.14 System and Information Integrity | Input validation, redaction, untrusted-data delimiters, integrity of records and releases. |
| SR: Supply Chain Risk Management | 03.17 Supply Chain Risk Management | Release provenance, SBOM, dependency gate, pinned build. |

## SP 800-53 Revision 5

| control | Anvil feature | claims |
|---|---|---|
| AC-3 Access Enforcement | Every tool call passes one permission check with a fixed order; file tools are confined to the workspace and refused on secret files and on the files that grant power. | `PERM-1`, `PERM-2`, `FS-1`, `FS-3`, `FS-5` |
| AC-4 Information Flow Enforcement | Requests go only to allowed `host:port` pairs; keys only to their own host; tool output is redacted before it flows to the model; programs start without keys. | `NET-1`, `NET-4`, `RED-1`, `KEY-4` |
| AC-6 Least Privilege | Under a policy the shell, MCP, hooks, skills, memory, `anvil build`, delegation and repository trust are off by default; subagents get no more than the session; a repository can only tighten. | `SH-1`, `PERM-4`, `TRUST-1`, `TRUST-5` |
| AC-6(9) Log Use of Privileged Functions | Every permission decision is recorded with its source; a policy refusal names the key. | `AUD-8` |
| AU-2 Event Logging | Model requests, tool calls, decisions, results, file changes, plan answers and policy refusals are recorded. | `AUD-7`, `AUD-8`, `AUD-9` |
| AU-3 Content of Audit Records | Each record carries time, session, agent, user, host, version, binary hash, policy hash and the event. | `AUD-1`, `AUD-7` |
| AU-5 Response to Audit Logging Process Failures | With `[audit] required`, a session whose log cannot be written does not start, and after a failed write nothing further runs. | `AUD-5`, `AUD-6` |
| AU-8 Time Stamps | Each record has a UTC `ts` from the workstation clock. | `AUD-1` |
| AU-9 Protection of Audit Information | Hash chain over a canonical form; a tampered, cut-and-resealed or replaced file is detected (an end cut or full rewrite needs the mirror); `anvil audit verify`. | `AUD-1`, `AUD-2`, `AUD-3`, `AUD-6` |
| AU-9(2) Store on Separate Physical Systems or Components | `[audit] mirror_dir` writes every record to a second location the organization collects. | `AUD-4` |
| AU-11 Audit Record Retention | `retention_days` and `anvil purge` delete session folders by age; the mirror is never deleted by Anvil. | `DATA-4` |
| AU-12 Audit Record Generation | Every session and build opens its log before the first model request, under the policy by default. | `AUD-5`, `AUD-10` |
| CM-2 Baseline Configuration | Two shipped example policies (`policies/restricted.toml`, `policies/standard.toml`) that parse in CI. | `POL-3` |
| CM-5 Access Restrictions for Change | The policy must be owned and writable only by administrators; file tools never write the files that grant power. | `POL-2`, `FS-5` |
| CM-6 Configuration Settings | The policy outranks every lower layer; a conflict is an error naming the key; `anvil policy show` prints every setting and its layer. | `POL-4`, `POL-7`, `POL-8` |
| CM-7 Least Functionality | Removed features (self-update, webhooks, tickets, remote skill install); every optional feature off by default under a policy. | `SH-1`, `POL-5` |
| CM-7(2) Prevent Program Execution | Shell mode `off`; untrusted repositories start no hook or MCP server. | `SH-1`, `TRUST-2` |
| CM-7(5) Authorized Software: Allow-by-exception | Allowlist mode starts only listed programs with plain arguments, without a shell; MCP servers allowlisted by name and program or URL. | `SH-2`, `SH-3`, `POL-5` |
| CM-8 System Component Inventory | Each release carries a CycloneDX SBOM of the binary's dependencies. | `SUP-4` |
| CM-11 User-installed Software | Package installs (`pip`, `npm`, `cargo`, `gem`) are a built-in deny category. | `SH-4` |
| IA-5 Authenticator Management | Keys come only from the environment or `~/.anvil/.env`, never from configuration or the repository; they go only to their host and are redacted from errors and logs. | `KEY-1`, `KEY-2`, `NET-4`, `RED-4` |
| IA-5(7) No Embedded Unencrypted Static Authenticators | `anvil scan-keys` fails when a tracked file holds a key shape or a `.env` value. | `KEY-6` |
| SC-7 Boundary Protection | One HTTP client checks every request against the allowed set; no redirects; proxy only when the policy names one. | `NET-1`, `NET-2`, `NET-5`, `NET-7` |
| SC-7(5) Deny by Default: Allow by Exception | Under a policy only `[network] allowed_hosts` is reachable. | `NET-1`, `NET-8` |
| SC-8 Transmission Confidentiality and Integrity | https except loopback, checked at load, session start and every request. | `NET-3` |
| SC-8(1) Cryptographic Protection | TLS through rustls; the policy's CA bundle replaces the system store; mutual TLS. Not FIPS-validated (see below). | `NET-3`, `NET-6` |
| SC-39 Process Isolation | The container executor runs commands without network, root, capabilities or host folders besides the workspace. | `SH-5`, `SH-6` |
| SI-7 Software, Firmware, and Information Integrity | Release provenance attestations and checksums; audit records sealed by hash. | `SUP-4`, `AUD-1` |
| SI-10 Information Input Validation | The policy, configuration and repository files are validated with unknown keys refused; paths are resolved and checked before use; shell commands are parsed conservatively in allowlist mode. | `POL-3`, `TRUST-1`, `FS-1`, `FS-2`, `SH-2` |
| SI-12 Information Management and Retention | `[data] transcript` decides what is kept; retention by age; redaction before anything is stored. | `DATA-1`, `DATA-4`, `RED-1` |
| SR-3 Supply Chain Controls and Processes | `cargo deny` gate (advisories, licenses, banned crates, crates.io only); pinned actions; least-privilege workflows; locked builds. | `SUP-1`, `SUP-2`, `SUP-3` |
| SR-4 Provenance | SLSA build provenance attestation for every released file. | `SUP-4` |
| SR-11 Component Authenticity | `SHA256SUMS` and `gh attestation verify` let IT check a binary came from the tagged release workflow. | `SUP-4` |

## SP 800-171 Revision 3

The source controls in brackets are the ones Revision 3 itself names for each requirement.

| requirement | Anvil feature | claims |
|---|---|---|
| 03.01.02 Access Enforcement [AC-03] | The permission check and the file boundary. | `PERM-1`, `FS-1`, `FS-3`, `FS-5` |
| 03.01.03 Information Flow Enforcement [AC-04] | Egress allowlist, key binding, redaction, cleared child environment. | `NET-1`, `NET-4`, `RED-1`, `KEY-4` |
| 03.01.05 Least Privilege [AC-06, AC-06(01), AC-06(07), AU-09(04)] | Restrictive policy defaults; subagents confined like the session; tightening-only repository rules. | `SH-1`, `PERM-4`, `TRUST-1` |
| 03.03.01 Event Logging [AU-02] | The audit log's event types. | `AUD-7`, `AUD-8` |
| 03.03.02 Audit Record Content [AU-03, AU-03(01)] | The fields of each record. | `AUD-1` |
| 03.03.03 Audit Record Generation [AU-11, AU-12] | A log per session and build, opened before the first request; retention by age. | `AUD-10`, `DATA-4` |
| 03.03.04 Response to Audit Logging Process Failures [AU-05] | Fail closed under `[audit] required`. | `AUD-5`, `AUD-6` |
| 03.03.07 Time Stamps [AU-08] | UTC time stamp per record. | `AUD-1` |
| 03.03.08 Protection of Audit Information [AU-09, AU-09(04)] | Hash chain, verifier, mirror. | `AUD-1`, `AUD-2`, `AUD-4` |
| 03.04.01 Baseline Configuration [CM-02] | Shipped example policies. | `POL-3` |
| 03.04.02 Configuration Settings [CM-06] | The policy outranks every lower layer. | `POL-4`, `POL-7` |
| 03.04.05 Access Restrictions for Change [CM-05] | Administrator-owned policy; protected configuration files. | `POL-2`, `FS-5` |
| 03.04.06 Least Functionality [CM-07, CM-07(01)] | Features off by default under a policy; removed features. | `SH-1`, `POL-5` |
| 03.04.08 Authorized Software: Allow by Exception [CM-07(05)] | Allowlist shell mode; MCP allowlist. | `SH-2`, `SH-3`, `POL-5` |
| 03.04.10 System Component Inventory [CM-08, CM-08(01)] | Release SBOM. | `SUP-4` |
| 03.05.12 Authenticator Management [IA-05] | Key handling and scanning. | `KEY-1`, `KEY-2`, `KEY-6`, `NET-4` |
| 03.13.01 Boundary Protection [SC-07] | One checked HTTP client, no redirects. | `NET-1`, `NET-2`, `NET-7` |
| 03.13.06 Network Communications: Deny by Default: Allow by Exception [SC-07(05)] | `[network] allowed_hosts`. | `NET-1`, `NET-8` |
| 03.13.08 Transmission and Storage Confidentiality [SC-08, SC-08(01), SC-28, SC-28(01)] | TLS in transit. At rest, Anvil only restricts access (owner-only files on Unix) and does not encrypt; storage encryption is the organization's (disk encryption). | `NET-3`, `NET-6`, `DATA-2` |
| 03.14.08 Information Management and Retention [SI-12] | Transcript modes, retention, redaction. | `DATA-1`, `DATA-4`, `RED-1` |
| 03.17.03 Supply Chain Requirements and Processes [SR-03] | Dependency gate, pinned workflows, provenance. | `SUP-1`, `SUP-2`, `SUP-4` |

## What Anvil does not provide

These are named so an assessor does not look for them. The ones Anvil could add are in [ROADMAP.md](ROADMAP.md).

- **SC-13 Cryptographic Protection / 03.13.11 Cryptographic Protection.** Anvil's TLS uses rustls with the aws-lc-rs provider in its default (non-FIPS) mode. It makes no FIPS 140 claim.
- **SC-28 Protection of Information at Rest** beyond file permissions: transcripts and audit logs are plain files; encrypt the disk.
- **CM-14 Signed Components.** Binaries are not code-signed; provenance attestations and checksums are provided instead.
- **IA-2 identity in records.** Audit records name the operating system user from the environment, not a single sign-on identity, and Anvil has no authentication of its own.
- **AU-6 review and AU-4 storage capacity**, and any export to a SIEM: the organization collects and reviews the mirror; syslog and event log export are not built.
- **Operating system process isolation on the host** (SC-39 outside the container executor).
