# Cutting turns per task

How the `fewer-turns` work (PR #37, 2026-11-04) was measured, what it changed, and what it left on the table. Every number below comes from committed eval result files or from the session logs of the runs that produced them.

## Why turns

Each model turn re-sends the whole conversation, so a session costs roughly turns times context per turn. The three eval runs before this work (`eval/results/2026-11-04-29295de-claude-opus-5-5*.json`, anvil at main) showed that neither context trimming nor replacing stale tool results with stubs moved the turn count, so turns were the lever.

## Method

1. Measure first: break the pre-change turns down by loop state (plan, build, review, optimize, verify) and by what each turn did (one tool call, several, text only, verdict).
2. Change only what the breakdown pointed at.
3. Measure again with `eval run --repeat 3` on all 23 tasks and compare per group (small and large tasks), accepting only a 25 % fall in turns per task in both groups with no pass-rate loss.

### Data sources

- Pre-change totals: the three completed runs above (23 tasks x 3 attempts each, 100 % pass). They predate per-state recording, so they give turns, tool calls, tokens and cost per attempt but no state split.
- Pre-change state split: 26 complete sessions left in `/tmp/anvil-eval-*` by two aborted runs on the same binary (19 small, all Python or Markdown; 7 large). Each attempt directory holds `.anvil/telemetry/*.jsonl` (one `turn` record per model turn with `state`, `tokens` and `stop_reason`, followed by the turn's `tool_call` records) and `.anvil/sessions/<id>/session.jsonl` (the full transcript).
- After: `eval/results/2026-11-04-fde33d6-claude-opus-5-5.json`, run with `--keep` so its sessions could be read the same way.

### How a turn is classified

A tool call belongs to the `turn` record before it. A turn with one call is "1 call", with two or more "2+ calls", with none "text", or "verdict" when it has none and the state is review or verify. Context per turn is input plus cached prompt tokens. `eval` records this per attempt as `turns_by_state` (commit 59365c5) and `eval compare` prints it per state and per group.

## Where the turns went before

Per-group totals, mean of the three pre-change runs:

| group | turns / task | tool calls / turn | context / turn | input tok / passed task | cost / task |
|---|---|---|---|---|---|
| small (20 tasks) | 18.2 | 1.04 | 9.3k | 169k | $0.348 |
| large (3 tasks) | 22.4 | 1.5 | 20.4k | 456k | $0.619 |

Small tasks by language: Python 18.8 to 21.0 turns, shell 17 to 20, Rust 14 to 18, Markdown 15.

Turns per task by state, from the 26 leftover sessions:

| state | small | large | what the turns were |
|---|---|---|---|
| plan | 3.3 | 4.9 | mostly 2+ call turns, then one text turn (the plan) |
| build | 9.4 | 6.7 | the first build turn re-read `plan.md` in 29 of 30 sessions (alone in 21); the last build turn was a checkbox tick on its own in 15 of 30 |
| review | 1.8 | 1.0 | verdicts; 0.8 re-reviews per small task |
| optimize | 1.7 | 1.1 | almost always "nothing to simplify" |
| verify | 3.1 | 0 | hand verification: `checks::detect` knew only `justfile`, `Cargo.toml`, `pyproject.toml` and `package.json`, so every Python, shell and Markdown fixture got "No test runner was detected"; Rust fixtures ran `cargo test` with no model turn |
| memory | 1.0 | 1.0 | the post-session summarizer that extracts facts for cross-project memory; not a loop state, but a real turn |

Two things the breakdown settled before any code changed:

- Parallel tool calls already worked. The model issued 2+ calls in 23 % of small turns and 41 % of large turns, and the loop ran every call and answered them in one message (now pinned by tests in `agent/mod.rs`, `provider/anthropic.rs` and `provider/openai.rs`). The prompt line was the only missing piece.
- Context per turn was not the limiter. Large tasks ran twice the context of small ones but tokens tracked turns. Context budgets and BM25 memory stayed out.

## What changed

Commits on the branch, in order:

| commit | change | files |
|---|---|---|
| 59365c5 | eval records `turns_by_state` per attempt; `compare` prints a per-state table per group | `crates/eval/src/{telemetry,results,compare}.rs` |
| bf875c0 | base prompt: "Issue every independent tool call in the same turn; reads of different files are always independent." Tests pin the plumbing | `crates/anvil-core/prompts/base.md`, `agent/mod.rs`, `provider/anthropic.rs` |
| 2c5b739 | `outline` tool: functions, types and headings with line ranges via tree-sitter. A plain tool with a good description; no prompt text says when to use it | `crates/anvil-core/src/outline/`, `tools/outline.rs` |
| 27518dd | `read_file` takes `files: [{path, offset?, limit?}]` (up to 20) in one call; each file gets an equal share of the output cap | `tools/read_file.rs` |
| 2242179 | `grep` returns 3 lines of context around each match and the enclosing item's signature, so a hit rarely needs a follow-up read | `tools/grep.rs` |
| 1213b4b | loop lanes by diff size: under 50 changed lines skips Optimize; a small first build with a check command runs the checks before Review and the reviewer sees the diff and the check output together, so its approval ends the run (Build, Verify, Review, Done). Larger diffs keep the full loop | `agent/{mod,state,diff}.rs`, `prompts/review.md` |
| fa1797d | plan prompt: list every file to change with the line ranges to edit | `prompts/plan.md` |
| d9305eb | build intro no longer points the model at `plan.md`; it says the plan above is approved and that a checkbox tick belongs in the same turn as the step's last edit. `checks::detect` finds pytest from `tests/test_*.py`, a root `test_*.py`, `conftest.py` or `pytest.ini`, and runs it as `python3 -m pytest` (bare `pytest` cannot import the project root when `tests/` has no `__init__.py`). `eval run --keep` keeps the attempt directories. `read_file` `files[]` paths count for nested `CLAUDE.md` discovery. A test pins several OpenAI tool calls in one assistant message | `agent/mod.rs`, `agent/checks.rs`, `prompts/build.md`, `crates/eval/src/{main,runner,results}.rs` |

Not done, on purpose: stale-result stubbing, prompt text about `outline`, context budgets, BM25 memory, removing the memory turn, running tool calls concurrently (one turn either way).

## Results

Before is the mean of the three pre-change runs; after is one run of 69 attempts, all passing.

| | small before | small after | large before | large after |
|---|---|---|---|---|
| pass rate | 100 % | 100 % | 100 % | 100 % |
| turns / task | 18.2 | 14.0 (-22.9 %) | 22.4 | 16.9 (-24.8 %) |
| 2+ call turns / task | n/a | 3.8 (48 % of tool turns) | n/a | 7.3 (62 %) |
| tool calls / task | 18.8 | 14.5 | 33.2 | 28.9 |
| context / turn | 9.3k | 7.2k (-23 %) | 20.4k | 26.9k (+31 %) |
| input tok / passed task | 169k | 101k (-41 %) | 456k | 454k |
| cost / task | $0.348 | $0.229 (-34 %) | $0.619 | $0.636 |

Turns per task by state after the change:

| state | small | large |
|---|---|---|
| plan | 3.1 | 4.7 |
| build | 6.6 | 8.7 |
| review | 1.6 | 1.2 |
| optimize | 0.6 | 1.3 |
| verify | 1.1 | 0 |
| memory | 1.0 | 1.0 |

Per task, every one of the 23 fell or held: `rs-fix-failing-test` 15.3 to 10.0, `py-resolve-type-errors` 31.8 to 17.3, `large-permission-trace` 15.9 to 10.0, `large-ticket-component` 31.9 to 25.3. The first build turn read `plan.md` in 0 of 69 attempts.

The 25 % bar was missed narrowly in both groups and merged on the author's call.

### Why the large tasks saved turns but not tokens

Grep results on the 50k-line fixture grew from 1.7 KB to 6.8 KB mean with the context rendering, and multi-file reads doubled the mean read result. Context per turn rose 31 %, which cancelled the turn savings. Capping grep context (fewer matches with context, or no enclosing header on big groups) is the obvious next step for large codebases.

### What is left

- The last build turn is still a checkbox tick on its own in 25 of 69 attempts (about 0.4 turns per task).
- Markdown, shell and the four Python fixtures without tests still verify by hand (about 4 model turns each).
- The reviewer's standing rules forced extra review rounds; 4 attempts hit the 3-round cap and were still counted as passes because the check passed.
- The memory summarizer costs one turn per session.
- Diffs of 50 lines or more still get the simplification pass (0.6 turns per small task, 1.3 per large).

## Tests and checks

- Unit tests: `agent::tests::every_call_of_one_turn_runs_and_answers_in_one_message`, the lane tests in `agent/mod.rs` (check-first, failed first check, large diff) and `agent/state.rs`, `checks::tests::detects_pytest_from_tests_without_pyproject`, `tools::read_file` and `tools::grep` tests, `provider::anthropic::tests::body_carries_several_calls_and_their_results_in_one_message_each`, `provider::openai::tests::body_carries_several_calls_in_one_assistant_message_and_answers_each`.
- Prompt and schema snapshots (insta) for every prompt file and tool schema; the build prompt snapshot and anvil-cli's `rule_message_writes_a_valid_entry_and_the_next_prompt_contains_it` were regenerated for the new build text.
- `just check` (fmt, clippy with `-D warnings`, all tests, `scan-keys`, `no-old-name`) passes.
- `factory selftest` passes 15 of 15 steps (podman, fake provider, no network).
- A one-task smoke run (`eval run --tags types --repeat 1 --keep`) before the full run caught that bare `pytest` fails to import the fixture; it was switched to `python3 -m pytest` before the measurement.

## Reproducing

```sh
cargo build --release -p anvil-cli -p eval
target/release/eval run --provider anthropic --repeat 3 --parallel 4 --keep \
    --anvil-bin "$PWD/target/release/anvil"
target/release/eval compare eval/results/<before>.json eval/results/<after>.json
```

Pass `--anvil-bin` as an absolute path: attempts run with their work directory as the current directory. A full run costs about $26 at Claude Opus 5.5 prices and takes 30 to 90 minutes depending on `--parallel`. With `--keep` the attempt directories stay under `/tmp/anvil-eval-XXXX`; delete its `cargo-target/` afterwards, since the large tasks leave about 56 GB of build artifacts there.
