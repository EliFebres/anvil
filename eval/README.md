# eval

A fixed task set that scores a model or an anvil build. Results are committed under `results/`, so a change to a prompt, a tool or a model can be gated against the last result on `main`.

## A task

Each folder under `tasks/<name>/` holds:

| part | what it is |
|---|---|
| `fixture/` | A tiny repository the task starts from. The harness copies it to a temp folder and gives it a git history. Never put `.git/` or `.anvil/` in it. |
| `task.md` | The prompt anvil receives, verbatim, as one argument to `anvil --yes run`. At most 8 KB. |
| `check.sh` | Exit 0 means pass. It runs with the fixture copy as its working directory, from the task folder, so the model never sees it. |
| `meta.toml` | `tags` (the first is the language: `python`, `rust`, `shell`, `markdown`), `timeout_s` (for the anvil run, and again for the check) and `max_tokens` (anvil's session budget). |
| `expected/` | Optional reference files the check diffs against, reached as `$EVAL_TASK_DIR/expected/...`. |
| `solution/` | Files that, copied over the fixture, make the check pass. Not shown to the model; `just eval-tasks` uses it to prove the check both ways. |

`check.sh` runs under bash (Git Bash on Windows) with `EVAL_TASK_DIR` (the task folder, forward slashes), `EVAL_PYTHON` (a Python 3 interpreter) and `CARGO_TARGET_DIR` (one build folder shared by every attempt of a run) in its environment.

### Writing one

- Names are kebab-case and start with the language: `py-`, `rs-`, `sh-`, `md-`.
- The check must fail on the untouched fixture and pass on `fixture/` plus `solution/`. `just eval-tasks` runs that for every task; run it before committing a new one.
- Compare files with `diff --strip-trailing-cr`, never `cmp`, and run Python as `"$PY" -m pytest`, so the check behaves the same on Windows and Linux. Files under `eval/` are forced to LF by `.gitattributes`.
- Rust fixtures carry an empty `[workspace]` table so cargo does not claim them for this repository. Keep them std-only unless the task is about a crate.
- A task should take a good model under ten minutes. Say exactly what the check needs (file names, output shape) in `task.md`; the check should test behavior, not wording.

## Running

```
eval run [--provider NAME] [--anvil-bin PATH] [--tags a,b] [--repeat N] [--parallel N]
eval compare <a.json> <b.json>
eval gate --base <main.json> --candidate <branch.json>
eval baseline [--anvil-bin PATH] [--parallel N]
```

`run` scores every task (or those with one of `--tags`) and writes `results/<date>-<git sha>-<model>.json`. `--repeat 3` reports a pass rate with its spread, because single runs lie. `--provider` forces the coder provider; it must be a `[providers.*]` entry in `~/.anvil/config.toml`. The anvil binary is `--anvil-bin`, else `anvil` on `PATH`, else `target/release/anvil`, else `target/debug/anvil`. `--parallel` defaults to `[eval] parallel` in `factory.toml`.

`compare` prints both runs per task with `PASS 2/3`-style verdicts, the totals (tasks passing, pass rate, tokens, cost, cost per passed task) and the tasks that flipped.

`gate` is the factory's regression rule: a task passes a run when a strict majority of its attempts passed; the candidate fails (exit 2) when more than one task that passed in base fails in the candidate. Tasks the candidate did not run are warned about, not counted.

`baseline` runs every task three times on the `coder` role from `factory.toml`, writes the result and commits that one file. A result on `main` must exist before the factory starts.

Exit codes: 0 ok, 1 error, 2 gate failed.

## Isolation and cost

Every run gets a temporary `ANVIL_HOME` seeded with copies of `~/.anvil/config.toml`, `.env`, `rules.toml` and `checks/`. API keys and prices flow in; the memory and telemetry anvil writes never reach the real home. Each attempt gets its own fixture copy with a project `.anvil/config.toml` that sets `roles.coder` and `limits.session_tokens`.

anvil prints no usage, so the harness reads the attempt's `.anvil/telemetry/*.jsonl`, sums the `turn` and `compacted` records of the session, and prices them with each provider's `price_per_mtoken`. A run that used an unpriced provider reports its cost as `n/a`.

## Prerequisites

`anvil` (built or on `PATH`), `git`, `cargo`, bash (Git for Windows on Windows), and a Python 3 with `pytest` and `pandas` installed for the Python tasks.

## The result file

`schema` is 1. The file holds the run (`started_at`, `git_sha`, `provider`, `model`, `runner`, `repeat`, `parallel`, `tags`), one entry per task with its attempts (`passed`, `status`, `check_exit`, `session`, `tokens`, `cost_usd`, `anvil_ms`, `check_ms`, `wall_ms`, output tails) and `totals`. `crates/eval/src/results.rs` is the schema; its snapshot test pins the shape.

## The tasks

| task | what the model must do |
|---|---|
| py-cli-flag-lines | add `--lines` to an argparse word counter |
| py-fix-failing-pytest | fix `slugify` so two failing tests pass, tests untouched |
| py-google-docstrings | Google docstring on every public function in a module |
| py-dedupe-formatters | collapse three copy-pasted formatters without changing output |
| py-test-for-bug | write a test that exposes a `parse_duration` bug, then fix it |
| py-pandas-add-column | read a CSV with pandas, add a `total` column, write it back |
| py-pandas-monthly-summary | derive a month column and write per-month totals |
| py-streamlit-metric-picker | add a sidebar selectbox and `st.metric` to a Streamlit page |
| py-resolve-type-errors | fix four listed mypy errors without changing behavior |
| py-argparse-subcommand | wire a `done <n>` subcommand into a todo CLI |
| rs-thiserror-enum | replace `Result<_, String>` with a `thiserror` enum |
| rs-clap-subcommand | wire `remove <n>` into a clap derive CLI |
| rs-fix-failing-test | fix an off-by-one so `cargo test` passes |
| rs-dedupe-sums | extract one helper from three copy-pasted loops |
| rs-doc-comments | document six public items so `-D warnings` builds |
| rs-cli-flag-count | add `--count` to a std-only grep clone |
| rs-test-for-bug | write a test for a panic at the exact word count, then fix it |
| sh-quote-backup-script | fix unquoted variables so names with spaces work |
| sh-dry-run-flag | add `--dry-run` to a cleanup script |
| md-fix-readme-links | repoint five relative links at renamed files |
