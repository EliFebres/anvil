//! Reads the implementer output log into the lines the dashboard shows.
//!
//! The sandbox writes one line per output line as `<unix ms>\t<o|e>\t<text>`
//! ([`crate::paths::FactoryHome::output_log`]). On stderr anvil's line
//! renderer prints a spinner `<frame> <tool> <args>` while a tool runs,
//! redrawn with a carriage return, then `  <tool> <args> .. <mark>: <preview>`
//! when it ends, and `== <STATE>` when the agent changes state. stdout is
//! the model's text.

use crate::board::{TAIL_LINES, TailLine};

/// The spinner frames anvil draws, in order.
const FRAMES: [&str; 4] = ["|", "/", "-", "\\"];

/// The words anvil ends a tool line with, longest first so `denied by
/// rule` is not read as something shorter.
const MARKS: [&str; 7] = [
    "denied by rule",
    "bad arguments",
    "unknown tool",
    "declined",
    "blocked",
    "failed",
    "ok",
];

/// What the log says so far.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Tail {
    /// The last [`TAIL_LINES`] lines, oldest first; a running tool last.
    pub lines: Vec<TailLine>,
    /// The agent's state from the last `== STATE` line, in lower case.
    pub state: Option<String>,
}

/// A tool whose spinner has been seen and whose end has not.
struct Running {
    name: String,
    args: String,
    started_ms: u64,
}

