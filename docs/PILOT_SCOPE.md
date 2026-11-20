# Pilot scope (template)

Copy this file, fill in every bracketed field, and have it signed off before the pilot starts. Keep the pilot small and time-boxed; widen it only through a new version of this document.

## 1. Purpose

[One or two sentences: what the firm wants to learn, for example "whether anvil saves time on maintenance tasks in internal tooling without new risk".]

## 2. Time box

- Start: [date]
- End: [date, at most eight weeks after the start]
- Review meetings: [for example every two weeks, and at the end]

The pilot ends on the end date unless a new version of this document extends it.

## 3. People

- Named supervisor: [name, role]. Owns the pilot, approves participants and repositories, and can stop the pilot at any time.
- Security contact: [name]. Reviews this document, the policy file and the incident log.
- Participants: [names, at most five]. Each has read SECURITY.md and docs/THREAT_MODEL.md.

## 4. Repositories

- In scope: internal-tooling repositories only, listed here: [repository names].
- Out of scope: any repository with client data, client code, credentials, production configuration or regulated data, and any repository not listed above.
- A repository is added only by the supervisor, by updating this list.

## 5. Data

- No client data, no personal data and no production secrets may be read by anvil, pasted into a task, or present in an in-scope repository.
- Model endpoint: [the one endpoint, its operator, and where it runs]. Its prompt retention and access rules: [what the host has confirmed].

## 6. Technical controls

- Managed policy installed by IT on every participant machine, at the system path in SECURITY.md, with at least:
  - `[network] allowed_hosts` set to the one endpoint above, as `host:port`;
  - `allow_yes = false`, `allow_allow_tool = false`, `allow_always = false` and `require_plan_approval = true`;
  - `[permissions] ask = ["shell", "write_file", "edit_file"]`, so every command and write asks a person;
  - `[features] hooks = false`, `mcp = "off"`, `memory = false`, and `[projects] allow_trust = false`;
  - `[shell] mode = "allowlist"` with only the commands the work needs, or `"off"`, and `executor = "container"` where commands must not touch the workstation;
  - `[audit] required = true` and `mirror_dir` set to a folder IT collects;
  - `[data] transcript_dir` set to [folder], `retention_days` to [days], and `transcript` to `"metadata"` (or `"full"` if sessions must be resumable).
- `anvil doctor` run on each machine at the start, and its policy block attached to this document.
- Version of anvil: [commit or release], built by [who] from [where].

## 7. Process

- All output goes through normal pull request review by someone other than the participant. Nothing anvil writes is merged without that review.
- Participants run anvil interactively. No unattended runs.
- Each participant keeps a short log: date, repository, task, outcome, anything surprising.

## 8. Conditions for stopping

The supervisor stops the pilot at once, and the security contact is told, if any of the following happens:

- anvil contacts a host other than the allowed endpoint, or a policy key is found not to be enforced;
- anvil reads or sends a secret, client data or personal data;
- an approved command does damage outside the repository it ran in;
- a participant runs anvil outside the listed repositories or without the policy;
- the model host reports a breach or changes its retention terms.

## 9. Success criteria and decision

- Measures: [for example tasks completed, review comments per change, time saved as estimated by participants, incidents].
- At the end the supervisor writes a one-page result and recommends one of: stop, extend with the same scope, or widen the scope in a new version of this document.

## Sign-off

| role | name | date |
|---|---|---|
| Supervisor | | |
| Security contact | | |
| IT (policy installed) | | |
