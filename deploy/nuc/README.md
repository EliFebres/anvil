# The factory on the NUC

The factory is a user-level systemd service that works tickets from the
`factory/state` branch into squash-merged pull requests on `main`. Everything
here is installed by `bootstrap.sh`:

```
ssh nuc 'bash -s' < deploy/nuc/bootstrap.sh
```

| file | what it is |
|---|---|
| `bootstrap.sh` | One-time, idempotent setup: packages, Rust, the clone at `~/factory-repo`, `~/.factory/env`, the binaries, the worker image, the service. |
| `env.template` | The secrets file, copied to `~/.factory/env` with mode 600. The service loads it itself; the unit never names it. |
| `factory.service` | The systemd user unit `factory install-service` installs. Logs go to journald and `~/.factory/logs/factory.jsonl`. |
| `Containerfile`, `entrypoint.sh` | The worker image: stable Rust, `just`, `rg`, Python with pytest and pandas, `socat`. The entrypoint bridges the egress proxy's socket to `localhost:3128`. |
| `anvil-factory.toml` | The `factory` profile anvil runs with inside the container: the coder and reviewer roles, and permissions that allow writes in the worktree and `shell` for cargo, just and git only. The factory reads it from `main`, so a pull request cannot loosen its own permissions. |

## How a ticket moves

1. `anvil ticket submit` pushes `tickets/<id>` from a laptop. The factory copies the ticket into `tickets/open/` on `factory/state` and deletes the branch; a branch that touches anything else is refused and reported.
2. Every `[service] poll_minutes` the service fast-forwards `factory/state`, checks that `main` is green, and claims the highest-priority oldest ticket (`tickets/claimed/`, with `claimed_at` and the hostname).
3. The worker makes a worktree on `factory/<id>` from `main` and runs the implementer in the container with `--network none`; only the allowlisted hosts are reachable through the proxy, and only the provider keys enter the container.
4. Gate A runs `just check` (one retry with the failure appended), Gate B runs the eval harness against the newest result on `main`, Gate C runs the review swarm (one more implementer round on `changes`). Any budget from `[budget]` rejects with the reason.
5. The branch is pushed, a pull request opened with the gates, the eval table and every verdict. A change to a `[protected]` path, or a new dependency, labels it `needs-human` and parks the ticket in `tickets/waiting/`. Otherwise green CI squash-merges it and the ticket lands in `tickets/merged/` with the pull request number.
6. A rejection writes `tickets/rejected/<id>.toml` and `<id>.notes.md` with the full gate output.

`factory status` shows the queue, the ticket in progress and the last ten outcomes; `factory pause` and `factory resume` toggle the flag the loop checks between tickets. The service pauses itself when `main` is red, when there is no eval result on `main` to gate against, or after two errors in a row, and says so on the webhook.

## Updating

After a change lands on `main`, log into the NUC and run:

```
factory update
```

It sets the pause flag and waits for the ticket in progress to finish (printing its stage every 30 seconds), stops the service, pulls `main` into `~/factory-repo`, rebuilds `anvil` and `factory` in release mode (the binaries in `~/.local/bin` are links into `target/release`), rebuilds the worker image named in `factory.toml`, re-installs the unit and starts the service. `factory update --now` skips the wait; a ticket that was in progress then stays in `tickets/claimed/` on `factory/state` and the command says which one; to retry it, move the file to `tickets/open/`, set `status = "open"` and drop `claimed_at` and `claimed_by`.

## First run

`main` needs an eval result under `eval/results/` before any ticket can pass Gate B; until the baseline commit reaches `main` the service pauses with "no eval result on main". Run `anvil doctor --fix-github` once so `factory/state` exists with its ruleset, then `factory resume`.
