//! Renders loop events as plain streaming text.
//!
//! Model text goes to stdout as it arrives. Everything else (state changes,
//! tool calls with a spinner, checks, warnings) goes to stderr so that
//! stdout stays usable as the model's answer. The sinks are injectable so
//! a test can pin the exact bytes.

use std::io::Write;
use std::time::Duration;

use anvil_core::agent::{LoopEvent, ReviewVerdict, State};
use anvil_core::provider::ToolCall;
use anvil_core::tools::ToolOutcome;
use tokio::sync::mpsc;

/// Spinner frames, advanced while a tool call runs.
const FRAMES: [&str; 4] = ["|", "/", "-", "\\"];

/// How often the spinner advances.
const TICK: Duration = Duration::from_millis(120);

/// Prints events to stdout and stderr until the channel closes.
///
/// # Arguments
///
/// * `events` - The loop's event channel.
/// * `show_reasoning` - Whether to print reasoning deltas too.
pub async fn render(events: mpsc::UnboundedReceiver<LoopEvent>, show_reasoning: bool) {
    render_to(events, show_reasoning, std::io::stdout(), std::io::stderr()).await;
}

/// Prints events to the given sinks until the channel closes.
///
/// # Arguments
///
/// * `events` - The loop's event channel.
/// * `show_reasoning` - Whether to print reasoning deltas too.
/// * `out` - Where model text goes.
/// * `err` - Where everything else goes.
pub async fn render_to<O: Write, E: Write>(
    mut events: mpsc::UnboundedReceiver<LoopEvent>,
    show_reasoning: bool,
    out: O,
    err: E,
) {
    let mut renderer = Renderer::new(show_reasoning, out, err);
    loop {
        let next = if renderer.spinning.is_some() {
            match tokio::time::timeout(TICK, events.recv()).await {
                Ok(event) => event,
                Err(_tick) => {
                    renderer.tick();
                    continue;
                }
            }
        } else {
            events.recv().await
        };
        match next {
            Some(event) => renderer.handle(event),
            None => break,
        }
    }
    renderer.end_line();
}

/// Tracks what is on the current line.
struct Renderer<O, E> {
    show_reasoning: bool,
    /// Whether stdout has model text on an unfinished line.
    mid_text: bool,
    /// The tool call being spun for, with its frame index.
    spinning: Option<(ToolCall, usize)>,
    out: O,
    err: E,
}

impl<O: Write, E: Write> Renderer<O, E> {
    fn new(show_reasoning: bool, out: O, err: E) -> Self {
        Self {
            show_reasoning,
            mid_text: false,
            spinning: None,
            out,
            err,
        }
    }

