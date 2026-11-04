//! Reads what telemetry cannot tell: what a session's tool calls changed.
//!
//! Telemetry is content-free, so a tool call there is a name and a size.
//! The session transcript `<cwd>/.anvil/sessions/<id>/session.jsonl` holds
//! the arguments, which is what it takes to tell a build turn that only
//! ticked plan checkboxes from one that did work.

use std::path::Path;

use serde::Deserialize;

use crate::error::EvalError;

/// The transcript file inside a session folder.
const SESSION_FILE: &str = "session.jsonl";

/// The plan file inside a session folder.
const PLAN_FILE: &str = "plan.md";

/// The tool anvil offers for marking a plan step done.
const COMPLETE_STEP: &str = "complete_step";

/// The tool a model can also tick a checkbox with.
const EDIT_FILE: &str = "edit_file";

/// The fields of a transcript line the harness reads.
#[derive(Debug, Deserialize)]
struct Line {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    message: Option<Message>,
}

/// One message of the conversation.
#[derive(Debug, Deserialize)]
struct Message {
    role: String,
    #[serde(default)]
    tool_calls: Vec<Call>,
}

/// One tool call of an assistant message.
#[derive(Debug, Deserialize)]
struct Call {
    name: String,
    #[serde(default)]
    arguments_json: String,
}

/// The arguments of an `edit_file` call.
#[derive(Debug, Deserialize)]
struct EditArgs {
    path: String,
    old: String,
    new: String,
}

/// Counts the lone checkbox-tick turns of one session.
///
/// # Arguments
///
/// * `session_dir` - `<cwd>/.anvil/sessions/<id>` of the fixture copy.
///
/// # Returns
///
/// The count; 0 when the session has no transcript.
///
/// # Errors
///
/// When the transcript exists and cannot be read.
pub fn read_lone_tick_turns(session_dir: &Path) -> Result<u32, EvalError> {
    let path = session_dir.join(SESSION_FILE);
    if !path.is_file() {
        return Ok(0);
    }
    let text =
        std::fs::read_to_string(&path).map_err(|source| EvalError::io("reading", &path, source))?;
    Ok(lone_tick_turns(&text))
}

/// Counts the build turns of a transcript that only ticked plan checkboxes.
///
/// A turn is a lone tick when it made at least one tool call and every call
/// was either `complete_step` or an `edit_file` on the session's plan file
/// that changed nothing but checkbox characters.
///
/// # Arguments
///
/// * `transcript` - The text of a `session.jsonl`.
///
/// # Returns
///
/// The count. Lines that do not parse are skipped.
pub fn lone_tick_turns(transcript: &str) -> u32 {
    let mut state = String::new();
    let mut count = 0;
    for line in transcript.lines().filter(|line| !line.trim().is_empty()) {
        let Ok(line) = serde_json::from_str::<Line>(line) else {
            continue;
        };
        if line.kind == "state" {
            state = line.state.unwrap_or_default();
            continue;
        }
        let Some(message) = line.message.filter(|m| m.role == "assistant") else {
            continue;
        };
        let calls = &message.tool_calls;
        if state == "build" && !calls.is_empty() && calls.iter().all(is_tick) {
            count += 1;
        }
    }
    count
}

/// Whether a tool call only marked plan steps done.
fn is_tick(call: &Call) -> bool {
    if call.name == COMPLETE_STEP {
        return true;
    }
    if call.name != EDIT_FILE {
        return false;
    }
    let Ok(args) = serde_json::from_str::<EditArgs>(&call.arguments_json) else {
        return false;
    };
    let path = args.path.replace('\\', "/");
    let is_plan = path.contains(".anvil/sessions/") && path.ends_with(&format!("/{PLAN_FILE}"));
    is_plan && args.old != args.new && without_boxes(&args.old) == without_boxes(&args.new)
}

