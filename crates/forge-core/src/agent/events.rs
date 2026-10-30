//! Events the loop emits. The CLI renders them; telemetry records them.

use std::path::PathBuf;

use tokio::sync::mpsc;

use super::state::{State, Status};
use super::verdict::ReviewVerdict;
use crate::provider::{StopReason, ToolCall, Usage};
use crate::tools::ToolOutcome;

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
        /// Why the model stopped.
        stop_reason: StopReason,
        /// Tokens it used, when the provider reported them.
        usage: Option<Usage>,
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
    },
    /// The plan was written to disk.
    PlanWritten {
        /// Where.
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
        /// Whether it exited with zero.
        passed: bool,
        /// The last lines of its output.
        output_tail: String,
    },
    /// Older turns were replaced by a summary.
    Compacted {
        /// How many messages the summary replaced.
        dropped_messages: usize,
        /// Where the summary was written.
        summary_path: PathBuf,
    },
    /// Something non-fatal worth telling the user.
    Warning(String),
    /// The run ended.
    Finished {
        /// How.
        status: Status,
        /// Where the summary was written.
        summary_path: PathBuf,
    },
}

/// Where events go. Sending never blocks and never fails the loop.
#[derive(Debug, Clone)]
pub struct EventSink {
    /// The channel, if anyone is listening.
    sender: Option<mpsc::UnboundedSender<LoopEvent>>,
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
        }
    }

    /// A sink that drops everything.
    ///
    /// # Returns
    ///
    /// The sink.
    pub fn discard() -> Self {
        Self { sender: None }
    }

    /// Emits one event; a closed channel is ignored.
    ///
    /// # Arguments
    ///
    /// * `event` - The event.
    pub fn emit(&self, event: LoopEvent) {
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
}
