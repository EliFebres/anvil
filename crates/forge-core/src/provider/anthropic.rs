//! The Anthropic Messages API with SSE streaming.

use std::collections::BTreeMap;
use std::time::Duration;

use forge_types::{LimitsConfig, ProviderConfig};
use serde_json::{Value, json};

use super::error::ProviderError;
use super::http::{self, Decoder, Items, StreamConfig};
use super::retry::RetryPolicy;
use super::sse::SseFrame;
use super::types::{CompletionRequest, Event, Message, StopReason, ToolCall, Usage};
use super::{EventStream, Provider};

/// The API version header value this adapter targets.
pub const ANTHROPIC_VERSION: &str = "2026-10-21";

/// Output cap used when neither the request nor the config sets one.
pub const DEFAULT_MAX_TOKENS: u32 = 16_384;

/// A provider speaking `POST {base_url}/messages` with SSE streaming.
pub struct AnthropicProvider {
    /// HTTP client shared by every request.
    client: reqwest::Client,
    /// Full endpoint URL.
    url: String,
    /// `x-api-key` header value.
    api_key: String,
    /// Model name as the server expects it.
    model: String,
    /// Default output cap.
    max_output_tokens: u32,
    /// Whether to request thinking.
    supports_reasoning: bool,
    /// Deadline and retry policy.
    stream_cfg: StreamConfig,
}

impl AnthropicProvider {
    /// Creates the adapter.
    ///
    /// # Arguments
    ///
    /// * `name` - Config entry name, for logs.
    /// * `cfg` - The provider entry.
    /// * `api_key` - Sent as `x-api-key`.
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
            url: format!("{}/messages", cfg.base_url.trim_end_matches('/')),
            api_key,
            model: cfg.model.clone(),
            max_output_tokens: cfg.max_output_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
            supports_reasoning: cfg.supports_reasoning,
            stream_cfg: StreamConfig {
                name: name.to_owned(),
                timeout: Duration::from_secs(limits.request_timeout_s),
                policy: RetryPolicy::from_limits(limits),
            },
        }
    }
}

impl Provider for AnthropicProvider {
    fn complete(&self, request: CompletionRequest) -> EventStream {
        let max_tokens = request.max_output_tokens.unwrap_or(self.max_output_tokens);
        let body = match build_body(&self.model, max_tokens, self.supports_reasoning, &request) {
            Ok(body) => body,
            Err(error) => return Box::pin(futures_util::stream::iter([Err(error)])),
        };
        let client = self.client.clone();
        let url = self.url.clone();
        let api_key = self.api_key.clone();
        let attempt = move |index: u32| {
            tracing::debug!(attempt = index, url = %url, "sending messages request");
            let builder = client
                .post(&url)
                .header("x-api-key", &api_key)
                .header("anthropic-version", ANTHROPIC_VERSION)
                .json(&body);
            http::send_attempt(builder)
        };
        http::start(
            self.stream_cfg.clone(),
            attempt,
            AnthropicDecoder::default(),
        )
    }
}

