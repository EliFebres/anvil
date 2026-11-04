//! A scripted provider for tests and for `protocol = "fake"`. No network,
//! no randomness.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Mutex;

use futures_util::stream;
use serde::Deserialize;

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
    /// The script file this provider was loaded from; when set, a call past
    /// the end is a provider error instead of a panic.
    source: Option<String>,
}

/// The JSON shape of a script file: one entry per completion, in call order.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScriptFile {
    /// The completions.
    turns: Vec<ScriptTurn>,
}

/// One completion of a script file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScriptTurn {
    /// Text the model writes, if any.
    #[serde(default)]
    text: Option<String>,
    /// Tool calls the model makes, in order.
    #[serde(default)]
    tool_calls: Vec<ScriptCall>,
}

/// One tool call of a script turn.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScriptCall {
    /// The tool's name.
    name: String,
    /// The tool's arguments, as a JSON object.
    arguments: serde_json::Value,
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
            source: None,
        }
    }

    /// Loads a provider from a JSON script file:
    /// `{"turns": [{"text": "...", "tool_calls": [{"name": "...", "arguments": {...}}]}]}`.
    ///
    /// Each turn becomes its text, its tool calls (ids `call-1`, `call-2`,
    /// and so on across the whole script), a small `Usage`, and `Done` with
    /// `ToolUse` when it calls tools and `EndTurn` otherwise.
    ///
    /// # Arguments
    ///
    /// * `path` - The script file.
    ///
    /// # Returns
    ///
    /// The provider. A call past the last turn yields a
    /// [`ProviderError::Api`] naming the file, instead of panicking.
    ///
    /// # Errors
    ///
    /// [`ProviderError::Malformed`] when the file cannot be read or parsed.
    pub fn from_script_file(path: &Path) -> Result<Self, ProviderError> {
        let shown = path.display().to_string();
        let text = std::fs::read_to_string(path).map_err(|error| ProviderError::Malformed {
            detail: format!("reading the fake script `{shown}`: {error}"),
        })?;
        Self::from_script_text(&text, &shown)
    }

    /// Parses a script file's text; `source` names it in messages.
    fn from_script_text(text: &str, source: &str) -> Result<Self, ProviderError> {
        let file: ScriptFile =
            serde_json::from_str(text).map_err(|error| ProviderError::Malformed {
                detail: format!("parsing the fake script `{source}`: {error}"),
            })?;
        let mut next_id = 0;
        let scripts = file
            .turns
            .into_iter()
            .map(|turn| {
                let mut script: Script = Vec::new();
                if let Some(text) = turn.text {
                    script.push(Ok(Event::TextDelta(text)));
                }
                let calls_tools = !turn.tool_calls.is_empty();
                for call in turn.tool_calls {
                    next_id += 1;
                    script.push(Ok(Event::ToolCall(ToolCall {
                        id: format!("call-{next_id}"),
                        name: call.name,
                        arguments_json: call.arguments.to_string(),
                    })));
                }
                script.push(Ok(Event::Usage(Usage {
                    input: 10,
                    output: 5,
                    cached: 0,
                })));
                script.push(Ok(Event::Done {
                    stop_reason: if calls_tools {
                        StopReason::ToolUse
                    } else {
                        StopReason::EndTurn
                    },
                }));
                script
            })
            .collect();
        let mut provider = Self::new(scripts);
        provider.source = Some(source.to_owned());
        Ok(provider)
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
    /// When called more times than scripts were provided, unless the
    /// provider was loaded from a file.
    fn complete(&self, request: CompletionRequest) -> EventStream {
        let system_start: String = request.system.lines().next().unwrap_or("").to_owned();
        lock_or_recover(&self.requests).push(request);
        let calls = self.calls();
        let Some(script) = lock_or_recover(&self.scripts).pop_front() else {
            if let Some(source) = &self.source {
                let error = ProviderError::Api {
                    kind: "fake_script_exhausted".to_owned(),
                    message: format!(
                        "fake script `{source}` has {} turns and call {calls} found none; its system prompt starts: {system_start}",
                        self.total
                    ),
                };
                return Box::pin(stream::iter(vec![Err(error)]));
            }
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

    #[tokio::test]
    async fn a_script_file_becomes_turns_and_reports_running_out() {
        let fake = FakeProvider::from_script_text(
            r#"{"turns": [
                {"text": "looking", "tool_calls": [{"name": "read_file", "arguments": {"path": "a"}}]},
                {"text": "done"}
            ]}"#,
            "s.json",
        )
        .expect("parses");
        let first: Vec<_> = fake.complete(CompletionRequest::default()).collect().await;
        assert_eq!(first[0], Ok(Event::TextDelta("looking".to_owned())));
        assert_eq!(
            first[1],
            Ok(Event::ToolCall(ToolCall {
                id: "call-1".to_owned(),
                name: "read_file".to_owned(),
                arguments_json: r#"{"path":"a"}"#.to_owned(),
            }))
        );
        assert_eq!(
            first.last(),
            Some(&Ok(Event::Done {
                stop_reason: StopReason::ToolUse
            }))
        );
        let second: Vec<_> = fake.complete(CompletionRequest::default()).collect().await;
        assert_eq!(
            second.last(),
            Some(&Ok(Event::Done {
                stop_reason: StopReason::EndTurn
            }))
        );
        let third: Vec<_> = fake
            .complete(CompletionRequest {
                system: "You are in Verify.\nmore".to_owned(),
                ..CompletionRequest::default()
            })
            .collect()
            .await;
        let Some(Err(ProviderError::Api { message, .. })) = third.first() else {
            panic!("expected an exhausted error, got {third:?}");
        };
        assert!(message.contains("`s.json` has 2 turns"), "{message}");
        assert!(message.contains("You are in Verify."), "{message}");
    }

    #[test]
    fn a_bad_script_file_is_malformed() {
        let error = FakeProvider::from_script_text("{\"turns\": 3}", "s.json").expect_err("bad");
        assert!(
            matches!(error, ProviderError::Malformed { .. }),
            "{error:?}"
        );
    }

    #[test]
    #[should_panic(expected = "FakeProvider called 1 times but only 0 scripts")]
    fn fake_panics_when_over_called() {
        let fake = FakeProvider::new(vec![]);
        let _ = fake.complete(CompletionRequest::default());
    }
}