/// The text with every checkbox emptied, so two texts that differ only in
/// ticks compare equal.
fn without_boxes(text: &str) -> String {
    text.replace("[x]", "[ ]").replace("[X]", "[ ]")
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAN: &str = ".anvil/sessions/01ARZ3NDEKTSV4RRFFQ69G5FAV/plan.md";

    fn state(name: &str) -> String {
        format!("{{\"type\":\"state\",\"state\":\"{name}\",\"visit\":1}}")
    }

    fn call(name: &str, args: serde_json::Value) -> serde_json::Value {
        serde_json::json!({"id": "c", "name": name, "arguments_json": args.to_string()})
    }

    fn turn(calls: Vec<serde_json::Value>) -> String {
        serde_json::json!({
            "type": "message",
            "message": {"role": "assistant", "text": "", "reasoning": [], "tool_calls": calls}
        })
        .to_string()
    }

    fn edit(path: &str, old: &str, new: &str) -> serde_json::Value {
        call(
            "edit_file",
            serde_json::json!({"path": path, "old": old, "new": new}),
        )
    }

    fn count(lines: &[String]) -> u32 {
        lone_tick_turns(&lines.join("\n"))
    }

    #[test]
    fn a_build_turn_that_only_ticks_is_a_lone_tick() {
        let tick = edit(PLAN, "4. [ ] Check", "4. [x] Check");
        assert_eq!(count(&[state("build"), turn(vec![tick.clone()])]), 1);
        let two = edit(PLAN, "1. [ ] a\n2. [ ] b", "1. [x] a\n2. [x] b");
        assert_eq!(count(&[state("build"), turn(vec![tick, two])]), 1);
        let absolute = edit(&format!("/tmp/work/{PLAN}"), "- [ ] a", "- [X] a");
        assert_eq!(count(&[state("build"), turn(vec![absolute])]), 1);
    }

    #[test]
    fn complete_step_alone_is_a_lone_tick() {
        let step = call("complete_step", serde_json::json!({"step": 2}));
        assert_eq!(count(&[state("build"), turn(vec![step.clone()])]), 1);
        let work = edit("src/lib.rs", "a", "b");
        assert_eq!(count(&[state("build"), turn(vec![step, work])]), 0);
    }

    #[test]
    fn ticks_beside_work_and_other_plan_edits_do_not_count() {
        let tick = edit(PLAN, "1. [ ] a", "1. [x] a");
        let work = call("shell", serde_json::json!({"command": "cargo test"}));
        assert_eq!(count(&[state("build"), turn(vec![tick.clone(), work])]), 0);
        let reworded = edit(PLAN, "1. [ ] a", "1. [x] b");
        assert_eq!(count(&[state("build"), turn(vec![reworded])]), 0);
        let elsewhere = edit("docs/plan.md", "1. [ ] a", "1. [x] a");
        assert_eq!(count(&[state("build"), turn(vec![elsewhere])]), 0);
        let unchanged = edit(PLAN, "1. [x] a", "1. [x] a");
        assert_eq!(count(&[state("build"), turn(vec![unchanged])]), 0);
        assert_eq!(count(&[state("build"), turn(Vec::new())]), 0);
        assert_eq!(count(&[state("verify"), turn(vec![tick])]), 0);
    }

    #[test]
    fn counts_across_build_visits_and_skips_bad_lines() {
        let tick = edit(PLAN, "1. [ ] a", "1. [x] a");
        let lines = [
            "not json".to_owned(),
            state("plan"),
            turn(vec![tick.clone()]),
            state("build"),
            turn(vec![tick.clone()]),
            state("review"),
            state("build"),
            turn(vec![tick]),
        ];
        assert_eq!(count(&lines), 2);
    }

    #[test]
    fn a_missing_transcript_counts_nothing() {
        let dir = tempfile::tempdir().expect("tmp");
        assert_eq!(read_lone_tick_turns(dir.path()).expect("reads"), 0);
        let tick = edit(PLAN, "1. [ ] a", "1. [x] a");
        let text = [state("build"), turn(vec![tick])].join("\n");
        std::fs::write(dir.path().join(SESSION_FILE), text).expect("write");
        assert_eq!(read_lone_tick_turns(dir.path()).expect("reads"), 1);
    }
}
