# The factory on the NUC

The factory is a user-level systemd service that works tickets from the
`factory/state` branch into squash-merged pull requests on `main`. Everything
here is installed by `bootstrap.sh`:

```
ssh nuc 'bash -s' < deploy/nuc/bootstrap.sh
```

| file | what it is |
|---|---|
| `bootstrap.sh` | One-time, idempotent setup: packages, Rust with the `x86_64-unknown-linux-musl` target, the clone at `~/factory-repo`, `~/.factory/env`, the binaries (anvil twice: for the host, and static for the container), the worker image, the service. |
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

It sets the pause flag and waits for the ticket in progress to finish (printing its stage every 30 seconds), stops the service, pulls `main` into `~/factory-repo`, rebuilds `anvil` and `factory` in release mode (the binaries in `~/.local/bin` are links into `target/release`; `anvil doctor` and the eval harness use that `anvil`), installs `musl-tools` and the `x86_64-unknown-linux-musl` target if they are missing and builds `anvil` once more for it (`target/x86_64-unknown-linux-musl/release/anvil`, which `[worker] anvil_bin` names: the container mounts it, and a static binary is the only kind the image's older glibc can run; `ldd` proves it static), rebuilds the worker image named in `factory.toml`, runs `factory selftest` with the new `factory`, and only when every step passes re-installs the unit and starts the service. A failed selftest leaves the service stopped. `factory update --now` skips the wait; a ticket that was in progress then stays in `tickets/claimed/` on `factory/state` and the command says which one; to retry it, move the file to `tickets/open/`, set `status = "open"` and drop `claimed_at` and `claimed_by`.

## Selftest

```
factory selftest
```

Runs the whole worker pipeline once, for real, in a few seconds, and prints `PASS` or `FAIL` per step with the failing command's output. A bare repository in a temp folder stands in for GitHub, and a scripted `protocol = "fake"` provider (`crates/factory-core/selftest/`) stands in for the models, so it needs no network and no key. Everything else is what the service uses: the worker image, the `[worker] anvil_bin` mounted into it (checked static with `ldd`), the egress proxy with an empty allowlist, the ticket moves on `factory/state`, the worktree, Gate A (`true` by default; `--check "just check"` for the real one), Gate C with an approving reviewer, the push, and a squash merge into the bare repository's `main`. Gate B is skipped. It then checks that `main` has the change, that the ticket is in `tickets/merged/`, that the worktree and branch are gone, and that no secret value appears anywhere in the temp folder. The containers, the volume and the temp folder are removed on every path (`--keep` keeps the folder). It tests the working tree, uncommitted changes included, without touching the index or any branch. CI runs it as the `selftest` job, part of the required `ci` check.

## First run

`main` needs an eval result under `eval/results/` before any ticket can pass Gate B; until the baseline commit reaches `main` the service pauses with "no eval result on main". Run `anvil doctor --fix-github` once so `factory/state` exists with its ruleset, then `factory resume`.
