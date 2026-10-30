//! The OpenAI chat-completions protocol, as served by vLLM and friends.

use std::collections::BTreeMap;
use std::time::Duration;

use forge_types::{LimitsConfig, ProviderConfig};
use serde_json::{Value, json};

use super::error::ProviderError;
use super::http::{self, Decoder, Items, StreamConfig};
use super::retry::RetryPolicy;
use super::sse::SseFrame;
use super::types::{CompletionRequest, Event, Message, StopReason, ToolCall, ToolSpec, Usage};
use super::{EventStream, Provider};

/// A provider speaking `POST {base_url}/chat/completions` with SSE streaming.
pub struct OpenAiProvider {
    /// HTTP client shared by every request.
    client: reqwest::Client,
    /// Full endpoint URL.
    url: String,
    /// `Authorization` header value.
    authorization: String,
    /// Model name as the server expects it.
    model: String,
    /// Default output cap, if configured.
    max_output_tokens: Option<u32>,
    /// Deadline and retry policy.
    stream_cfg: StreamConfig,
}

impl OpenAiProvider {
    /// Creates the adapter.
    ///
    /// # Arguments
    ///
    /// * `name` - Config entry name, for logs.
    /// * `cfg` - The provider entry.
    /// * `api_key` - Sent as a bearer token.
    /// * `limits` - Timeout and retry settings.
    /// * `client` - The HTTP client to use.
    ///
    /// # Returns
    ///
    /// The adapter.
    pub fn new(
        name: &str,
        cfg: &ProviderConfig,
        api_key: String,
        limits: &LimitsConfig,
        client: reqwest::Client,
    ) -> Self {
        Self {
            client,
            url: format!("{}/chat/completions", cfg.base_url.trim_end_matches('/')),
            authorization: format!("Bearer {api_key}"),
            model: cfg.model.clone(),
            max_output_tokens: cfg.max_output_tokens,
            stream_cfg: StreamConfig {
                name: name.to_owned(),
                timeout: Duration::from_secs(limits.request_timeout_s),
                policy: RetryPolicy::from_limits(limits),
            },
        }
    }
}

impl Provider for OpenAiProvider {
    fn complete(&self, request: CompletionRequest) -> EventStream {
        let max_tokens = request.max_output_tokens.or(self.max_output_tokens);
        let body = build_body(&self.model, max_tokens, &request);
        let client = self.client.clone();
        let url = self.url.clone();
        let authorization = self.authorization.clone();
        let attempt = move |index: u32| {
            tracing::debug!(attempt = index, url = %url, "sending chat completion");
            let builder = client
                .post(&url)
                .header("authorization", &authorization)
                .json(&body);
            http::send_attempt(builder)
        };
        http::start(self.stream_cfg.clone(), attempt, OpenAiDecoder::default())
    }
}

/// Builds the JSON request body.
///
/// # Arguments
///
/// * `model` - Model name.
/// * `max_tokens` - Output cap, omitted when `None`.
/// * `request` - The completion request.
///
/// # Returns
///
/// The body.
pub(crate) fn build_body(
    model: &str,
    max_tokens: Option<u32>,
    request: &CompletionRequest,
) -> Value {
    let mut messages = Vec::new();
    if !request.system.is_empty() {
        messages.push(json!({"role": "system", "content": request.system}));
    }
    for message in &request.messages {
        match message {
            Message::User { text } => messages.push(json!({"role": "user", "content": text})),
            Message::Assistant {
                text, tool_calls, ..
            } => messages.push(assistant_message(text, tool_calls)),
            Message::ToolResults { results } => {
                for result in results {
                    let content = if result.is_error {
                        format!("Error: {}", result.content)
                    } else {
                        result.content.clone()
                    };
                    messages.push(json!({
                        "role": "tool",
                        "tool_call_id": result.call_id,
                        "content": content,
                    }));
                }
            }
        }
    }
    let mut body = json!({
        "model": model,
        "stream": true,
        "stream_options": {"include_usage": true},
        "messages": messages,
    });
    if let Some(max_tokens) = max_tokens {
        body["max_tokens"] = json!(max_tokens);
    }
    if !request.tools.is_empty() {
        body["tools"] = Value::Array(request.tools.iter().map(tool_json).collect());
    }
    body
}

