# Cutting turns per task, round 2

The follow-up to [fewer-turns.md](fewer-turns.md): how its five leftover overheads were measured again, what was changed for each, and what the eval showed. Every number below comes from committed eval result files or from the session logs of the runs that produced them.

## Why

The first round left five things on the table: a lone checkbox tick at the end of build, hand verification on projects without a test runner, extra review rounds, a memory turn after every session, and a simplification pass. Together they were about a third of the turns of a small task. This round takes each to zero or near zero.

## Method

1. Measure first. Before any code changed, the kept session logs of the last run were read for the four counts its result file did not carry, and the acceptance bar was written down.
2. Change only what the measurement points at, one commit per change, in a fixed order.
3. Measure again with one full `eval run --repeat 3` and judge the bar line by line.

### Data sources

- Before: `eval/results/2026-11-04-fde33d6-claude-opus-5-5.json` (69 attempts) and the attempt directories that run kept in `/tmp/anvil-eval-YESn13`: `.anvil/telemetry/*.jsonl` and `.anvil/sessions/<id>/session.jsonl` per attempt.
- After: `eval/results/2026-11-05-2a29fea-claude-opus-5-5.json` (69 attempts, run with `--keep`) and its session logs.
- After the one fix that followed the measured run: `docs/data/2026-11-05-f42a8c4-targeted.json` (15 attempts on 5 tasks) and its session logs.
- Memory summarizer output is not in telemetry, which is content-free, and the home directory of the before-run is gone. Two homes left by earlier aborted runs on the previous binary (26 sessions) show what it writes.

### How the new counts are classified

`eval` now records them per attempt, and `eval compare` prints them per group.

- A lone tick is a build turn whose tool calls are all `complete_step` calls or `edit_file` calls on the session's plan file that change nothing but checkbox characters. Telemetry carries no tool arguments, so this one count is read from the session transcript.
- Review rounds are the session's `review` records. A cap hit is a session that ended with the reason `review_rounds_exhausted`, which telemetry keeps under both the `incomplete` and the `denied` outcome.
- Verify model turns are `turn` records in the verify state.
- The check source is `detected` when a detected or configured command ran, `plan` when only the plan's command ran, `none` when the model verified by hand.
- Memory ran when the session has a `memory` turn; a skip writes a `memory_skipped` record with a fixed reason tag.

## Where the turns went before

Turns per task by state in the before-run:

| state | small (60 attempts) | large (9 attempts) |
|---|---|---|
| plan | 3.1 | 4.7 |
| build | 6.6 | 8.7 |
| review | 1.6 | 1.2 |
| optimize | 0.6 | 1.3 |
| verify | 1.1 | 0 |
| memory | 1.0 | 1.0 |
| total | 14.0 | 16.9 |

What the session logs added, against what the first doc claimed:

- **Lone ticks: 45 of 69 attempts, not 25.** 44 of the 45 ticked the plan's last step, and in all 44 that step was "run the checks". The turn before was a shell call in 35 cases. The model waits to see the check pass before ticking, so the cause is the plan rule that asked for a final check step.
- **Review rounds: 43 of 108 reviews requested changes**, 17 of them after a detected check had already passed. Four attempts hit the 3-round cap (`py-dedupe-formatters` 1 and 3, `py-google-docstrings` 1 and 2); one of them was recorded as `denied`. Of the 50 requests sent back to build, 20 asked to remove files that running the checks had generated (`__pycache__` bytecode 16, a new `Cargo.lock` 4), 19 cited a standing rule (`assert-boundaries` 10, `google-docstrings` 8, `minimal-comments` 1), and 11 named a concrete defect. Seven of those 11 asked to undo behavior changes that boundary assertions, added for the `assert-boundaries` rule, had introduced.
- **Hand verification: 3.1 model turns** on average (2 to 5) on the Markdown task, the two shell tasks and the four Python tasks without tests; the first doc said about 4.
- **Memory: one turn in 69 of 69 sessions.** In the 26 sessions whose output survives, every call produced 2 to 7 facts. No cut separates sessions that produced facts from sessions that produced nothing, so the skip rule was chosen on what the facts were about: the ones worth keeping came from sessions where something pushed back.

## The bar

Set before any change, against the before-run:

- pass rate 100 % in both groups
- turns per task down at least 20 %: small 14.0 to 11.2 or lower, large 16.9 to 13.5 or lower
- input tokens per passed task not above the baseline: small 101k, large 454k
- lone checkbox-tick turns: 0 of 69
- model turns in verify: 0 on every task with a check command, detected or plan-declared
- optimize turns: 0
- attempts that hit the review-round cap: 0, and any that do recorded explicitly
- memory turns per session below 1.0, with the skip reason logged

