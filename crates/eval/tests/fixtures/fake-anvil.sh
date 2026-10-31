#!/usr/bin/env bash
# Stands in for the anvil binary in the eval harness tests.
#
# It checks the argument shape the harness promises, proves the run is
# isolated in a temporary ANVIL_HOME, writes one telemetry record the way
# anvil does, and then succeeds, fails or sleeps as files in that home say.
set -eu

fail() { echo "fake-anvil: $1" >&2; exit 97; }

[ "${1:-}" = "--yes" ] || fail "expected --yes as the first argument, got '${1:-}'"
[ "${2:-}" = "run" ] || fail "expected run as the second argument, got '${2:-}'"
[ $# -eq 3 ] || fail "expected the prompt as one argument, got $# arguments"
[ -n "${ANVIL_HOME:-}" ] || fail "ANVIL_HOME is not set"
[ -f "$ANVIL_HOME/.anvil/config.toml" ] || fail "no config.toml in ANVIL_HOME"
[ -f .anvil/config.toml ] || fail "no project .anvil/config.toml"

mkdir -p "$ANVIL_HOME/.anvil/memory"
echo "eval run" > "$ANVIL_HOME/.anvil/memory/touched"
printf '%s' "$3" > "$ANVIL_HOME/.anvil/last-prompt.txt"
cp .anvil/config.toml "$ANVIL_HOME/.anvil/last-project-config.toml"

session=01ARZ3NDEKTSV4RRFFQ69G5FAV
echo "session $session" >&2
mkdir -p .anvil/telemetry
day=$(date -u +%Y-%m-%d)
printf '{"ts":"%sT10:00:00Z","session":"%s","project":"p","event":"turn","state":"build","provider":"fake","model":"fake-1","tokens":{"input":1000,"output":100,"cached":0},"latency_ms":5,"retries":0,"stop_reason":"end_turn"}\n' "$day" "$session" >> ".anvil/telemetry/$day.jsonl"

if [ -f "$ANVIL_HOME/.anvil/sleep" ]; then
  sleep "$(cat "$ANVIL_HOME/.anvil/sleep")"
fi
if [ -f "$ANVIL_HOME/.anvil/fail" ]; then
  echo "incomplete: scripted" >&2
  exit 2
fi

echo DONE > DONE
echo "done" >&2