/// Renders an assistant turn; `content` is null when the turn is only calls.
fn assistant_message(text: &str, tool_calls: &[ToolCall]) -> Value {
    let mut message = json!({"role": "assistant"});
    message["content"] = if text.is_empty() && !tool_calls.is_empty() {
        Value::Null
    } else {
        Value::String(text.to_owned())
    };
    if !tool_calls.is_empty() {
        message["tool_calls"] = Value::Array(
            tool_calls
                .iter()
                .map(|call| {
                    json!({
                        "id": call.id,
                        "type": "function",
                        "function": {"name": call.name, "arguments": call.arguments_json},
                    })
                })
                .collect(),
        );
    }
    message
}

/// Renders one tool definition.
fn tool_json(tool: &ToolSpec) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": tool.name,
            "description": tool.description,
            "parameters": tool.parameters,
        },
    })
}

/// A tool call being assembled from argument fragments.
#[derive(Debug, Default)]
struct PartialCall {
    /// The id, once a chunk has carried it.
    id: Option<String>,
    /// Name fragments concatenated.
    name: String,
    /// Argument fragments concatenated.
    arguments: String,
}

/// Collects streamed tool-call fragments by their `index`.
#[derive(Debug, Default)]
pub struct AccumulatingToolCalls {
    /// Partial calls keyed by the chunk's `index`.
    calls: BTreeMap<u64, PartialCall>,
}

impl AccumulatingToolCalls {
    /// Absorbs the `tool_calls` array of one delta.
    ///
    /// Continuation chunks often carry `id: null` and no name; those fields
    /// are only set when present.
    ///
    /// # Arguments
    ///
    /// * `tool_calls` - The array from `choices[0].delta.tool_calls`.
    pub fn absorb(&mut self, tool_calls: &[Value]) {
        for fragment in tool_calls {
            let index = fragment.get("index").and_then(Value::as_u64).unwrap_or(0);
            let call = self.calls.entry(index).or_default();
            if let Some(id) = fragment
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
            {
                call.id = Some(id.to_owned());
            }
            if let Some(function) = fragment.get("function") {
                if let Some(name) = function.get("name").and_then(Value::as_str) {
                    call.name.push_str(name);
                }
                if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
                    call.arguments.push_str(arguments);
                }
            }
        }
    }

    /// Whether no fragments have been absorbed.
    ///
    /// # Returns
    ///
    /// `true` when empty.
    pub fn is_empty(&self) -> bool {
        self.calls.is_empty()
    }

    /// Drains the completed calls in index order.
    ///
    /// Missing ids become `call_<index>` and empty arguments become `{}`.
    ///
    /// # Returns
    ///
    /// The calls.
    pub fn finish(&mut self) -> Vec<ToolCall> {
        std::mem::take(&mut self.calls)
            .into_iter()
            .map(|(index, call)| ToolCall {
                id: call.id.unwrap_or_else(|| format!("call_{index}")),
                name: call.name,
                arguments_json: if call.arguments.trim().is_empty() {
                    "{}".to_owned()
                } else {
                    call.arguments
                },
            })
            .collect()
    }
}

/// Decoder state for one chat-completions stream.
#[derive(Debug, Default)]
pub(crate) struct OpenAiDecoder {
    /// Tool calls being assembled.
    calls: AccumulatingToolCalls,
    /// Whether a reasoning block is open and needs closing.
    reasoning_open: bool,
    /// The mapped `finish_reason`, held until `[DONE]`.
    pending_stop: Option<StopReason>,
    /// Whether usage was already emitted.
    usage_emitted: bool,
    /// Whether a terminal item was produced.
    done: bool,
}

