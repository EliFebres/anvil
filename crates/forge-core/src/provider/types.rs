//! Data that crosses the provider boundary: requests in, events out.
//!
//! Request types derive `Serialize` and `Deserialize` so a session transcript
//! can be persisted as JSONL without a second set of types.

use serde::{Deserialize, Serialize};

/// One completion request, independent of wire protocol.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CompletionRequest {
    /// The system prompt. Empty means none.
    pub system: String,
    /// The conversation so far, oldest first.
    pub messages: Vec<Message>,
    /// Tools the model may call. Empty means none.
    pub tools: Vec<ToolSpec>,
    /// Cap on output tokens for this request; `None` uses the provider's own cap.
    pub max_output_tokens: Option<u32>,
}

/// One turn of the conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Message {
    /// Text from the user.
    User {
        /// The message text.
        text: String,
    },
    /// A turn produced by the model.
    Assistant {
        /// Visible text, possibly empty when the turn is only tool calls.
        text: String,
        /// Reasoning blocks in the order they were produced.
        reasoning: Vec<ReasoningBlock>,
        /// Tool calls requested by the model, in order.
        tool_calls: Vec<ToolCall>,
    },
    /// Results for every tool call of the preceding assistant turn.
    ToolResults {
        /// One result per call, in call order.
        results: Vec<ToolResult>,
    },
}

impl Message {
    /// Builds a user message.
    ///
    /// # Arguments
    ///
    /// * `text` - The message text.
    ///
    /// # Returns
    ///
    /// A [`Message::User`].
    pub fn user(text: impl Into<String>) -> Self {
        Self::User { text: text.into() }
    }
}

/// A reasoning block emitted by the model.
///
/// Anthropic signs its thinking blocks and requires them to be replayed
/// unchanged on later requests, so the signature and any redacted payload are
/// kept alongside the text.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ReasoningBlock {
    /// The reasoning text; empty for a redacted block.
    pub text: String,
    /// Opaque signature from the provider, if any.
    pub signature: Option<String>,
    /// Opaque payload of a redacted block, if any.
    pub redacted_data: Option<String>,
}

/// A tool call requested by the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Provider-assigned id, echoed back in the matching [`ToolResult`].
    pub id: String,
    /// Name of the tool to run.
    pub name: String,
    /// Arguments as a JSON object, serialized.
    pub arguments_json: String,
}

/// The outcome of running one tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
    /// The [`ToolCall::id`] this answers.
    pub call_id: String,
    /// Text returned to the model.
    pub content: String,
    /// Whether the tool failed.
    pub is_error: bool,
}

/// A tool definition offered to the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSpec {
    /// Tool name, unique within a request.
    pub name: String,
    /// What the tool does, for the model.
    pub description: String,
    /// JSON Schema of the arguments object.
    pub parameters: serde_json::Value,
}

/// Why the model stopped generating.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The model finished its turn.
    EndTurn,
    /// The model wants tool results before continuing.
    ToolUse,
    /// Output hit the token cap.
    MaxTokens,
    /// Any other provider-specific reason, verbatim.
    Other(String),
}

/// Token counts for one completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Usage {
    /// Prompt tokens that were not served from cache.
    pub input: u64,
    /// Generated tokens.
    pub output: u64,
    /// Prompt tokens served from cache.
    pub cached: u64,
}

impl Usage {
    /// Sums every token this completion consumed.
    ///
    /// # Returns
    ///
    /// `input + output + cached`, saturating.
    pub fn total(&self) -> u64 {
        self.input
            .saturating_add(self.output)
            .saturating_add(self.cached)
    }
}

/// One item of a completion stream.
///
/// A stream yields zero or more non-terminal events and then exactly one
/// [`Event::Done`] or one error. [`Event::Usage`] appears at most once and
/// always before `Done`. [`Event::ToolCall`] is always complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A fragment of visible text.
    TextDelta(String),
    /// A fragment of reasoning text.
    ReasoningDelta(String),
    /// The end of a reasoning block, carrying what must be replayed later.
    ReasoningEnd {
        /// Provider signature over the block, if any.
        signature: Option<String>,
        /// Opaque payload of a redacted block, if any.
        redacted_data: Option<String>,
    },
    /// A complete tool call.
    ToolCall(ToolCall),
    /// Token usage for the whole completion.
    Usage(Usage),
    /// The completion finished.
    Done {
        /// Why the model stopped.
        stop_reason: StopReason,
    },
}

impl Event {
    /// Whether this event ends the stream.
    ///
    /// # Returns
    ///
    /// `true` for [`Event::Done`].
    pub fn is_terminal(&self) -> bool {
        matches!(self, Event::Done { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_total_saturates() {
        let usage = Usage {
            input: u64::MAX,
            output: 1,
            cached: 1,
        };
        assert_eq!(usage.total(), u64::MAX);
        assert_eq!(
            Usage {
                input: 1,
                output: 2,
                cached: 3
            }
            .total(),
            6
        );
    }

    #[test]
    fn message_round_trips_through_json() {
        let message = Message::Assistant {
            text: "hi".to_owned(),
            reasoning: vec![ReasoningBlock {
                text: "why".to_owned(),
                signature: Some("sig".to_owned()),
                redacted_data: None,
            }],
            tool_calls: vec![ToolCall {
                id: "c1".to_owned(),
                name: "read_file".to_owned(),
                arguments_json: r#"{"path":"a"}"#.to_owned(),
            }],
        };
        let json = serde_json::to_string(&message).expect("serializes");
        assert!(json.contains(r#""role":"assistant""#));
        let back: Message = serde_json::from_str(&json).expect("parses");
        assert_eq!(back, message);
    }

    #[test]
    fn only_done_is_terminal() {
        assert!(
            Event::Done {
                stop_reason: StopReason::EndTurn
            }
            .is_terminal()
        );
        assert!(!Event::TextDelta(String::new()).is_terminal());
        assert!(!Event::Usage(Usage::default()).is_terminal());
    }
}
