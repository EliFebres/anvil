//! A scripted provider for tests. No network, no randomness.

use std::collections::VecDeque;
use std::sync::Mutex;

use futures_util::stream;

use super::error::ProviderError;
use super::types::{CompletionRequest, Event, StopReason, ToolCall, Usage};
use super::{EventStream, Provider, lock_or_recover};

/// The items one `complete` call replays, in order.
pub type Script = Vec<Result<Event, ProviderError>>;

/// A provider that replays scripts and records what it was asked.
#[derive(Debug)]
pub struct FakeProvider {
    /// Scripts not yet replayed, oldest first.
    scripts: Mutex<VecDeque<Script>>,
    /// Every request received, oldest first.
    requests: Mutex<Vec<CompletionRequest>>,
    /// How many scripts were provided, for the over-call message.
    total: usize,
}

impl FakeProvider {
    /// Creates a provider that replays `scripts` one per call.
    ///
    /// # Arguments
    ///
    /// * `scripts` - One script per expected call, in call order.
    ///
    /// # Returns
    ///
    /// The provider.
    pub fn new(scripts: Vec<Script>) -> Self {
        Self {
            total: scripts.len(),
            scripts: Mutex::new(scripts.into()),
            requests: Mutex::new(Vec::new()),
        }
    }

    /// Every request received so far.
    ///
    /// # Returns
    ///
    /// Clones of the requests, oldest first.
    pub fn requests(&self) -> Vec<CompletionRequest> {
        lock_or_recover(&self.requests).clone()
    }

    /// How many times `complete` was called.
    ///
    /// # Returns
    ///
    /// The count.
    pub fn calls(&self) -> usize {
        lock_or_recover(&self.requests).len()
    }

    /// How many scripts are still waiting to be replayed.
    ///
    /// # Returns
    ///
    /// The count.
    pub fn remaining_scripts(&self) -> usize {
        lock_or_recover(&self.scripts).len()
    }

    /// A script for a plain text answer.
    ///
    /// # Arguments
    ///
    /// * `text` - The whole answer, delivered as one delta.
    ///
    /// # Returns
    ///
    /// `[TextDelta, Usage, Done { EndTurn }]`.
    pub fn text_turn(text: &str) -> Script {
        vec![
            Ok(Event::TextDelta(text.to_owned())),
            Ok(Event::Usage(Usage {
                input: 10,
                output: 5,
                cached: 0,
            })),
            Ok(Event::Done {
                stop_reason: StopReason::EndTurn,
            }),
        ]
    }

    /// A script for a turn that only calls tools.
    ///
    /// # Arguments
    ///
    /// * `calls` - The calls, in order.
    ///
    /// # Returns
    ///
    /// `[ToolCall.., Usage, Done { ToolUse }]`.
    pub fn tool_turn(calls: Vec<ToolCall>) -> Script {
        let mut script: Script = calls
            .into_iter()
            .map(|call| Ok(Event::ToolCall(call)))
            .collect();
        script.push(Ok(Event::Usage(Usage {
            input: 10,
            output: 5,
            cached: 0,
        })));
        script.push(Ok(Event::Done {
            stop_reason: StopReason::ToolUse,
        }));
        script
    }
}

impl Provider for FakeProvider {
    /// Records the request and replays the next script.
    ///
    /// # Panics
    ///
    /// When called more times than scripts were provided.
    fn complete(&self, request: CompletionRequest) -> EventStream {
        lock_or_recover(&self.requests).push(request);
        let calls = self.calls();
        let Some(script) = lock_or_recover(&self.scripts).pop_front() else {
            panic!(
                "FakeProvider called {calls} times but only {} scripts were provided",
                self.total
            );
        };
        Box::pin(stream::iter(script))
    }
}

#[cfg(test)]
mod tests {
    use futures_util::StreamExt;

    use super::*;
    use crate::provider::types::Message;

    #[tokio::test]
    async fn fake_replays_scripts_in_order_and_records_requests() {
        let fake = FakeProvider::new(vec![
            FakeProvider::text_turn("one"),
            FakeProvider::tool_turn(vec![ToolCall {
                id: "c1".to_owned(),
                name: "glob".to_owned(),
                arguments_json: "{}".to_owned(),
            }]),
        ]);
        let first = CompletionRequest {
            messages: vec![Message::user("hi")],
            ..CompletionRequest::default()
        };
        let items: Vec<_> = fake.complete(first.clone()).collect().await;
        assert_eq!(items[0], Ok(Event::TextDelta("one".to_owned())));
        assert_eq!(fake.remaining_scripts(), 1);

        let items: Vec<_> = fake.complete(CompletionRequest::default()).collect().await;
        assert!(matches!(items[0], Ok(Event::ToolCall(_))));
        assert_eq!(
            items.last(),
            Some(&Ok(Event::Done {
                stop_reason: StopReason::ToolUse
            }))
        );
        assert_eq!(fake.calls(), 2);
        assert_eq!(fake.requests()[0], first);
    }

    #[tokio::test]
    async fn fake_script_can_yield_error() {
        let fake = FakeProvider::new(vec![vec![Err(ProviderError::Timeout { after_s: 1 })]]);
        let items: Vec<_> = fake.complete(CompletionRequest::default()).collect().await;
        assert_eq!(items, vec![Err(ProviderError::Timeout { after_s: 1 })]);
    }

    #[test]
    #[should_panic(expected = "FakeProvider called 1 times but only 0 scripts")]
    fn fake_panics_when_over_called() {
        let fake = FakeProvider::new(vec![]);
        let _ = fake.complete(CompletionRequest::default());
    }
}