impl Decoder for OpenAiDecoder {
    fn on_frame(&mut self, frame: SseFrame) -> Items {
        if self.done {
            return Vec::new();
        }
        if frame.data.trim() == "[DONE]" {
            self.done = true;
            return vec![match self.pending_stop.take() {
                Some(stop_reason) => Ok(Event::Done { stop_reason }),
                None => Err(ProviderError::malformed(
                    "stream ended before finish_reason",
                )),
            }];
        }
        match serde_json::from_str::<Value>(&frame.data) {
            Ok(chunk) => openai_chunk_to_events(&chunk, self),
            Err(error) => {
                self.done = true;
                vec![Err(ProviderError::malformed(format!(
                    "chunk is not JSON: {error}: {}",
                    frame.data
                )))]
            }
        }
    }

    fn on_end(&mut self) -> Items {
        if self.done {
            return Vec::new();
        }
        self.done = true;
        vec![match self.pending_stop.take() {
            Some(stop_reason) => Ok(Event::Done { stop_reason }),
            None => Err(ProviderError::malformed(
                "stream ended before finish_reason",
            )),
        }]
    }
}

/// Converts one streamed chunk into events.
///
/// # Arguments
///
/// * `chunk` - The parsed `data:` payload.
/// * `state` - Decoder state carried between chunks.
///
/// # Returns
///
/// Events in the order they should reach the loop.
pub(crate) fn openai_chunk_to_events(chunk: &Value, state: &mut OpenAiDecoder) -> Items {
    if let Some(error) = chunk.get("error") {
        state.done = true;
        return vec![Err(ProviderError::Api {
            kind: error
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("error")
                .to_owned(),
            message: error
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| error.to_string()),
        })];
    }
    let mut events = Vec::new();
    if let Some(usage) = chunk.get("usage").filter(|usage| !usage.is_null())
        && !state.usage_emitted
    {
        state.usage_emitted = true;
        events.push(Ok(Event::Usage(parse_usage(usage))));
    }
    let Some(choice) = chunk
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first())
    else {
        return events;
    };
    if let Some(delta) = choice.get("delta") {
        let reasoning = delta
            .get("reasoning_content")
            .or_else(|| delta.get("reasoning"))
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty());
        if let Some(reasoning) = reasoning {
            state.reasoning_open = true;
            events.push(Ok(Event::ReasoningDelta(reasoning.to_owned())));
        }
        if let Some(content) = delta
            .get("content")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
        {
            close_reasoning(state, &mut events);
            events.push(Ok(Event::TextDelta(content.to_owned())));
        }
        if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
            state.calls.absorb(tool_calls);
        }
    }
    if let Some(finish) = choice.get("finish_reason").and_then(Value::as_str) {
        close_reasoning(state, &mut events);
        let had_calls = !state.calls.is_empty();
        for call in state.calls.finish() {
            events.push(Ok(Event::ToolCall(call)));
        }
        state.pending_stop = Some(match finish {
            "tool_calls" => StopReason::ToolUse,
            "stop" if had_calls => StopReason::ToolUse,
            "stop" => StopReason::EndTurn,
            "length" => StopReason::MaxTokens,
            other => StopReason::Other(other.to_owned()),
        });
    }
    events
}

/// Emits `ReasoningEnd` if a reasoning block is open.
fn close_reasoning(state: &mut OpenAiDecoder, events: &mut Items) {
    if state.reasoning_open {
        state.reasoning_open = false;
        events.push(Ok(Event::ReasoningEnd {
            signature: None,
            redacted_data: None,
        }));
    }
}

