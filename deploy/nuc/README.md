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
| `Containerfile`, `entrypoint.sh` | The worker image: the Rust toolchain pinned in `rust-toolchain.toml` (read at build time, with its components and the musl target, since the offline container can never download one), `musl-tools`, `just`, `rg`, Python with pytest and pandas, `socat`. The entrypoint bridges the egress proxy's socket to `localhost:3128`. |
| `anvil-factory.toml` | The `factory` profile anvil runs with inside the container: the coder, optional planner and reviewer roles, and permissions that allow writes in the worktree and `shell` for cargo, just and git only. The factory reads it from `main`, so a pull request cannot loosen its own permissions. |

## How a ticket moves

1. `anvil ticket submit` pushes `tickets/<id>` from a laptop. The factory copies the ticket into `tickets/open/` on `factory/state` and deletes the branch; a branch that touches anything else is refused and reported.
2. Every `[service] poll_minutes` the service fast-forwards `factory/state`, checks that `main` is green, and claims the highest-priority oldest ticket (`tickets/claimed/`, with `claimed_at` and the hostname).
3. The worker clones the factory's repository into `~/.factory/work/<id>` with `git clone --local` (hardlinked objects, a real `.git` folder, so git works inside the container) on `factory/<id>` from `main` and runs the implementer in the container with `--network none`; only the allowlisted hosts are reachable through the proxy, and only the provider keys enter the container.
4. Gate A runs `just check` (one retry with the failure appended). Gate C runs the review swarm; only the correctness and safety reviewers block by default (`blocking = true|false` on a `[[reviewers]]` entry overrides it), and `changes` buys another implementer run in a fresh session whose task holds only the findings and the changed files, up to `[budget] review_rounds`, with no release build or eval in between. A reviewer that still fails after the swarm's own reruns pauses the factory with no implementer run spent. Then the release build and Gate B, the eval harness against the newest result on `main` (one retry), run only when a changed path matches `[eval_gate] paths`; otherwise both are skipped and the pull request records `B: eval` as `skipped`. Any budget from `[budget]` rejects with the reason.
5. The branch is pushed, a pull request opened with the gates in run order, a cost table (cost and weighted tokens per implementer run, review round and eval), the eval table and every verdict. The same per-step costs are written to `outcomes.jsonl` and `status.json`. A change to a `[protected]` path, or a new dependency, labels it `needs-human` and parks the ticket in `tickets/waiting/`. Otherwise green CI squash-merges it and the ticket lands in `tickets/merged/` with the pull request number.
6. A rejection writes `tickets/rejected/<id>.toml` and `<id>.notes.md` with the full gate output.

`factory status` shows the queue, the ticket in progress and the last ten outcomes; `factory pause` and `factory resume` toggle the flag the loop checks between tickets. The service pauses itself when `main` is red, when there is no eval result on `main` to gate against, when a reviewer cannot be reached after its reruns, or after two errors in a row, and says so on the webhook. The ticket in hand goes back to `tickets/open/`; it is worked again after `factory resume`.

## Updating

After a change lands on `main`, log into the NUC and run:

```
factory update
```