    /// Dispatches one event.
    fn handle(&mut self, event: LoopEvent) {
        match event {
            LoopEvent::TextDelta(text) => {
                self.clear_spinner();
                let _ = self.out.write_all(text.as_bytes());
                let _ = self.out.flush();
                self.mid_text = !text.ends_with('\n');
            }
            LoopEvent::ReasoningDelta(text) => {
                if self.show_reasoning {
                    self.clear_spinner();
                    let _ = self.err.write_all(text.as_bytes());
                    let _ = self.err.flush();
                    self.mid_text = !text.ends_with('\n');
                }
            }
            LoopEvent::StateEntered { state, visit } => {
                self.end_line();
                let again = if visit > 1 {
                    format!(" (round {visit})")
                } else {
                    String::new()
                };
                self.err_line(&format!("== {}{again}", state_title(state)));
            }
            LoopEvent::Transition { .. }
            | LoopEvent::TurnFinished { .. }
            | LoopEvent::ProposedEdit { .. } => {}
            LoopEvent::ToolStarted { call } => {
                self.end_line();
                self.spinning = Some((call, 0));
                self.draw_spinner();
            }
            LoopEvent::ToolFinished {
                name,
                outcome,
                preview,
                ..
            } => {
                let call = self.spinning.take().map(|(call, _)| call);
                let args = call.as_ref().map(call_summary).unwrap_or_default();
                let mark = match outcome {
                    ToolOutcome::Completed => "ok",
                    ToolOutcome::Failed => "failed",
                    ToolOutcome::DeniedByRule => "denied by rule",
                    ToolOutcome::DeclinedByUser => "declined",
                    ToolOutcome::Blocked => "blocked",
                    ToolOutcome::UnknownTool => "unknown tool",
                    ToolOutcome::InvalidArguments => "bad arguments",
                };
                self.err_line(&format!("\r  {name} {args} .. {mark}: {preview}"));
            }
            LoopEvent::PlanWritten { path } => {
                self.end_line();
                self.err_line(&format!("plan written to {}", path.display()));
            }
            LoopEvent::ReviewVerdict { verdict } => {
                self.end_line();
                match verdict {
                    ReviewVerdict::Approve => self.err_line("review: approved"),
                    ReviewVerdict::Changes { items } => {
                        self.err_line("review: changes requested");
                        for item in items {
                            self.err_line(&format!("  - {item}"));
                        }
                    }
                }
            }
            LoopEvent::CheckStarted { command } => {
                self.end_line();
                self.err_line(&format!("running `{command}`"));
            }
            LoopEvent::CheckFinished {
                command,
                passed,
                output_tail,
            } => {
                if passed {
                    self.err_line(&format!("`{command}` passed"));
                } else {
                    self.err_line(&format!("`{command}` failed:\n{output_tail}"));
                }
            }
            LoopEvent::Compacted {
                dropped_messages,
                summary_path,
                ..
            } => {
                self.end_line();
                self.err_line(&format!(
                    "compacted {dropped_messages} messages; summary at {}",
                    summary_path.display()
                ));
            }
            LoopEvent::Warning(message) => {
                self.end_line();
                self.err_line(&format!("warning: {message}"));
            }
            LoopEvent::Finished {
                status,
                summary_path,
            } => {
                self.end_line();
                self.err_line(&format!(
                    "\n{status}\nsummary at {}",
                    summary_path.display()
                ));
            }
        }
    }

    /// Advances the spinner.
    fn tick(&mut self) {
        if let Some((_, frame)) = &mut self.spinning {
            *frame = (*frame + 1) % FRAMES.len();
        }
        self.draw_spinner();
    }

    /// Redraws the spinner line.
    fn draw_spinner(&mut self) {
        if let Some((call, frame)) = &self.spinning {
            let _ = write!(
                self.err,
                "\r{} {} {}",
                FRAMES[*frame],
                call.name,
                call_summary(call)
            );
            let _ = self.err.flush();
        }
    }

    /// Clears a spinner line so text can be printed cleanly.
    fn clear_spinner(&mut self) {
        if self.spinning.is_some() {
            let _ = write!(self.err, "\r");
            let _ = self.err.flush();
        }
    }

    /// Finishes an unfinished text line.
    fn end_line(&mut self) {
        if self.mid_text {
            let _ = writeln!(self.out);
            let _ = self.out.flush();
            self.mid_text = false;
        }
    }

    /// Writes one line to the error sink.
    fn err_line(&mut self, line: &str) {
        let _ = writeln!(self.err, "{line}");
        let _ = self.err.flush();
    }
}

/// A one-line rendering of a call's main argument.
///
/// # Arguments
///
/// * `call` - The call.
///
/// # Returns
///
/// The first line of `command`, `path` or `pattern`, or the raw arguments,
/// cut to 80 characters.
pub(crate) fn call_summary(call: &ToolCall) -> String {
    let args: serde_json::Value = serde_json::from_str(&call.arguments_json).unwrap_or_default();
    let main = ["command", "path", "pattern"]
        .iter()
        .find_map(|key| args.get(key).and_then(|v| v.as_str()))
        .map(str::to_owned)
        .unwrap_or_else(|| call.arguments_json.clone());
    let first_line = main.lines().next().unwrap_or("");
    if first_line.chars().count() > 80 {
        format!("{}...", first_line.chars().take(80).collect::<String>())
    } else {
        first_line.to_owned()
    }
}