/// Reads a `usage` object; cached prompt tokens are split out of `input`.
fn parse_usage(usage: &Value) -> Usage {
    let prompt = usage
        .get("prompt_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = usage
        .get("completion_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cached = usage
        .get("prompt_tokens_details")
        .and_then(|details| details.get("cached_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(prompt);
    Usage {
        input: prompt - cached,
        output,
        cached,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::sse::SseParser;
    use crate::provider::types::{ReasoningBlock, ToolResult};

    fn request() -> CompletionRequest {
        CompletionRequest {
            system: "sys".to_owned(),
            messages: vec![
                Message::user("hi"),
                Message::Assistant {
                    text: String::new(),
                    reasoning: vec![ReasoningBlock {
                        text: "thinking".to_owned(),
                        ..ReasoningBlock::default()
                    }],
                    tool_calls: vec![ToolCall {
                        id: "c1".to_owned(),
                        name: "read_file".to_owned(),
                        arguments_json: "{\"path\":\"a\"}".to_owned(),
                    }],
                },
                Message::ToolResults {
                    results: vec![ToolResult {
                        call_id: "c1".to_owned(),
                        content: "boom".to_owned(),
                        is_error: true,
                    }],
                },
            ],
            tools: vec![ToolSpec {
                name: "read_file".to_owned(),
                description: "Reads".to_owned(),
                parameters: json!({"type": "object"}),
            }],
            max_output_tokens: None,
        }
    }

    fn chunk(delta: Value, finish: Option<&str>) -> Value {
        json!({"choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
    }

    fn ok_events(items: Items) -> Vec<Event> {
        items
            .into_iter()
            .map(|item| item.expect("ok event"))
            .collect()
    }

    #[test]
    fn body_includes_stream_options_and_tools() {
        let body = build_body("m", Some(99), &request());
        assert_eq!(body["stream"], json!(true));
        assert_eq!(body["stream_options"]["include_usage"], json!(true));
        assert_eq!(body["max_tokens"], json!(99));
        assert_eq!(
            body["messages"][0],
            json!({"role": "system", "content": "sys"})
        );
        assert_eq!(body["tools"][0]["type"], json!("function"));
        assert_eq!(body["tools"][0]["function"]["name"], json!("read_file"));
        assert!(
            build_body("m", None, &request())
                .get("max_tokens")
                .is_none()
        );
    }

    #[test]
    fn body_assistant_with_tool_calls_and_tool_results() {
        let body = build_body("m", None, &request());
        let assistant = &body["messages"][2];
        assert_eq!(assistant["content"], Value::Null);
        assert_eq!(assistant["tool_calls"][0]["id"], json!("c1"));
        assert_eq!(
            assistant["tool_calls"][0]["function"]["arguments"],
            json!("{\"path\":\"a\"}")
        );
        assert!(assistant.get("reasoning_content").is_none());
        let tool = &body["messages"][3];
        assert_eq!(tool["role"], json!("tool"));
        assert_eq!(tool["tool_call_id"], json!("c1"));
        assert_eq!(tool["content"], json!("Error: boom"));
    }

    #[test]
    fn body_omits_tools_and_system_when_empty() {
        let body = build_body("m", None, &CompletionRequest::default());
        assert!(body.get("tools").is_none());
        assert_eq!(body["messages"], json!([]));
    }

    #[test]
    fn chunk_text_delta() {
        let mut state = OpenAiDecoder::default();
        let events = ok_events(openai_chunk_to_events(
            &chunk(json!({"content": "hi"}), None),
            &mut state,
        ));
        assert_eq!(events, vec![Event::TextDelta("hi".to_owned())]);
        let events = openai_chunk_to_events(&chunk(json!({"content": null}), None), &mut state);
        assert!(events.is_empty());
    }

    #[test]
    fn chunk_reasoning_then_content_closes_reasoning() {
        let mut state = OpenAiDecoder::default();
        let mut events = ok_events(openai_chunk_to_events(
            &chunk(json!({"reasoning_content": "hmm"}), None),
            &mut state,
        ));
        events.extend(ok_events(openai_chunk_to_events(
            &chunk(json!({"content": "so"}), None),
            &mut state,
        )));
        assert_eq!(
            events,
            vec![
                Event::ReasoningDelta("hmm".to_owned()),
                Event::ReasoningEnd {
                    signature: None,
                    redacted_data: None
                },
                Event::TextDelta("so".to_owned()),
            ]
        );
    }

    #[test]
    fn tool_call_accumulated_across_chunks_with_null_id() {
        let mut state = OpenAiDecoder::default();
        let first = chunk(
            json!({"tool_calls": [{"index": 0, "id": "c1", "type": "function", "function": {"name": "read", "arguments": "{\"pa"}}]}),
            None,
        );
        let second = chunk(
            json!({"tool_calls": [{"index": 0, "id": null, "function": {"arguments": "th\":1}"}}]}),
            None,
        );
        assert!(openai_chunk_to_events(&first, &mut state).is_empty());
        assert!(openai_chunk_to_events(&second, &mut state).is_empty());
        let events = ok_events(openai_chunk_to_events(
            &chunk(json!({}), Some("tool_calls")),
            &mut state,
        ));
        assert_eq!(
            events,
            vec![Event::ToolCall(ToolCall {
                id: "c1".to_owned(),
                name: "read".to_owned(),
                arguments_json: "{\"path\":1}".to_owned(),
            })]
        );
        assert_eq!(state.pending_stop, Some(StopReason::ToolUse));
    }

    #[test]
    fn parallel_tool_calls_ordered_by_index() {
        let mut state = OpenAiDecoder::default();
        let frag = chunk(
            json!({"tool_calls": [
                {"index": 1, "id": "b", "function": {"name": "two", "arguments": "{}"}},
                {"index": 0, "id": "a", "function": {"name": "one", "arguments": "{}"}}
            ]}),
            Some("tool_calls"),
        );
        let events = ok_events(openai_chunk_to_events(&frag, &mut state));
        let names: Vec<_> = events
            .iter()
            .map(|event| match event {
                Event::ToolCall(call) => call.name.clone(),
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(names, vec!["one", "two"]);
    }

    #[test]
    fn missing_id_synthesized_and_empty_args_default() {
        let mut state = OpenAiDecoder::default();
        let frag = chunk(
            json!({"tool_calls": [{"index": 2, "function": {"name": "ls"}}]}),
            Some("stop"),
        );
        let events = ok_events(openai_chunk_to_events(&frag, &mut state));
        assert_eq!(
            events,
            vec![Event::ToolCall(ToolCall {
                id: "call_2".to_owned(),
                name: "ls".to_owned(),
                arguments_json: "{}".to_owned(),
            })]
        );
        assert_eq!(
            state.pending_stop,
            Some(StopReason::ToolUse),
            "stop with calls is tool use"
        );
    }

    #[test]
    fn finish_reasons_map() {
        for (finish, expected) in [
            ("stop", StopReason::EndTurn),
            ("length", StopReason::MaxTokens),
            (
                "content_filter",
                StopReason::Other("content_filter".to_owned()),
            ),
        ] {
            let mut state = OpenAiDecoder::default();
            openai_chunk_to_events(&chunk(json!({}), Some(finish)), &mut state);
            assert_eq!(state.pending_stop, Some(expected));
        }
    }

    #[test]
    fn usage_chunk_with_empty_choices_then_done() {
        let mut decoder = OpenAiDecoder::default();
        decoder.on_frame(SseFrame {
            event: None,
            data: chunk(json!({"content": "x"}), Some("stop")).to_string(),
        });
        let usage = json!({"choices": [], "usage": {"prompt_tokens": 12, "completion_tokens": 3, "prompt_tokens_details": {"cached_tokens": 4}}});
        let events = ok_events(decoder.on_frame(SseFrame {
            event: None,
            data: usage.to_string(),
        }));
        assert_eq!(
            events,
            vec![Event::Usage(Usage {
                input: 8,
                output: 3,
                cached: 4
            })]
        );
        let events = ok_events(decoder.on_frame(SseFrame {
            event: None,
            data: "[DONE]".to_owned(),
        }));
        assert_eq!(
            events,
            vec![Event::Done {
                stop_reason: StopReason::EndTurn
            }]
        );
        assert!(
            decoder
                .on_frame(SseFrame {
                    event: None,
                    data: "[DONE]".to_owned()
                })
                .is_empty()
        );
        assert!(decoder.on_end().is_empty());
    }

    #[test]
    fn done_before_finish_reason_is_malformed() {
        let mut decoder = OpenAiDecoder::default();
        let items = decoder.on_frame(SseFrame {
            event: None,
            data: "[DONE]".to_owned(),
        });
        assert!(matches!(
            items.as_slice(),
            [Err(ProviderError::Malformed { .. })]
        ));
    }

    #[test]
    fn error_object_yields_api_error() {
        let mut decoder = OpenAiDecoder::default();
        let items = decoder.on_frame(SseFrame {
            event: None,
            data: json!({"error": {"message": "model not found", "type": "invalid_request_error"}})
                .to_string(),
        });
        assert_eq!(
            items,
            vec![Err(ProviderError::Api {
                kind: "invalid_request_error".to_owned(),
                message: "model not found".to_owned(),
            })]
        );
    }

    #[test]
    fn non_json_chunk_is_malformed() {
        let mut decoder = OpenAiDecoder::default();
        let items = decoder.on_frame(SseFrame {
            event: None,
            data: "<html>".to_owned(),
        });
        assert!(matches!(
            items.as_slice(),
            [Err(ProviderError::Malformed { .. })]
        ));
    }

    #[test]
    fn eof_without_done_emits_done() {
        let mut decoder = OpenAiDecoder::default();
        decoder.on_frame(SseFrame {
            event: None,
            data: chunk(json!({}), Some("stop")).to_string(),
        });
        assert_eq!(
            ok_events(decoder.on_end()),
            vec![Event::Done {
                stop_reason: StopReason::EndTurn
            }]
        );
        let mut fresh = OpenAiDecoder::default();
        assert!(matches!(
            fresh.on_end().as_slice(),
            [Err(ProviderError::Malformed { .. })]
        ));
    }

    #[test]
    fn full_vllm_fixture_stream() {
        let transcript = concat!(
            "data: {\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"Need the file.\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"chatcmpl-tool-1\",\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":null,\"function\":{\"arguments\":\"{\\\"path\\\": \"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":null,\"function\":{\"arguments\":\"\\\"a.rs\\\"}\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"x\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: {\"id\":\"x\",\"choices\":[],\"usage\":{\"prompt_tokens\":50,\"completion_tokens\":20,\"total_tokens\":70}}\n\n",
            "data: [DONE]\n\n",
        );
        let mut parser = SseParser::default();
        let mut decoder = OpenAiDecoder::default();
        let mut events = Vec::new();
        for frame in parser.push(transcript.as_bytes()) {
            events.extend(ok_events(decoder.on_frame(frame)));
        }
        assert_eq!(
            events,
            vec![
                Event::ReasoningDelta("Need the file.".to_owned()),
                Event::ReasoningEnd {
                    signature: None,
                    redacted_data: None
                },
                Event::ToolCall(ToolCall {
                    id: "chatcmpl-tool-1".to_owned(),
                    name: "read_file".to_owned(),
                    arguments_json: "{\"path\": \"a.rs\"}".to_owned(),
                }),
                Event::Usage(Usage {
                    input: 50,
                    output: 20,
                    cached: 0
                }),
                Event::Done {
                    stop_reason: StopReason::ToolUse
                },
            ]
        );
    }
}