The large-group turn line looked out of reach by arithmetic before starting: optimize (1.33), lone ticks (0.78) and memory (0.33 to 1.0) at zero give 13.8 to 14.5. It was kept at 13.5.

## What changed

In the order the changes were made:

| change | files |
|---|---|
| eval records `lone_tick_turns`, `review_rounds`, `review_cap_hit`, `verify_model_turns`, `check_source`, `memory_ran` and `memory_skip_reason` per attempt; `compare` prints them per group; the run summary marks a capped attempt even when its check passed | `crates/eval/src/{session,telemetry,results,compare,anvil,runner}.rs` |
| the optimize state is gone: the loop is Plan, Build, Review, Verify, Done. The reviewer may require one simplification in round 1, naming the function or block and what to do, only when behavior cannot change; later it is a note. Approvals carry optional `notes`, shown to the user and never sent to build | `agent/{state,mod,prompts,verdict}.rs`, `prompts/review.md`, `anvil-types/src/config.rs`, `deploy/nuc/anvil-factory.toml`, renderers |
| the reviewer requests changes only for a concrete defect: wrong behavior on a stated input, a missing requirement of the task, or a failing check. Style, standing rules (cited by id) and generated files are notes. With passing check output in front of it, it approves unless it can name the input that still fails | `prompts/review.md`, `anvil-memory/src/rules/mod.rs` |
| commands anvil runs get `PYTHONDONTWRITEBYTECODE=1`, so a test run leaves no bytecode in the session's diff | `tools/shell.rs` |
| `complete_step` tool: ticks one plan step, called beside the step's last edit. When the review approves, anvil ticks whatever is still open with no model turn and emits `PlanUpdated`, which the full-screen plan panel follows | `agent/{complete_step,plan,mod,events}.rs`, `prompts/build.md`, `anvil-cli/src/tui/runner.rs` |
| the plan ends with a `Check:` line, a command or `auto`, and no longer has a step that runs the checks. The planner is told whether a check command was detected. anvil parses the line from the approved plan, asks the shell permission rules, and runs it in Verify after the detected command, through the same path | `prompts/plan.md`, `agent/{plan,mod,checks}.rs`, `tools/mod.rs` |
| checks run before the review whenever a check command exists, whatever the diff size, so the reviewer sees their output and its approval ends the run: Plan, Build, Verify, Review, Done | `agent/{mod,state}.rs` |
| telemetry records where each check came from; a plan-declared command is recorded as program `plan`, because its first word can be a project file | `anvil-telemetry/src/record.rs`, `anvil-cli/src/telemetry.rs` |
| the memory summarizer is skipped when the run ended done, the reviewer never requested changes, no check failed and the final diff is under 50 changed lines. The skip prints `memory: skipped (...)` and writes a `memory_skipped` record. `[memory] always_summarize = true` forces it on | `anvil-cli/src/{commands,memory}.rs`, `anvil-types/src/config.rs`, `anvil-telemetry/src/record.rs` |

Two design choices that differ from the first plan for them:

- `complete_step` was kept over "anvil ticks everything at the end" because the full-screen UI shows progress per step. The price is that a lone tick stays possible, and two happened.
- The review cap needed no new flag in anvil. The reason tag was already in telemetry under both outcomes; the gap was that eval counted a capped attempt as a plain pass.

### Not done, on purpose

- Grep context, `read_file` output size, context budgets and BM25 memory: a separate piece of work.
- A mechanical guard that drops a reviewer's simplification request after round 1. The prompt rule went untested in this run: all six rejections came in round 1 and none asked for a simplification.
- Removing the model's own check runs in build (see what is left).
- Rewording the standing-rules text the builder gets. Three of the six review rejections in the after-run trace back to it; it is the builder's prompt, not one of the five items.
- Any change after the measured run other than the fix described under "What the run broke".

## Results

Before is the `fde33d6` run, after is the `2a29fea` run, 69 attempts each.

| | small before | small after | large before | large after |
|---|---|---|---|---|
| pass rate | 100 % | 100 % | 100 % | 100 % |
| turns / task | 14.0 | 8.8 (-37.5 %) | 16.9 | 11.6 (-31.6 %) |
| 2+ call turns / task | 3.8 (48 % of tool turns) | 2.6 (52 %) | 7.3 (62 %) | 3.2 (41 %) |
| tool calls / task | 14.5 | 10.3 | 28.9 | 15.3 |
| context / turn | 7.2k | 6.1k (-15 %) | 26.9k | 22.4k (-17 %) |
| input tok / passed task | 101k | 54k (-47 %) | 454k | 259k (-43 %) |
| cost / task | $0.229 | $0.119 (-48 %) | $0.636 | $0.381 (-40 %) |

