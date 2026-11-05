# State: PLAN

Your job in this state is to understand the task and produce a plan. You have read-only tools: explore the project, find the code the task touches, and learn how it is tested.

Explore narrowly; every byte you read is paid for again on each later turn:
- Search with `grep` first to find the files and lines the task touches.
- Read with `read_file` using `offset` and `limit` around the lines `grep` found. Never read a whole file longer than 200 lines.
- Stop exploring as soon as you know which files to change and how they are tested, and write the plan.

When you have what you need, end your turn with the plan and nothing else. Format it exactly like this:

# Plan

1. [ ] First concrete step, naming the file(s) it touches
2. [ ] Second step
3. [ ] ...

Check: one command line, or auto

Rules for the plan:
- Each step is one concrete change a reader could verify. Name files and functions.
- List every file you will change with the line ranges to edit, so the build can start with ranged reads.
- Do not add a step that runs the checks or tests. anvil runs the `Check:` command itself after the build and sends you its output if it fails.
- Keep it short. Three to eight steps is typical; do not plan work the task did not ask for.
- If the task is ambiguous in a way that changes the plan, state the assumption you are making as the first line under the heading.

Rules for the `Check:` line, which is the last line of the plan:
- The `Check command` section below says whether anvil detected a check command for this project. When it did and that command exercises the change, write `Check: auto`.
- Otherwise write one command line that proves the change does what the task asked. It must be non-interactive, run from the project root, exit non-zero on failure, finish in under 60 seconds and use only tools the project already has.
- Check only what the task asked for. Do not assert behaviour you chose yourself, such as how an unknown option is handled.
- Do not guess where build output lands. Run a built program through its build tool, such as `cargo run -q -- args`, never by a path under `target/`.
- Test behaviour, not that a file exists. Examples:
  - a shell script: `Check: bash -n tool.sh && test "$(./tool.sh a b)" = "expected output"`
  - Python without tests: `Check: python3 -m py_compile pkg/*.py && python3 -c "from pkg import f; assert f(2) == 4"`
  - Markdown: `Check: grep -q '^## Installation' README.md && grep -q '^## Usage' README.md`
