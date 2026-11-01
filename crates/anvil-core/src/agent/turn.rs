//! One model completion, collected from the event stream into a message.

use std::time::Instant;

use futures_util::StreamExt;

use super::events::{EventSink, LoopEvent};
use crate::provider::{
    CompletionRequest, Event, Message, Provider, ProviderError, ReasoningBlock, StopReason,
    ToolCall, Usage,
};

/// What one completion produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    /// Visible text.
    pub text: String,
    /// Reasoning blocks, with whatever must be replayed.
    pub reasoning: Vec<ReasoningBlock>,
    /// Tool calls, in order.
    pub tool_calls: Vec<ToolCall>,
    /// Tokens used, when reported.
    pub usage: Option<Usage>,
    /// Why the model stopped.
    pub stop_reason: StopReason,
    /// Wall-clock milliseconds from the request to the `Done` event.
    pub latency_ms: u64,
    /// Retries before the request got through.
    pub retries: u32,
}

impl Turn {
    /// The assistant message to append to the conversation.
    ///
    /// # Returns
    ///
    /// The message.
    pub fn message(&self) -> Message {
        Message::Assistant {
            text: self.text.clone(),
            reasoning: self.reasoning.clone(),
            tool_calls: self.tool_calls.clone(),
        }
    }
}

/// Runs one completion, forwarding deltas to the sink as they arrive.
///
/// # Arguments
///
/// * `provider` - Who to ask.
/// * `request` - What to ask.
/// * `events` - Where text and reasoning deltas go.
///
/// # Returns
///
/// The collected turn.
///
/// # Errors
///
/// The provider's error, or [`ProviderError::Malformed`] when the stream
/// ends without a `Done` event.
pub async fn complete_turn(
    provider: &dyn Provider,
    request: CompletionRequest,
    events: &EventSink,
) -> Result<Turn, ProviderError> {
    let started = Instant::now();
    let mut stream = provider.complete(request);
    let mut turn = Turn {
        text: String::new(),
        reasoning: Vec::new(),
        tool_calls: Vec::new(),
        usage: None,
        stop_reason: StopReason::EndTurn,
        latency_ms: 0,
        retries: 0,
    };
    let mut reasoning_text = String::new();
    while let Some(item) = stream.next().await {
        match item? {
            Event::TextDelta(delta) => {
                turn.text.push_str(&delta);
                events.emit(LoopEvent::TextDelta(delta));
            }
            Event::ReasoningDelta(delta) => {
                reasoning_text.push_str(&delta);
                events.emit(LoopEvent::ReasoningDelta(delta));
            }
            Event::ReasoningEnd {
                signature,
                redacted_data,
            } => turn.reasoning.push(ReasoningBlock {
                text: std::mem::take(&mut reasoning_text),
                signature,
                redacted_data,
            }),
            Event::ToolCall(call) => turn.tool_calls.push(call),
            Event::Usage(usage) => turn.usage = Some(usage),
            Event::Retried { retries } => turn.retries = retries,
            Event::Done { stop_reason } => {
                if !reasoning_text.is_empty() {
                    turn.reasoning.push(ReasoningBlock {
                        text: std::mem::take(&mut reasoning_text),
                        ..ReasoningBlock::default()
                    });
                }
                turn.stop_reason = stop_reason;
                turn.latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                return Ok(turn);
            }
        }
    }
    Err(ProviderError::malformed(
        "stream ended without a Done event",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::FakeProvider;

    #[tokio::test]
    async fn collects_text_reasoning_calls_and_usage() {
        let call = ToolCall {
            id: "c".to_owned(),
            name: "glob".to_owned(),
            arguments_json: "{}".to_owned(),
        };
        let fake = FakeProvider::new(vec![vec![
            Ok(Event::Retried { retries: 2 }),
            Ok(Event::ReasoningDelta("th".to_owned())),
            Ok(Event::ReasoningDelta("ink".to_owned())),
            Ok(Event::ReasoningEnd {
                signature: Some("sig".to_owned()),
                redacted_data: None,
            }),
            Ok(Event::TextDelta("hel".to_owned())),
            Ok(Event::TextDelta("lo".to_owned())),
            Ok(Event::ToolCall(call.clone())),
            Ok(Event::Usage(Usage {
                input: 1,
                output: 2,
                cached: 0,
            })),
            Ok(Event::Done {
                stop_reason: StopReason::ToolUse,
            }),
        ]]);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let turn = complete_turn(&fake, CompletionRequest::default(), &EventSink::new(tx))
            .await
            .expect("turn");
        assert_eq!(turn.text, "hello");
        assert_eq!(turn.reasoning[0].text, "think");
        assert_eq!(turn.reasoning[0].signature.as_deref(), Some("sig"));
        assert_eq!(turn.tool_calls, vec![call]);
        assert_eq!(turn.usage.map(|u| u.total()), Some(3));
        assert_eq!(turn.stop_reason, StopReason::ToolUse);
        assert_eq!(turn.retries, 2);
        assert!(matches!(turn.message(), Message::Assistant { .. }));
        assert_eq!(
            rx.try_recv(),
            Ok(LoopEvent::ReasoningDelta("th".to_owned()))
        );
    }

    #[tokio::test]
    async fn unterminated_reasoning_and_missing_done() {
        let fake = FakeProvider::new(vec![
            vec![
                Ok(Event::ReasoningDelta("open".to_owned())),
                Ok(Event::Done {
                    stop_reason: StopReason::EndTurn,
                }),
            ],
            vec![Ok(Event::TextDelta("x".to_owned()))],
            vec![Err(ProviderError::Timeout { after_s: 2 })],
        ]);
        let sink = EventSink::discard();
        let turn = complete_turn(&fake, CompletionRequest::default(), &sink)
            .await
            .expect("turn");
        assert_eq!(turn.reasoning[0].text, "open");
        assert!(matches!(
            complete_turn(&fake, CompletionRequest::default(), &sink).await,
            Err(ProviderError::Malformed { .. })
        ));
        assert_eq!(
            complete_turn(&fake, CompletionRequest::default(), &sink).await,
            Err(ProviderError::Timeout { after_s: 2 })
        );
    }
}