The whole run cost $10.59 against $19.47 and took 18 minutes against 29 at the same `--parallel 4`.

Turns per task by state after the change:

| state | small | large |
|---|---|---|
| plan | 3.0 | 4.8 |
| build | 4.3 | 5.1 |
| review | 1.0 | 1.0 |
| optimize | 0 | 0 |
| verify | 0 | 0 |
| memory | 0.4 | 0.7 |

The overheads, before from the session logs and after from the result file:

| | before | after |
|---|---|---|
| lone tick turns | 45 | 2 |
| reviews (requesting changes) | 108 (43) | 71 (6) |
| review-cap hits | 4 | 0 |
| model turns in verify | 66 | 0 |
| check source: detected, plan, none | 46, 0, 23 | 48, 21, 0 |
| optimize turns | 48 | 0 |
| memory ran | 69 of 69 | 30 of 69 |

Per task, every one of the 23 fell or held: `py-argparse-subcommand` 19.3 to 7.7, `py-google-docstrings` 18.3 to 7.0, `py-fix-failing-pytest` 12.3 to 5.3, `large-ticket-component` 25.3 to 16.0. Three barely moved: `rs-clap-subcommand` 14.7 to 14.7, `sh-dry-run-flag` 12.0 to 11.3 and `py-test-for-bug` 12.7 to 12.0. The first two are the defect described below; the third failed its standing-rule docstring check once in each attempt and went back to build.

### The bar, line by line

| line | bar | result | met |
|---|---|---|---|
| pass rate | 100 % in both groups | 60 of 60 and 9 of 9 | yes |
| turns / task, small | 11.2 or lower | 8.8 | yes |
| turns / task, large | 13.5 or lower | 11.6 | yes |
| input tok / passed task, small | 101k or lower | 54k | yes |
| input tok / passed task, large | 454k or lower | 259k | yes |
| lone tick turns | 0 of 69 | 2 of 69 | no, by 2 |
| model turns in verify | 0 where a check command exists | 0 in 69 attempts, all of which had one | yes |
| optimize turns | 0 | 0 | yes |
| review-cap hits | 0, recorded if any | 0 | yes |
| memory turns per session | below 1.0, reason logged | 0.43 (small 0.40, large 0.67), 39 skips each with its reason | yes |

The large group cleared a line that arithmetic said it would miss. Optimize, memory, review and lone ticks account for 2.6 of its 5.3 saved turns. The other 2.8 came out of build, which fell from 8.7 to 5.1 by more than the ticks: the plan no longer ends with a check step for the model to run, and no large attempt was sent back by the reviewer (2 of 9 before).

Both lone ticks were a `complete_step` call alone in the turn after the model ran its own check (`large-rename-tool` 2, `py-streamlit-metric-picker` 3).

### What the run broke

Four attempts passed `check.sh` but ended `incomplete` on anvil's side with "verification failed 3 times" (`rs-clap-subcommand` 1 and 2, `sh-dry-run-flag` 1 and 3), the same number of unfinished sessions as the four cap hits before. Two more (`rs-clap-subcommand` 3, `rs-cli-flag-count` 1) failed their plan check twice before passing. In all six the plan's own check command was wrong, in one of two ways:

- It ran the built program by path, `$PWD/target/debug/notes`, and the harness sets `CARGO_TARGET_DIR` elsewhere.
- It asserted behavior the planner had chosen and the task had not asked for (an unknown flag must be rejected). The reviewer asked for the opposite, and then the check failed.

In five of the six the builder corrected the `Check:` line in the plan file, which is where the build prompt says to fix a wrong plan, and anvil went on running the command it had parsed at approval. The two that passed in the end got there by putting a `target` entry into the project, a symlink to the real build folder in one case.

This was fixed after the measured run (`f42a8c4`): the line is read again after every build, a changed command goes through the permission rules again, a failed plan check tells the builder the line can be corrected, and the plan prompt gained two rules (check only what the task asked for; run a built program through its build tool). The numbers above do not include the fix.

The fix was then run on the five tasks that share a tag with the failures (`--tags subcommand,cli-flag`, 3 attempts each, $1.84). That result is `docs/data/2026-11-05-f42a8c4-targeted.json`; it is kept out of `eval/results/` because the factory gates against the newest file there and this one holds 5 tasks.

| task | turns in the full run | turns after the fix | sessions ending done |
|---|---|---|---|
| py-argparse-subcommand | 7.7 | 8.3 | 3 of 3 |
| py-cli-flag-lines | 7.3 | 7.7 | 3 of 3 |
| rs-clap-subcommand | 14.7 | 9.7 | 3 of 3 (1 of 3 before) |
| rs-cli-flag-count | 11.7 | 8.3 | 3 of 3 |
| sh-dry-run-flag | 11.3 | 11.0 | 3 of 3 (1 of 3 before) |