/// Builds the JSON request body.
///
/// The system prompt and the last tool carry `cache_control` so the stable
/// prefix is cached across turns. Thinking blocks are replayed exactly as
/// received, which the API requires on tool-use continuations.
///
/// # Arguments
///
/// * `model` - Model name.
/// * `max_tokens` - Output cap; the API requires one.
/// * `supports_reasoning` - Whether to request adaptive thinking.
/// * `request` - The completion request.
///
/// # Returns
///
/// The body.
///
/// # Errors
///
/// [`ProviderError::Malformed`] when a tool call's arguments are not valid JSON.
pub(crate) fn build_body(
    model: &str,
    max_tokens: u32,
    supports_reasoning: bool,
    request: &CompletionRequest,
) -> Result<Value, ProviderError> {
    let mut messages = Vec::new();
    for message in &request.messages {
        match message {
            Message::User { text } => messages.push(json!({
                "role": "user",
                "content": [{"type": "text", "text": text}],
            })),
            Message::Assistant {
                text,
                reasoning,
                tool_calls,
            } => {
                let mut blocks = Vec::new();
                for block in reasoning {
                    if let Some(data) = &block.redacted_data {
                        blocks.push(json!({"type": "redacted_thinking", "data": data}));
                    } else if let Some(signature) = &block.signature {
                        blocks.push(json!({
                            "type": "thinking",
                            "thinking": block.text,
                            "signature": signature,
                        }));
                    }
                }
                if !text.is_empty() {
                    blocks.push(json!({"type": "text", "text": text}));
                }
                for call in tool_calls {
                    let input: Value =
                        serde_json::from_str(&call.arguments_json).map_err(|error| {
                            ProviderError::malformed(format!(
                                "tool call {} has invalid JSON arguments: {error}",
                                call.id
                            ))
                        })?;
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": call.id,
                        "name": call.name,
                        "input": input,
                    }));
                }
                if !blocks.is_empty() {
                    messages.push(json!({"role": "assistant", "content": blocks}));
                }
            }
            Message::ToolResults { results } => {
                let blocks: Vec<Value> = results
                    .iter()
                    .map(|result| {
                        json!({
                            "type": "tool_result",
                            "tool_use_id": result.call_id,
                            "content": result.content,
                            "is_error": result.is_error,
                        })
                    })
                    .collect();
                messages.push(json!({"role": "user", "content": blocks}));
            }
        }
    }
    let mut body = json!({
        "model": model,
        "max_tokens": max_tokens,
        "stream": true,
        "messages": messages,
    });
    if !request.system.is_empty() {
        body["system"] = json!([{
            "type": "text",
            "text": request.system,
            "cache_control": {"type": "ephemeral"},
        }]);
    }
    if !request.tools.is_empty() {
        let mut tools: Vec<Value> = request
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "name": tool.name,
                    "description": tool.description,
                    "input_schema": tool.parameters,
                })
            })
            .collect();
        if let Some(last) = tools.last_mut() {
            last["cache_control"] = json!({"type": "ephemeral"});
        }
        body["tools"] = Value::Array(tools);
    }
    if supports_reasoning {
        body["thinking"] = json!({"type": "adaptive", "display": "summarized"});
    }
    Ok(body)
}

/// What is known about one content block while it streams.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum BlockState {
    /// A visible text block.
    Text,
    /// A thinking block; the signature arrives at the end.
    Thinking {
        /// The signature, once seen.
        signature: Option<String>,
    },
    /// A redacted thinking block, replayed by its opaque data.
    Redacted {
        /// The opaque data.
        data: String,
    },
    /// A tool call whose arguments arrive as JSON fragments.
    ToolUse {
        /// The call id.
        id: String,
        /// The tool name.
        name: String,
        /// Argument JSON fragments concatenated.
        json: String,
    },
}

/// Decoder state for one messages stream.
#[derive(Debug, Default)]
pub(crate) struct AnthropicDecoder {
    /// Open blocks keyed by their index.
    blocks: BTreeMap<u64, BlockState>,
    /// Uncached input tokens from `message_start`.
    input: u64,
    /// Cached input tokens from `message_start`.
    cached: u64,
    /// The stop reason from `message_delta`.
    stop: Option<StopReason>,
    /// Whether usage was already emitted.
    usage_emitted: bool,
    /// Whether a terminal item was produced.
    done: bool,
}

impl Decoder for AnthropicDecoder {
    fn on_frame(&mut self, frame: SseFrame) -> Items {
        if self.done {
            return Vec::new();
        }
        let data: Value = match serde_json::from_str(&frame.data) {
            Ok(data) => data,
            Err(error) => {
                self.done = true;
                return vec![Err(ProviderError::malformed(format!(
                    "event is not JSON: {error}: {}",
                    frame.data
                )))];
            }
        };
        let kind = frame
            .event
            .clone()
            .or_else(|| data.get("type").and_then(Value::as_str).map(str::to_owned))
            .unwrap_or_default();
        anthropic_event_to_events(&kind, &data, self)
    }

