//! Renders loop events as plain streaming text.
//!
//! Model text goes to stdout as it arrives. Everything else (state changes,
//! tool calls with a spinner, checks, warnings) goes to stderr so that
//! stdout stays usable as the model's answer.

use std::io::Write;
use std::time::Duration;

use forge_core::agent::{LoopEvent, ReviewVerdict, State};
use forge_core::provider::ToolCall;
use forge_core::tools::ToolOutcome;
use tokio::sync::mpsc;

/// Spinner frames, advanced while a tool call runs.
const FRAMES: [&str; 4] = ["|", "/", "-", "\\"];

/// How often the spinner advances.
const TICK: Duration = Duration::from_millis(120);

/// Prints events until the channel closes.
///
/// # Arguments
///
/// * `events` - The loop's event channel.
/// * `show_reasoning` - Whether to print reasoning deltas (dimmed) too.
pub async fn render(mut events: mpsc::UnboundedReceiver<LoopEvent>, show_reasoning: bool) {
    let mut renderer = Renderer::new(show_reasoning);
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
struct Renderer {
    show_reasoning: bool,
    /// Whether stdout has model text on an unfinished line.
    mid_text: bool,
    /// The tool call being spun for, with its frame index.
    spinning: Option<(ToolCall, usize)>,
}

impl Renderer {
    fn new(show_reasoning: bool) -> Self {
        Self {
            show_reasoning,
            mid_text: false,
            spinning: None,
        }
    }

    /// Dispatches one event.
    fn handle(&mut self, event: LoopEvent) {
        match event {
            LoopEvent::TextDelta(text) => {
                self.clear_spinner();
                let mut stdout = std::io::stdout();
                let _ = stdout.write_all(text.as_bytes());
                let _ = stdout.flush();
                self.mid_text = !text.ends_with('\n');
            }
            LoopEvent::ReasoningDelta(text) => {
                if self.show_reasoning {
                    self.clear_spinner();
                    let mut stderr = std::io::stderr();
                    let _ = stderr.write_all(text.as_bytes());
                    let _ = stderr.flush();
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
                eprintln!("== {}{again}", state_title(state));
            }
            LoopEvent::Transition { .. } | LoopEvent::TurnFinished { .. } => {}
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
                eprintln!("\r  {name} {args} .. {mark}: {preview}");
            }
            LoopEvent::PlanWritten { path } => {
                self.end_line();
                eprintln!("plan written to {}", path.display());
            }
            LoopEvent::ReviewVerdict { verdict } => {
                self.end_line();
                match verdict {
                    ReviewVerdict::Approve => eprintln!("review: approved"),
                    ReviewVerdict::Changes { items } => {
                        eprintln!("review: changes requested");
                        for item in items {
                            eprintln!("  - {item}");
                        }
                    }
                }
            }
            LoopEvent::CheckStarted { command } => {
                self.end_line();
                eprintln!("running `{command}`");
            }
            LoopEvent::CheckFinished {
                command,
                passed,
                output_tail,
            } => {
                if passed {
                    eprintln!("`{command}` passed");
                } else {
                    eprintln!("`{command}` failed:\n{output_tail}");
                }
            }
            LoopEvent::Compacted {
                dropped_messages,
                summary_path,
                ..
            } => {
                self.end_line();
                eprintln!(
                    "compacted {dropped_messages} messages; summary at {}",
                    summary_path.display()
                );
            }
            LoopEvent::Warning(message) => {
                self.end_line();
                eprintln!("warning: {message}");
            }
            LoopEvent::Finished {
                status,
                summary_path,
            } => {
                self.end_line();
                eprintln!("\n{status}\nsummary at {}", summary_path.display());
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
    fn draw_spinner(&self) {
        if let Some((call, frame)) = &self.spinning {
            let mut stderr = std::io::stderr();
            let _ = write!(
                stderr,
                "\r{} {} {}",
                FRAMES[*frame],
                call.name,
                call_summary(call)
            );
            let _ = stderr.flush();
        }
    }

    /// Clears a spinner line so text can be printed cleanly.
    fn clear_spinner(&mut self) {
        if self.spinning.is_some() {
            eprint!("\r");
        }
    }

    /// Finishes an unfinished text line.
    fn end_line(&mut self) {
        if self.mid_text {
            println!();
            self.mid_text = false;
        }
    }
}

/// A one-line rendering of a call's main argument.
fn call_summary(call: &ToolCall) -> String {
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
}