It sets the pause flag and waits for the ticket in progress to finish (printing its stage every 30 seconds), stops the service, pulls `main` into `~/factory-repo`, rebuilds `anvil` and `factory` in release mode (the binaries in `~/.local/bin` are links into `target/release`; `anvil doctor` and the eval harness use that `anvil`), installs `musl-tools` and the `x86_64-unknown-linux-musl` target if they are missing and builds `anvil` once more for it (`target/x86_64-unknown-linux-musl/release/anvil`, which `[worker] anvil_bin` names: the container mounts it, and a static binary is the only kind the image's older glibc can run; `ldd` proves it static), rebuilds the worker image named in `factory.toml` when its recipe changed (a SHA-256 of `rust-toolchain.toml`, `deploy/nuc/Containerfile` and `deploy/nuc/entrypoint.sh`, stored on the image as the `factory.recipe` label; an image without the label, such as one `just image` or `bootstrap.sh` built, is rebuilt too; `--rebuild-image` forces it), runs `factory selftest` with the new `factory`, and only when every step passes re-installs the unit and starts the service. A failed selftest leaves the service stopped. `factory update --now` skips the wait; a ticket that was in progress then stays in `tickets/claimed/` on `factory/state` and the command says which one; to retry it, run `factory reopen <id>` once the service is stopped. `factory reopen --all` puts every rejected ticket back in `tickets/open/`, without its claim, note or notes file, in one commit pushed to `factory/state`.

## Selftest

```
factory selftest
```

Runs the whole worker pipeline once, for real, in a few seconds, and prints `PASS` or `FAIL` per step with the failing command's output. A bare repository in a temp folder stands in for GitHub, and a scripted `protocol = "fake"` provider (`crates/factory-core/selftest/`) stands in for the models, so it needs no network and no key. Everything else is what the service uses: the worker image, the `[worker] anvil_bin` mounted into it (checked static with `ldd`), the egress proxy with an empty allowlist, the ticket moves on `factory/state`, the ticket's clone (with `git status` and `just no-old-name` run inside the container first, plus a planted untracked file that `just no-old-name` must catch, so a `/work` that is not a complete repository, or a grep that searches nothing, fails here), the toolchain (`cargo --version`, `rustc --version` and `cargo fmt --version` in the container must be the build `rust-toolchain.toml` pins, and a one-line crate must `cargo build --offline` for the host and every pinned target), Gate A (`true` by default; `--check "just check"` for the real one), Gate C with an approving reviewer, the push, and a squash merge into the bare repository's `main`. Gate B is skipped. It then checks that `main` has the change, that the ticket is in `tickets/merged/`, that the clone and branch are gone, and that no secret value appears anywhere in the temp folder. The containers, the volume and the temp folder are removed on every path (`--keep` keeps the folder). It tests the working tree, uncommitted changes included, without touching the index or any branch. CI runs it as the `selftest` job, part of the required `ci` check.

## First run

`main` needs an eval result under `eval/results/` before any ticket can pass Gate B; until the baseline commit reaches `main` the service pauses with "no eval result on main". Run `anvil doctor --fix-github` once so `factory/state` exists with its ruleset, then `factory resume`.

## Troubleshooting

Each entry is a failure the service has shown, what caused it, and the guard that now covers it.

- **Every `cargo` in the sandbox tries to download the toolchain.** `factory update` was run from a snap terminal (VS Code's), so it built and selftested the worker image in the snap's private podman store while the systemd service kept running a stale image. Every podman call now drops a snap's `XDG_*` store variables, and the selftest's "image store" step fails if podman still reads a store under `~/snap/`.
- **VERIFY fails with "could not run `python`".** A standing rule runs `python`, but the image had only `python3`. The image installs `python-is-python3`, and the selftest's "sandbox tools" step runs `python --version` in the container.
- **Every eval task "regresses" with 0 tokens under systemd.** Gate B's wrapper expanded unset passthrough variables under `set -u` and named its proxy socket after `$PWD`. It now forwards only the variables that are set and mounts the ticket's socket; the selftest's "gate B wrapper" step runs it with systemd's bare environment.
- **`factory watch` shows a healthy run as over budget.** The bar counted raw tokens against the weighted `[budget] tokens`. It now shows weighted tokens, the number the budget enforces.
- **A round-2 reviewer asks to undo a round-1 fix.** Later rounds saw only the new diff. Reviewers now receive each earlier round's findings with a note not to request that one be undone.
- **Gate B rejects a branch with no explanation.** The eval kept none of anvil's output when it exited at once. Gate B's summary now quotes the first regressed attempt's stderr and `check.sh` output, and the first Gate B failure goes back to the implementer with that evidence for one more run.
- **A ticket is rejected after two productive review rounds.** The round limit was fixed at two. `[budget] review_rounds` is a setting (default 2); this machine allows 3 rounds in 150 minutes.
- **A clean final review still rejects the ticket.** A reviewer that fails with a provider error counted as `changes`. Reviewers retry a 5xx, a 429 or a dropped connection after 5, 20 and 60 seconds before giving up.
- **One error on a ticket becomes two and pauses the service: "statfs …-proxy.sock: no such file or directory".** A reopened ticket claimed again in the same process got a new proxy socket at the same path, and the old proxy's cleanup then removed it. The old proxy is now stopped before the new one starts.
- **Gate B fails every ticket with "missing fixture" after `main` was reset by hand.** `git reset` moved `~/factory-repo` but the service kept the old binary, whose eval loader did not know the newer task layout. Move the checkout with `factory update`, which rebuilds; after a history rewrite, reset and then run `factory update`, which rebuilds even when the pull finds nothing new.
- **`factory update` stops before building when `factory.toml` gains a key.** The old binary read the pulled config first. When the pull moves `HEAD`, the update builds the new factory first and hands it the rest (`update --now --handed-off`).
- **`anvil update` reports "no asset `anvil-x86_64-unknown-linux-musl`".** `~/.local/bin/anvil` is the static musl build the sandbox uses, and releases published only the glibc and Windows binaries. `release.yml` publishes the musl build too.
- **`anvil update` installs nothing new although a newer release exists.** GitHub lists same-day releases by tag name, and tags end in a hex sha. The update sorts releases by `published_at`.
