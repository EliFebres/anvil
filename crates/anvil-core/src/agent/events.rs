//! Events the loop emits. The CLI renders them; the usage records hold them.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc;

use super::checks::CheckSource;
use super::role::{EscalationReason, Role};
use super::state::{State, Status};
use super::verdict::ReviewVerdict;
use crate::provider::{StopReason, ToolCall, Usage};
use crate::tools::{Execution, OutputStats, ToolOutcome};

/// Something the loop did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopEvent {
    /// The loop moved from one state to another.
    Transition {
        /// Where it was.
        from: State,
        /// Where it goes.
        to: State,
    },
    /// A state started running.
    StateEntered {
        /// The state.
        state: State,
        /// How many times this state has been entered, starting at 1.
        visit: u32,
    },
    /// A fragment of model text.
    TextDelta(String),
    /// A fragment of model reasoning.
    ReasoningDelta(String),
    /// One model completion ended.
    TurnFinished {
        /// The state it ran in.
        state: State,
        /// The job it was made for, which says whose provider served it.
        role: Role,
        /// Why the model stopped.
        stop_reason: StopReason,
        /// Tokens it used, when the provider reported them.
        usage: Option<Usage>,
        /// Wall-clock milliseconds the completion took.
        latency_ms: u64,
        /// Retries before the request got through.
        retries: u32,
    },
    /// A tool call is about to run.
    ToolStarted {
        /// The call.
        call: ToolCall,
    },
    /// A tool call finished.
    ToolFinished {
        /// The call id.
        call_id: String,
        /// The tool name.
        name: String,
        /// How it ended.
        outcome: ToolOutcome,
        /// The first line of the result.
        preview: String,
        /// Size and digest of the full result.
        stats: OutputStats,
    },
    /// A tool call wrote a file; follows the call's `ToolFinished`.
    FileChanged {
        /// The call id.
        call_id: String,
        /// The file, as the tool showed it to the model.
        path: String,
        /// SHA-256 before the call, or `None` when the file was new.
        before: Option<String>,
        /// SHA-256 after the call.
        after: String,
    },
    /// The plan was written to disk.
    PlanWritten {
        /// Where.
        path: PathBuf,
    },
    /// anvil marked plan steps done in the plan file without a model turn.
    PlanUpdated {
        /// The plan file.
        path: PathBuf,
    },
    /// The reviewer answered.
    ReviewVerdict {
        /// The verdict.
        verdict: ReviewVerdict,
    },
    /// A project check command started.
    CheckStarted {
        /// The command line.
        command: String,
    },
    /// A project check command finished.
    CheckFinished {
        /// The command line.
        command: String,
        /// Where the command came from.
        source: CheckSource,
        /// Whether it exited with zero.
        passed: bool,
        /// The last lines of its output.
        output_tail: String,
    },
    /// Older turns were replaced by a summary.
    Compacted {
        /// How many messages the summary replaced.
        dropped_messages: usize,
        /// Where the summary was written, when the transcript mode keeps
        /// conversation text.
        summary_path: Option<PathBuf>,
        /// Tokens the summarizer used, when reported.
        usage: Option<Usage>,
    },
    /// The next build round runs on the planner because the coder is stuck.
    Escalated {
        /// What got the coder stuck.
        reason: EscalationReason,
        /// The build visit that runs on the planner, counting from 1.
        round: u32,
    },
    /// Something non-fatal worth telling the user.
    Warning(String),
    /// What an `edit_file` or `write_file` call is about to do, emitted
    /// before the call runs so the change can be shown and judged.
    ProposedEdit {
        /// The file.
        path: PathBuf,
        /// The file before the call; empty when it does not exist yet.
        old: String,
        /// The file after the call.
        new: String,
    },
    /// The run ended.
    Finished {
        /// How.
        status: Status,
        /// Where the summary was written.
        summary_path: PathBuf,
    },
}

/// The events that report a finished call: its `ToolFinished`, then one
/// `FileChanged` per file it wrote. The loop, chat and delegated runs all
/// report calls through this, so every front end and record sees the same.
///
/// # Arguments
///
/// * `call` - The call.
/// * `execution` - How it ran.
///
/// # Returns
///
/// The events, in order.
pub fn finished_events(call: &ToolCall, execution: &Execution) -> Vec<LoopEvent> {
    let mut events = vec![LoopEvent::ToolFinished {
        call_id: call.id.clone(),
        name: call.name.clone(),
        outcome: execution.outcome,
        preview: super::preview(&execution.result.content),
        stats: execution.stats,
    }];
    events.extend(file_change_events(call, execution));
    events
}