    fn on_end(&mut self) -> Items {
        if self.done {
            return Vec::new();
        }
        self.done = true;
        vec![match self.stop.take() {
            Some(stop_reason) => Ok(Event::Done { stop_reason }),
            None => Err(ProviderError::malformed("stream ended before message_stop")),
        }]
    }
}

/// Converts one streamed event into loop events.
///
/// # Arguments
///
/// * `kind` - The SSE event name, such as `content_block_delta`.
/// * `data` - The parsed payload.
/// * `state` - Decoder state carried between events.
///
/// # Returns
///
/// Events in the order they should reach the loop.
pub(crate) fn anthropic_event_to_events(
    kind: &str,
    data: &Value,
    state: &mut AnthropicDecoder,
) -> Items {
    match kind {
        "message_start" => {
            let usage = data.get("message").and_then(|m| m.get("usage"));
            state.input =
                count(usage, "input_tokens") + count(usage, "cache_creation_input_tokens");
            state.cached = count(usage, "cache_read_input_tokens");
            Vec::new()
        }
        "content_block_start" => {
            let index = index_of(data);
            let block = data.get("content_block");
            let block_type = block.and_then(|b| b.get("type")).and_then(Value::as_str);
            let block_state = match block_type {
                Some("thinking") => BlockState::Thinking { signature: None },
                Some("redacted_thinking") => BlockState::Redacted {
                    data: string_field(block, "data"),
                },
                Some("tool_use") => BlockState::ToolUse {
                    id: string_field(block, "id"),
                    name: string_field(block, "name"),
                    json: String::new(),
                },
                Some("text") => BlockState::Text,
                other => {
                    tracing::debug!(block_type = ?other, "treating unknown content block as text");
                    BlockState::Text
                }
            };
            state.blocks.insert(index, block_state);
            Vec::new()
        }
        "content_block_delta" => {
            let index = index_of(data);
            let Some(block) = state.blocks.get_mut(&index) else {
                state.done = true;
                return vec![Err(ProviderError::malformed(format!(
                    "delta for unknown content block {index}"
                )))];
            };
            let delta = data.get("delta");
            match delta.and_then(|d| d.get("type")).and_then(Value::as_str) {
                Some("text_delta") => vec![Ok(Event::TextDelta(string_field(delta, "text")))],
                Some("thinking_delta") => {
                    vec![Ok(Event::ReasoningDelta(string_field(delta, "thinking")))]
                }
                Some("signature_delta") => {
                    if let BlockState::Thinking { signature } = block {
                        *signature = Some(string_field(delta, "signature"));
                    }
                    Vec::new()
                }
                Some("input_json_delta") => {
                    if let BlockState::ToolUse { json, .. } = block {
                        json.push_str(&string_field(delta, "partial_json"));
                    }
                    Vec::new()
                }
                _ => Vec::new(),
            }
        }
        "content_block_stop" => {
            let index = index_of(data);
            let Some(block) = state.blocks.remove(&index) else {
                state.done = true;
                return vec![Err(ProviderError::malformed(format!(
                    "stop for unknown content block {index}"
                )))];
            };
            match block {
                BlockState::Text => Vec::new(),
                BlockState::Thinking { signature } => vec![Ok(Event::ReasoningEnd {
                    signature,
                    redacted_data: None,
                })],
                BlockState::Redacted { data } => vec![Ok(Event::ReasoningEnd {
                    signature: None,
                    redacted_data: Some(data),
                })],
                BlockState::ToolUse { id, name, json } => vec![Ok(Event::ToolCall(ToolCall {
                    id,
                    name,
                    arguments_json: if json.trim().is_empty() {
                        "{}".to_owned()
                    } else {
                        json
                    },
                }))],
            }
        }
        "message_delta" => {
            let mut events = Vec::new();
            if let Some(stop) = data
                .get("delta")
                .and_then(|d| d.get("stop_reason"))
                .and_then(Value::as_str)
            {
                state.stop = Some(match stop {
                    "end_turn" => StopReason::EndTurn,
                    "tool_use" => StopReason::ToolUse,
                    "max_tokens" => StopReason::MaxTokens,
                    other => StopReason::Other(other.to_owned()),
                });
            }
            if let Some(usage) = data.get("usage")
                && !state.usage_emitted
            {
                state.usage_emitted = true;
                events.push(Ok(Event::Usage(Usage {
                    input: state.input,
                    output: count(Some(usage), "output_tokens"),
                    cached: state.cached,
                })));
            }
            events
        }
        "message_stop" => {
            state.done = true;
            vec![match state.stop.take() {
                Some(stop_reason) => Ok(Event::Done { stop_reason }),
                None => Err(ProviderError::malformed(
                    "message_stop before message_delta",
                )),
            }]
        }
        "error" => {
            state.done = true;
            let error = data.get("error");
            vec![Err(ProviderError::Api {
                kind: string_field(error, "type"),
                message: string_field(error, "message"),
            })]
        }
        "ping" => Vec::new(),
        other => {
            tracing::debug!(event = other, "ignoring unknown stream event");
            Vec::new()
        }
    }
}

