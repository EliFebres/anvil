# tickets

The ticket queue, kept as files so it has git history. A ticket records what went wrong or was missing (`symptom`), the interface wanted (`spec`), an optional synthetic reproduction (`repro`), and the checks the factory must make pass (`acceptance`). `evidence` holds telemetry ids only, never content. The full schema is `anvil_types::Ticket`; print it as JSON Schema with `cargo run -p anvil-types -- --schema`.

This directory is the data boundary between anvil and the factory. The validator refuses any ticket whose `repro` or `spec` contains a path outside this repository or more than 40 lines of code, and `cargo test -p anvil-types` runs that validator over every file here.

## Where tickets live

Ticket state is committed to the long-lived `factory/state` branch, never to `main`. Code reaches `main` only through pull requests; `factory/state` carries only this folder. On `main` only `open/` exists, and it is empty.

| folder | status | written by |
|---|---|---|
| `tickets/draft/` | `draft` | `anvil ticket draft`, per machine, never committed |
| `tickets/open/` | `open` | `anvil ticket submit` pushes the ticket on a `tickets/<id>` branch; the factory copies it into `factory/state` and deletes the branch |
| `tickets/claimed/` | `claimed`, then `in_review` once a pull request exists | the factory, with `claimed_at`, `claimed_by` and later `pr` |
| `tickets/waiting/` | `waiting` | the factory, when a pull request touches a protected path or the ticket kind has no handler yet; `note` says why |
| `tickets/merged/` | `merged` | the factory, with `pr`; never edited again |
| `tickets/rejected/` | `rejected` | the factory, with `note` and the full gate output in `<id>.notes.md` |

The file stem is always the ticket `id`. Lifecycle: `draft` → `open` → `claimed` → `in_review` → `merged`, or `waiting`/`rejected` from any factory step.