/// Parses the whole log.
///
/// # Arguments
///
/// * `log` - The log's text.
///
/// # Returns
///
/// The lines to show and the agent's state.
pub fn parse(log: &str) -> Tail {
    let mut tail = Tail::default();
    let mut running: Option<Running> = None;
    for entry in log.lines() {
        let mut fields = entry.splitn(3, '\t');
        let (Some(ms), Some(stream), Some(text)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let ms: u64 = ms.parse().unwrap_or(0);
        if stream == "o" {
            if !text.trim().is_empty() {
                tail.lines.push(TailLine::Text {
                    text: text.trim_end().to_owned(),
                });
            }
            continue;
        }
        if let Some(state) = text.strip_prefix("== ") {
            let name = state.split(" (").next().unwrap_or(state);
            tail.state = Some(name.trim().to_lowercase());
            continue;
        }
        if text.starts_with("session ") || text.starts_with("summary at ") {
            continue;
        }
        if let Some((name, args)) = spinner(text) {
            if running.as_ref().is_none_or(|tool| tool.name != name) {
                running = Some(Running {
                    name,
                    args,
                    started_ms: ms,
                });
            }
            continue;
        }
        if let Some((name, args, mark)) = finished(text) {
            let millis = running
                .take()
                .filter(|tool| tool.name == name)
                .map(|tool| ms.saturating_sub(tool.started_ms));
            tail.lines.push(TailLine::Tool {
                status: short_mark(mark).to_owned(),
                name,
                args,
                millis,
                running: false,
            });
            continue;
        }
        if !text.trim().is_empty() {
            tail.lines.push(TailLine::Text {
                text: text.trim_end().to_owned(),
            });
        }
    }
    if let Some(tool) = running {
        tail.lines.push(TailLine::Tool {
            status: "..".to_owned(),
            name: tool.name,
            args: tool.args,
            millis: None,
            running: true,
        });
    }
    let skip = tail.lines.len().saturating_sub(TAIL_LINES);
    tail.lines.drain(..skip);
    tail
}

/// Rewrites captured implementer output with one line per tool call, as the
/// TUI shows it collapsed: `<status> <tool>  <args>`.
///
/// anvil's line renderer draws a tool's spinner frames and its final line
/// after carriage returns, never a newline, so a captured run would otherwise
/// put every frame of one call on a single line. Only text after a carriage
/// return is read as a tool line, so the model's own text, such as a
/// Markdown list starting with `- `, passes through untouched.
///
/// # Arguments
///
/// * `output` - The run's stdout and stderr as captured.
///
/// # Returns
///
/// The output with each tool call on its own line; a call that never ended
/// shows as running (`..`).
pub fn collapse(output: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut running: Option<(String, String)> = None;
    for line in output.lines() {
        let mut segments = line.split('\r');
        let first = segments.next().unwrap_or_default();
        if !line.contains('\r') || !first.trim().is_empty() {
            lines.push(first.trim_end().to_owned());
        }
        for segment in segments {
            if let Some(tool) = spinner(segment) {
                running = Some(tool);
            } else if let Some((name, args, mark)) = finished(segment) {
                running = None;
                lines.push(tool_line(short_mark(mark), &name, &args));
            } else if !segment.trim().is_empty() {
                lines.push(segment.trim_end().to_owned());
            }
        }
    }
    if let Some((name, args)) = running {
        lines.push(tool_line("..", &name, &args));
    }
    lines.join("\n")
}

/// One collapsed tool call: the status padded to the TUI's width, then the
/// tool and its arguments.
fn tool_line(status: &str, name: &str, args: &str) -> String {
    format!("{status:<6} {name}  {args}").trim_end().to_owned()
}

/// `<frame> <tool> <args>`: a tool that is running.
fn spinner(text: &str) -> Option<(String, String)> {
    let (frame, rest) = text.split_once(' ')?;
    if !FRAMES.contains(&frame) {
        return None;
    }
    let (name, args) = rest.split_once(' ').unwrap_or((rest, ""));
    (!name.is_empty()).then(|| (name.to_owned(), args.to_owned()))
}

/// `  <tool> <args> .. <mark>: <preview>`: a tool that ended.
fn finished(text: &str) -> Option<(String, String, &'static str)> {
    let body = text.strip_prefix("  ")?;
    let (at, mark) = MARKS
        .iter()
        .filter_map(|mark| {
            body.find(&format!(" .. {mark}:"))
                .or_else(|| {
                    body.ends_with(&format!(" .. {mark}"))
                        .then(|| body.len() - mark.len() - 4)
                })
                .map(|at| (at, *mark))
        })
        .min_by_key(|(at, _)| *at)?;
    let head = &body[..at];
    let (name, args) = head.split_once(' ').unwrap_or((head, ""));
    (!name.is_empty()).then(|| (name.to_owned(), args.to_owned(), mark))
}

/// The status token for a mark: `denied by rule` is `denied`, and so on.
fn short_mark(mark: &str) -> &'static str {
    match mark {
        "ok" => "ok",
        "failed" => "failed",
        "denied by rule" => "denied",
        "declined" => "declined",
        "blocked" => "blocked",
        "unknown tool" => "unknown",
        _ => "bad args",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a short anvil run writes, as the sandbox logs it.
    const LOG: &str = "\
1000\te\tsession 01JABCDEF
1001\te\t== BUILD
1100\te\t| grep fn permission
1109\te\t  grep fn permission .. ok: 3 matches
1200\to\tLooking at the permission module.
1300\te\t| shell cargo test -p anvil-core
1500\te\t/ shell cargo test -p anvil-core
49500\te\t  shell cargo test -p anvil-core .. failed: exit 101
49600\te\t| edit_file crates/a.rs
49631\te\t  edit_file crates/a.rs .. denied by rule: edit_file
50000\te\t== REVIEW (round 2)
50100\te\t- read_file crates/b.rs
";

    #[test]
    fn collapse_writes_one_line_per_tool_call() {
        // As captured from a real run: stdout first, then stderr, where each
        // tool's spinner frames and its end share one newline.
        let output = "\
I'll look first.
- a Markdown item, not a spinner
session 01M6EZ1F6J18MNMKVRGYHMKS81
== BUILD
\r| grep TicketsConfig\r  grep TicketsConfig .. ok: crates/anvil-types/src/config.rs:416
\r| shell cargo test 2>&1 | tail -n 25\r  shell cargo test 2>&1 | tail -n 25 .. denied by rule: denied by a permission rule
\r| shell cargo test -p anvil-types\r/ shell cargo test -p anvil-types\r- shell cargo test -p anvil-types\r\\ shell cargo test -p anvil-types\r| shell cargo test -p anvil-types\r  shell cargo test -p anvil-types .. failed: exit 101
\r| shell cargo build\r/ shell cargo build

incomplete: the build state used its token budget
";
        assert_eq!(
            collapse(output),
            "\
I'll look first.
- a Markdown item, not a spinner
session 01M6EZ1F6J18MNMKVRGYHMKS81
== BUILD
ok     grep  TicketsConfig
denied shell  cargo test 2>&1 | tail -n 25
failed shell  cargo test -p anvil-types

incomplete: the build state used its token budget
..     shell  cargo build"
        );
    }

    #[test]
    fn collapse_leaves_output_without_carriage_returns_alone() {
        let output = "   Compiling anvil v0.1.0\n\nerror[E0308]: mismatched types\n  |\n";
        assert_eq!(
            collapse(output),
            "   Compiling anvil v0.1.0\n\nerror[E0308]: mismatched types\n  |"
        );
    }

    #[test]
    fn tools_get_their_status_and_duration_and_the_running_one_comes_last() {
        let tail = parse(LOG);
        assert_eq!(tail.state.as_deref(), Some("review"));
        assert_eq!(
            tail.lines,
            vec![
                TailLine::Tool {
                    status: "ok".to_owned(),
                    name: "grep".to_owned(),
                    args: "fn permission".to_owned(),
                    millis: Some(9),
                    running: false,
                },
                TailLine::Text {
                    text: "Looking at the permission module.".to_owned(),
                },
                TailLine::Tool {
                    status: "failed".to_owned(),
                    name: "shell".to_owned(),
                    args: "cargo test -p anvil-core".to_owned(),
                    millis: Some(48_200),
                    running: false,
                },
                TailLine::Tool {
                    status: "denied".to_owned(),
                    name: "edit_file".to_owned(),
                    args: "crates/a.rs".to_owned(),
                    millis: Some(31),
                    running: false,
                },
                TailLine::Tool {
                    status: "..".to_owned(),
                    name: "read_file".to_owned(),
                    args: "crates/b.rs".to_owned(),
                    millis: None,
                    running: true,
                },
            ]
        );
    }

    #[test]
    fn keeps_the_newest_lines_and_skips_garbage() {
        let mut log = String::from("not a log line\n");
        for n in 0..40 {
            log.push_str(&format!("{n}\to\tline {n}\n"));
        }
        let tail = parse(&log);
        assert_eq!(tail.lines.len(), TAIL_LINES);
        assert_eq!(
            tail.lines.last(),
            Some(&TailLine::Text {
                text: "line 39".to_owned()
            })
        );
    }

    #[test]
    fn a_tool_line_with_an_empty_preview_still_parses() {
        let tail = parse("1\te\t  read_file x.rs .. ok: \n");
        assert!(matches!(
            &tail.lines[0],
            TailLine::Tool { status, millis: None, .. } if status == "ok"
        ));
    }
}