All 15 passed and all 15 ended done. No plan check failed, against 16 failed runs in 6 sessions before: no plan ran a program by a path under `target/`, and no check asserted how an unknown flag is handled. What this does not show is the re-read itself. No builder had to correct a `Check:` line, so that path is covered by its loop test only. `sh-dry-run-flag` still took two reviews in all three attempts, because the reviewer still asks the builder to stop rejecting unknown arguments; the check no longer contradicts it. One more lone tick turned up (`rs-cli-flag-count` 2).

### What did not pay off

- The 0-of-69 line for lone ticks. A tool the model calls can still be called alone.
- `PYTHONDONTWRITEBYTECODE` stopped bytecode from test runs, but the plan prompt's own Python example uses `python3 -m py_compile`, which writes it regardless. Three sessions (`py-streamlit-metric-picker`) ended with a `.pyc` file in their diff, which the reviewer noted instead of rejecting.
- A plan-declared check that repeats the detected command (`python3 -m pytest && ...`) runs the tests twice. It costs seconds, not turns.

### What is left

- The model still runs its own checks during build: 102 build turns in 61 of 69 attempts were shell calls only, about 1.5 turns per task, and anvil then runs the check again. The build prompt still says to run the narrowest relevant check as you go.
- Lone ticks: 2 of 69. Zero by construction means anvil owns every tick and the plan panel loses per-step progress.
- The `assert-boundaries` rule in the builder's prompt: three of the six review rejections asked to remove an assertion the builder had added for it on a task that said not to change behavior.
- `py_compile` in the plan prompt's example.
- Whether a stricter reviewer misses real defects cannot be read from `check.sh`: the pass rate did not move, and rejections fell from 43 to 6. In the before-run two `large-ticket-component` attempts were sent back for a regex that accepts a trailing newline; in the after-run all three were approved in round 1.
- Memory still runs after 30 of 69 sessions, by design: those had a rejection, a failed check or a diff of 50 lines or more.

## Tests and checks

- Unit tests added: the lone-tick classifier and transcript reader (`eval::session`), the overhead reader and the flat result fields (`eval::telemetry`, `eval::results`), the `Check:` line parser and the tick functions (`agent::plan`), the `complete_step` tool, `ToolRegistry::permit`, the memory skip rule at each boundary and its override, the telemetry records for a plan-declared check and a memory skip.
- Loop tests added in `agent/mod.rs`: a plan-declared check runs with no model turn in verify; a failing one sends its output to build; a detected and a declared command both run; `auto` and a repeat of the detected command run it once; a command the permission rules refuse is not run; a `Check:` line corrected in the plan file is the one that runs next; `complete_step` ticks without a prompt and approval ticks the rest; approval notes reach the summary and never the builder. The lane tests were rewritten for checks before review.
- Snapshots regenerated and read: the plan, build and review prompts, the reviewer's standing-rules section, anvil-cli's `rule_message_writes_a_valid_entry_and_the_next_prompt_contains_it`, the two `streamed_plan` UI snapshots and eval's compare table. Added: the `complete_step` schema. Removed: the optimize prompt.
- `just check` (fmt, clippy with `-D warnings`, all tests, `scan-keys`, `no-old-name`) passes.
- `factory selftest` passes 15 of 15 steps with binaries built from the branch. The static `anvil` it drives must be rebuilt with the factory: an older one rejects the new `[memory]` table.
- A smoke run before the full run (`eval run --tags links,cli-flag,docstrings --repeat 1 --keep`, 5 tasks, $0.57) was clean: a `Check:` line present and run in every plan, 0 verify model turns, 0 lone ticks, no optimize state, memory skipped with its reason in 4 and run in 1. It did not surface the wrong-check defect; `rs-clap-subcommand` was not in it and `sh-dry-run-flag` passed that time.

A config that still has a `[limits.optimize]` table no longer parses.

## Reproducing

```sh
cargo build --release -p anvil-cli -p eval
target/release/eval run --provider anthropic --repeat 3 --parallel 4 --keep \
    --anvil-bin "$PWD/target/release/anvil"
target/release/eval compare eval/results/2026-11-04-fde33d6-claude-opus-5-5.json \
    eval/results/<after>.json
```

The same notes as in the first doc apply: pass `--anvil-bin` as an absolute path, and delete the kept run's `cargo-target/` afterwards (55 GB this time). This run cost $10.59 at Claude Opus 5.5 prices.