/// One `FileChanged` per file a finished call wrote, for a front end that
/// reports the `ToolFinished` itself.
///
/// # Arguments
///
/// * `call` - The call.
/// * `execution` - How it ran.
///
/// # Returns
///
/// The events, in the order the tool wrote the files.
pub fn file_change_events(call: &ToolCall, execution: &Execution) -> Vec<LoopEvent> {
    execution
        .changes
        .iter()
        .map(|change| LoopEvent::FileChanged {
            call_id: call.id.clone(),
            path: change.path.clone(),
            before: change.before.clone(),
            after: change.after.clone(),
        })
        .collect()
}

/// Sees every event the moment it is emitted, on the emitting thread,
/// before it is queued for the channel. The audit log uses it so its
/// records keep the order things happened in, interleaved correctly with
/// the permission decisions made while a call runs.
pub trait EventTap: Send + Sync {
    /// Called once per emitted event.
    ///
    /// # Arguments
    ///
    /// * `event` - The event.
    fn seen(&self, event: &LoopEvent);
}

/// Where events go. Sending never blocks and never fails the loop.
#[derive(Clone)]
pub struct EventSink {
    /// The channel, if anyone is listening.
    sender: Option<mpsc::UnboundedSender<LoopEvent>>,
    /// Told of each event synchronously, if set.
    tap: Option<Arc<dyn EventTap>>,
}

impl std::fmt::Debug for EventSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventSink")
            .field("sender", &self.sender)
            .field("tap", &self.tap.is_some())
            .finish()
    }
}

impl EventSink {
    /// A sink over a channel.
    ///
    /// # Arguments
    ///
    /// * `sender` - The channel.
    ///
    /// # Returns
    ///
    /// The sink.
    pub fn new(sender: mpsc::UnboundedSender<LoopEvent>) -> Self {
        Self {
            sender: Some(sender),
            tap: None,
        }
    }

    /// A sink that drops everything.
    ///
    /// # Returns
    ///
    /// The sink.
    pub fn discard() -> Self {
        Self {
            sender: None,
            tap: None,
        }
    }

    /// The sink with a tap that sees each event before it is queued.
    ///
    /// # Arguments
    ///
    /// * `tap` - The tap.
    ///
    /// # Returns
    ///
    /// The sink.
    pub fn with_tap(mut self, tap: Arc<dyn EventTap>) -> Self {
        self.tap = Some(tap);
        self
    }

    /// Emits one event; a closed channel is ignored.
    ///
    /// # Arguments
    ///
    /// * `event` - The event.
    pub fn emit(&self, event: LoopEvent) {
        if let Some(tap) = &self.tap {
            tap.seen(&event);
        }
        if let Some(sender) = &self.sender {
            let _ = sender.send(event);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sink_delivers_and_tolerates_closed_channels() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let sink = EventSink::new(tx);
        sink.emit(LoopEvent::Warning("hi".to_owned()));
        assert_eq!(rx.try_recv(), Ok(LoopEvent::Warning("hi".to_owned())));
        drop(rx);
        sink.emit(LoopEvent::Warning("lost".to_owned()));
        EventSink::discard().emit(LoopEvent::Warning("lost".to_owned()));
    }

    #[test]
    fn a_tap_sees_each_event_even_without_a_channel() {
        struct Count(std::sync::Mutex<Vec<LoopEvent>>);
        impl EventTap for Count {
            fn seen(&self, event: &LoopEvent) {
                self.0.lock().expect("lock").push(event.clone());
            }
        }
        let count = Arc::new(Count(std::sync::Mutex::new(Vec::new())));
        let sink = EventSink::discard().with_tap(Arc::clone(&count) as Arc<dyn EventTap>);
        sink.emit(LoopEvent::Warning("a".to_owned()));
        sink.clone().emit(LoopEvent::Warning("b".to_owned()));
        assert_eq!(count.0.lock().expect("lock").len(), 2);
    }
}