/// The state name as shown in the prompt and headings.
pub fn state_title(state: State) -> String {
    state.name().to_uppercase()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use anvil_core::agent::Status;
    use anvil_core::provider::{StopReason, Usage};
    use anvil_core::tools::OutputStats;

    use super::*;

    #[test]
    fn call_summary_prefers_the_main_argument() {
        let call = ToolCall {
            id: "x".to_owned(),
            name: "shell".to_owned(),
            arguments_json: "{\"command\": \"cargo test\\n--all\", \"timeout_s\": 5}".to_owned(),
        };
        assert_eq!(call_summary(&call), "cargo test");
        let long = ToolCall {
            id: "x".to_owned(),
            name: "read_file".to_owned(),
            arguments_json: format!("{{\"path\": \"{}\"}}", "a".repeat(100)),
        };
        assert!(call_summary(&long).ends_with("..."));
        assert_eq!(state_title(State::Build), "BUILD");
    }

    /// A session with every event kind, as the loop would emit them.
    fn scripted_session() -> Vec<LoopEvent> {
        let call = ToolCall {
            id: "c1".to_owned(),
            name: "shell".to_owned(),
            arguments_json: "{\"command\": \"cargo test\"}".to_owned(),
        };
        vec![
            LoopEvent::StateEntered {
                state: State::Plan,
                visit: 1,
            },
            LoopEvent::TextDelta("# Plan\n\n1. [ ] add the".to_owned()),
            LoopEvent::TextDelta(" flag\n2. [ ] wire it\n".to_owned()),
            LoopEvent::TurnFinished {
                state: State::Plan,
                stop_reason: StopReason::EndTurn,
                usage: Some(Usage {
                    input: 10,
                    output: 5,
                    cached: 0,
                }),
                latency_ms: 1,
                retries: 0,
            },
            LoopEvent::PlanWritten {
                path: PathBuf::from("plan.md"),
            },
            LoopEvent::Transition {
                from: State::Plan,
                to: State::Build,
            },
            LoopEvent::StateEntered {
                state: State::Build,
                visit: 1,
            },
            LoopEvent::ToolStarted { call },
            LoopEvent::ProposedEdit {
                path: PathBuf::from("notes.txt"),
                old: String::new(),
                new: "hello\n".to_owned(),
            },
            LoopEvent::ToolFinished {
                call_id: "c1".to_owned(),
                name: "shell".to_owned(),
                outcome: ToolOutcome::Completed,
                preview: "test result: ok".to_owned(),
                stats: OutputStats {
                    bytes: 15,
                    truncated: false,
                    digest: 0,
                },
            },
            LoopEvent::ReasoningDelta("hidden".to_owned()),
            LoopEvent::TextDelta("All green.".to_owned()),
            LoopEvent::CheckStarted {
                command: "cargo test".to_owned(),
            },
            LoopEvent::CheckFinished {
                command: "cargo test".to_owned(),
                passed: false,
                output_tail: "boom".to_owned(),
            },
            LoopEvent::ReviewVerdict {
                verdict: ReviewVerdict::Changes {
                    items: vec!["fix it".to_owned()],
                },
            },
            LoopEvent::StateEntered {
                state: State::Build,
                visit: 2,
            },
            LoopEvent::Compacted {
                dropped_messages: 3,
                summary_path: PathBuf::from("summary.md"),
                usage: None,
            },
            LoopEvent::Warning("careful".to_owned()),
            LoopEvent::Transition {
                from: State::Build,
                to: State::Done,
            },
            LoopEvent::Finished {
                status: Status::Done,
                summary_path: PathBuf::from("summary.md"),
            },
        ]
    }

    #[tokio::test]
    async fn plain_renderer_bytes_are_stable() {
        let (tx, rx) = mpsc::unbounded_channel();
        for event in scripted_session() {
            tx.send(event).expect("open channel");
        }
        drop(tx);
        let mut out = Vec::new();
        let mut err = Vec::new();
        render_to(rx, false, &mut out, &mut err).await;
        let out = String::from_utf8(out).expect("utf-8 stdout");
        let err = String::from_utf8(err)
            .expect("utf-8 stderr")
            .replace('\r', "<CR>");
        insta::assert_snapshot!(format!("--- stdout\n{out}\n--- stderr\n{err}"));
    }
}