/// Reads `data.index`, defaulting to zero.
fn index_of(data: &Value) -> u64 {
    data.get("index").and_then(Value::as_u64).unwrap_or(0)
}

/// Reads an unsigned counter from an optional object.
fn count(object: Option<&Value>, key: &str) -> u64 {
    object
        .and_then(|o| o.get(key))
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

/// Reads a string from an optional object, defaulting to empty.
fn string_field(object: Option<&Value>, key: &str) -> String {
    object
        .and_then(|o| o.get(key))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::sse::SseParser;
    use crate::provider::types::{ReasoningBlock, ToolResult, ToolSpec};

    fn request() -> CompletionRequest {
        CompletionRequest {
            system: "sys".to_owned(),
            messages: vec![
                Message::user("hi"),
                Message::Assistant {
                    text: "look".to_owned(),
                    reasoning: vec![
                        ReasoningBlock {
                            text: "plan".to_owned(),
                            signature: Some("sig1".to_owned()),
                            redacted_data: None,
                        },
                        ReasoningBlock {
                            text: String::new(),
                            signature: None,
                            redacted_data: Some("opaque".to_owned()),
                        },
                        ReasoningBlock {
                            text: "unsigned".to_owned(),
                            signature: None,
                            redacted_data: None,
                        },
                    ],
                    tool_calls: vec![ToolCall {
                        id: "toolu_1".to_owned(),
                        name: "read_file".to_owned(),
                        arguments_json: "{\"path\":\"a\"}".to_owned(),
                    }],
                },
                Message::ToolResults {
                    results: vec![ToolResult {
                        call_id: "toolu_1".to_owned(),
                        content: "nope".to_owned(),
                        is_error: true,
                    }],
                },
            ],
            tools: vec![
                ToolSpec {
                    name: "read_file".to_owned(),
                    description: "Reads".to_owned(),
                    parameters: json!({"type": "object"}),
                },
                ToolSpec {
                    name: "glob".to_owned(),
                    description: "Finds".to_owned(),
                    parameters: json!({"type": "object"}),
                },
            ],
            max_output_tokens: None,
        }
    }

    fn feed(decoder: &mut AnthropicDecoder, kind: &str, data: Value) -> Vec<Event> {
        anthropic_event_to_events(kind, &data, decoder)
            .into_iter()
            .map(|item| item.expect("ok event"))
            .collect()
    }

    #[test]
    fn body_system_and_last_tool_have_cache_control() {
        let body = build_body("m", 100, false, &request()).expect("builds");
        assert_eq!(body["max_tokens"], json!(100));
        assert_eq!(body["stream"], json!(true));
        assert_eq!(
            body["system"][0]["cache_control"],
            json!({"type": "ephemeral"})
        );
        assert!(body["tools"][0].get("cache_control").is_none());
        assert_eq!(
            body["tools"][1]["cache_control"],
            json!({"type": "ephemeral"})
        );
        assert_eq!(body["tools"][1]["input_schema"], json!({"type": "object"}));
    }

    #[test]
    fn body_thinking_only_when_supports_reasoning() {
        let with = build_body("m", 1, true, &request()).expect("builds");
        assert_eq!(
            with["thinking"],
            json!({"type": "adaptive", "display": "summarized"})
        );
        let without = build_body("m", 1, false, &request()).expect("builds");
        assert!(without.get("thinking").is_none());
        for key in ["temperature", "top_p", "top_k"] {
            assert!(with.get(key).is_none(), "{key} must not be sent");
        }
    }

    #[test]
    fn body_replays_thinking_blocks_verbatim_and_skips_unsigned() {
        let body = build_body("m", 1, true, &request()).expect("builds");
        let content = body["messages"][1]["content"].as_array().expect("array");
        assert_eq!(
            content[0],
            json!({"type": "thinking", "thinking": "plan", "signature": "sig1"})
        );
        assert_eq!(
            content[1],
            json!({"type": "redacted_thinking", "data": "opaque"})
        );
        assert_eq!(content[2], json!({"type": "text", "text": "look"}));
        assert_eq!(
            content[3],
            json!({"type": "tool_use", "id": "toolu_1", "name": "read_file", "input": {"path": "a"}})
        );
        assert_eq!(content.len(), 4, "unsigned reasoning is dropped");
    }

    #[test]
    fn body_tool_results_as_user_blocks() {
        let body = build_body("m", 1, false, &request()).expect("builds");
        assert_eq!(
            body["messages"][2],
            json!({"role": "user", "content": [{
                "type": "tool_result", "tool_use_id": "toolu_1", "content": "nope", "is_error": true
            }]})
        );
    }

    #[test]
    fn body_invalid_tool_arguments_is_malformed() {
        let mut request = request();
        if let Message::Assistant { tool_calls, .. } = &mut request.messages[1] {
            tool_calls[0].arguments_json = "{".to_owned();
        }
        assert!(matches!(
            build_body("m", 1, false, &request),
            Err(ProviderError::Malformed { .. })
        ));
    }

    #[test]
    fn body_omits_empty_system_and_tools() {
        let body = build_body("m", 1, false, &CompletionRequest::default()).expect("builds");
        assert!(body.get("system").is_none());
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn max_tokens_precedence() {
        let mut cfg = ProviderConfig {
            protocol: forge_types::Protocol::Anthropic,
            base_url: "https://api.anthropic.com/v1/".to_owned(),
            api_key_env: "K".to_owned(),
            model: "m".to_owned(),
            context_window: 1,
            supports_tools: true,
            supports_reasoning: true,
            max_output_tokens: None,
            price_per_mtoken: None,
        };
        let limits = LimitsConfig::default();
        let client = reqwest::Client::new();
        let provider = AnthropicProvider::new("a", &cfg, "k".to_owned(), &limits, client.clone());
        assert_eq!(provider.max_output_tokens, DEFAULT_MAX_TOKENS);
        assert_eq!(provider.url, "https://api.anthropic.com/v1/messages");
        cfg.max_output_tokens = Some(4096);
        let provider = AnthropicProvider::new("a", &cfg, "k".to_owned(), &limits, client);
        assert_eq!(provider.max_output_tokens, 4096);
    }

    #[test]
    fn events_text_block_lifecycle() {
        let mut decoder = AnthropicDecoder::default();
        assert!(
            feed(
                &mut decoder,
                "content_block_start",
                json!({"index": 0, "content_block": {"type": "text", "text": ""}})
            )
            .is_empty()
        );
        assert_eq!(
            feed(
                &mut decoder,
                "content_block_delta",
                json!({"index": 0, "delta": {"type": "text_delta", "text": "Hi"}})
            ),
            vec![Event::TextDelta("Hi".to_owned())]
        );
        assert!(feed(&mut decoder, "content_block_stop", json!({"index": 0})).is_empty());
    }

    #[test]
    fn events_tool_use_accumulates_partial_json() {
        let mut decoder = AnthropicDecoder::default();
        feed(
            &mut decoder,
            "content_block_start",
            json!({"index": 1, "content_block": {"type": "tool_use", "id": "toolu_9", "name": "glob", "input": {}}}),
        );
        feed(
            &mut decoder,
            "content_block_delta",
            json!({"index": 1, "delta": {"type": "input_json_delta", "partial_json": "{\"pattern\":"}}),
        );
        feed(
            &mut decoder,
            "content_block_delta",
            json!({"index": 1, "delta": {"type": "input_json_delta", "partial_json": " \"*.rs\"}"}}),
        );
        assert_eq!(
            feed(&mut decoder, "content_block_stop", json!({"index": 1})),
            vec![Event::ToolCall(ToolCall {
                id: "toolu_9".to_owned(),
                name: "glob".to_owned(),
                arguments_json: "{\"pattern\": \"*.rs\"}".to_owned(),
            })]
        );
        feed(
            &mut decoder,
            "content_block_start",
            json!({"index": 2, "content_block": {"type": "tool_use", "id": "toolu_10", "name": "list_dir", "input": {}}}),
        );
        let events = feed(&mut decoder, "content_block_stop", json!({"index": 2}));
        assert!(matches!(&events[0], Event::ToolCall(call) if call.arguments_json == "{}"));
    }

    #[test]
    fn events_thinking_with_signature() {
        let mut decoder = AnthropicDecoder::default();
        feed(
            &mut decoder,
            "content_block_start",
            json!({"index": 0, "content_block": {"type": "thinking", "thinking": ""}}),
        );
        assert_eq!(
            feed(
                &mut decoder,
                "content_block_delta",
                json!({"index": 0, "delta": {"type": "thinking_delta", "thinking": "hm"}})
            ),
            vec![Event::ReasoningDelta("hm".to_owned())]
        );
        assert!(
            feed(
                &mut decoder,
                "content_block_delta",
                json!({"index": 0, "delta": {"type": "signature_delta", "signature": "abc"}})
            )
            .is_empty()
        );
        assert_eq!(
            feed(&mut decoder, "content_block_stop", json!({"index": 0})),
            vec![Event::ReasoningEnd {
                signature: Some("abc".to_owned()),
                redacted_data: None
            }]
        );
    }

    #[test]
    fn events_redacted_thinking() {
        let mut decoder = AnthropicDecoder::default();
        feed(
            &mut decoder,
            "content_block_start",
            json!({"index": 0, "content_block": {"type": "redacted_thinking", "data": "zzz"}}),
        );
        assert_eq!(
            feed(&mut decoder, "content_block_stop", json!({"index": 0})),
            vec![Event::ReasoningEnd {
                signature: None,
                redacted_data: Some("zzz".to_owned())
            }]
        );
    }

    #[test]
    fn events_usage_combines_start_and_delta() {
        let mut decoder = AnthropicDecoder::default();
        feed(
            &mut decoder,
            "message_start",
            json!({"message": {"usage": {"input_tokens": 100, "cache_read_input_tokens": 40, "cache_creation_input_tokens": 10}}}),
        );
        assert_eq!(
            feed(
                &mut decoder,
                "message_delta",
                json!({"delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 7}})
            ),
            vec![Event::Usage(Usage {
                input: 110,
                output: 7,
                cached: 40
            })]
        );
        assert_eq!(
            feed(&mut decoder, "message_stop", json!({})),
            vec![Event::Done {
                stop_reason: StopReason::EndTurn
            }]
        );
        assert!(decoder.on_end().is_empty());
    }

    #[test]
    fn events_stop_reasons() {
        for (stop, expected) in [
            ("end_turn", StopReason::EndTurn),
            ("tool_use", StopReason::ToolUse),
            ("max_tokens", StopReason::MaxTokens),
            ("refusal", StopReason::Other("refusal".to_owned())),
        ] {
            let mut decoder = AnthropicDecoder::default();
            feed(
                &mut decoder,
                "message_delta",
                json!({"delta": {"stop_reason": stop}}),
            );
            assert_eq!(decoder.stop, Some(expected));
        }
    }

    #[test]
    fn events_ping_ignored_error_surfaced() {
        let mut decoder = AnthropicDecoder::default();
        assert!(feed(&mut decoder, "ping", json!({"type": "ping"})).is_empty());
        let items = anthropic_event_to_events(
            "error",
            &json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}),
            &mut decoder,
        );
        assert_eq!(
            items,
            vec![Err(ProviderError::Api {
                kind: "overloaded_error".to_owned(),
                message: "Overloaded".to_owned(),
            })]
        );
        assert!(decoder.on_end().is_empty(), "already terminal");
    }

    #[test]
    fn events_delta_for_unknown_index_is_malformed() {
        let mut decoder = AnthropicDecoder::default();
        let items = anthropic_event_to_events(
            "content_block_delta",
            &json!({"index": 5, "delta": {"type": "text_delta", "text": "x"}}),
            &mut decoder,
        );
        assert!(matches!(
            items.as_slice(),
            [Err(ProviderError::Malformed { .. })]
        ));
    }

    #[test]
    fn eof_without_message_stop() {
        let mut decoder = AnthropicDecoder::default();
        feed(
            &mut decoder,
            "message_delta",
            json!({"delta": {"stop_reason": "end_turn"}}),
        );
        assert_eq!(
            decoder.on_end(),
            vec![Ok(Event::Done {
                stop_reason: StopReason::EndTurn
            })]
        );
        let mut fresh = AnthropicDecoder::default();
        assert!(matches!(
            fresh.on_end().as_slice(),
            [Err(ProviderError::Malformed { .. })]
        ));
    }

    #[test]
    fn frame_event_name_falls_back_to_data_type() {
        let mut decoder = AnthropicDecoder::default();
        let items = decoder.on_frame(SseFrame {
            event: None,
            data: json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text"}}).to_string(),
        });
        assert!(items.is_empty());
        assert_eq!(decoder.blocks.get(&0), Some(&BlockState::Text));
    }

    #[test]
    fn full_anthropic_fixture_stream() {
        let transcript = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":25,\"cache_read_input_tokens\":0,\"cache_creation_input_tokens\":0}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
            "event: ping\ndata: {\"type\":\"ping\"}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"Read it.\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"SIG\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"Let me look.\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"read_file\",\"input\":{}}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\": \\\"a.rs\\\"}\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":2}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":30}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        let mut parser = SseParser::default();
        let mut decoder = AnthropicDecoder::default();
        let mut events = Vec::new();
        for frame in parser.push(transcript.as_bytes()) {
            events.extend(
                decoder
                    .on_frame(frame)
                    .into_iter()
                    .map(|item| item.expect("ok")),
            );
        }
        assert_eq!(
            events,
            vec![
                Event::ReasoningDelta("Read it.".to_owned()),
                Event::ReasoningEnd {
                    signature: Some("SIG".to_owned()),
                    redacted_data: None
                },
                Event::TextDelta("Let me look.".to_owned()),
                Event::ToolCall(ToolCall {
                    id: "toolu_1".to_owned(),
                    name: "read_file".to_owned(),
                    arguments_json: "{\"path\": \"a.rs\"}".to_owned(),
                }),
                Event::Usage(Usage {
                    input: 25,
                    output: 30,
                    cached: 0
                }),
                Event::Done {
                    stop_reason: StopReason::ToolUse
                },
            ]
        );
    }
}
