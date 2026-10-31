//! Tool calling over plain text, for models whose native tool parsing is
//! broken or missing.
//!
//! The request is rewritten so the tool list rides in the system prompt and
//! the model is asked to answer with one fenced `tool` block. The response
//! stream is scanned for that block, which becomes an [`Event::ToolCall`]
//! instead of text.

use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_util::Stream;

use super::error::ProviderError;
use super::types::{CompletionRequest, Event, Message, StopReason, ToolCall, ToolSpec};
use super::{EventStream, Provider};

/// The line that opens a tool call block.
pub const OPEN_FENCE: &str = "```tool\n";

/// The text that closes a tool call block.
pub const CLOSE_FENCE: &str = "\n```";

/// Writes the instruction that teaches the model the fenced protocol.
///
/// # Arguments
///
/// * `tools` - The tools to describe.
///
/// # Returns
///
/// Markdown listing every tool with its schema, then the exact call format.
pub fn tools_instruction(tools: &[ToolSpec]) -> String {
    let mut text = String::from(
        "# Tools\n\n\
         You cannot call tools natively. To call one, reply with exactly one fenced block and \
         nothing after it. The block must contain a single JSON object with the tool `name` and \
         its `arguments`:\n\n\
         ```tool\n{\"name\": \"<tool name>\", \"arguments\": {}}\n```\n\n\
         You will receive the result in the next user message, marked `[tool_result id=...]`. \
         When you are done, answer in plain text without a block.\n\n\
         Available tools:\n",
    );
    for tool in tools {
        let schema = serde_json::to_string_pretty(&tool.parameters).unwrap_or_default();
        text.push_str(&format!(
            "\n## {}\n{}\nArguments schema:\n```json\n{}\n```\n",
            tool.name, tool.description, schema
        ));
    }
    text
}

/// Rewrites a request for a model without native tools.
///
/// Tools move into the system prompt, earlier tool calls are rendered as the
/// fenced blocks the model would have written, and tool results become user
/// text.
///
/// # Arguments
///
/// * `request` - A request that may carry tools and tool turns.
///
/// # Returns
///
/// A request with no tools and only `User` and `Assistant` turns.
pub fn rewrite_request(request: CompletionRequest) -> CompletionRequest {
    let instruction = tools_instruction(&request.tools);
    let system = if request.system.is_empty() {
        instruction
    } else {
        format!("{}\n\n{instruction}", request.system)
    };
    let messages = request
        .messages
        .into_iter()
        .map(|message| match message {
            Message::Assistant {
                text,
                reasoning,
                tool_calls,
            } => Message::Assistant {
                text: render_calls(text, &tool_calls),
                reasoning,
                tool_calls: Vec::new(),
            },
            Message::ToolResults { results } => Message::User {
                text: results
                    .iter()
                    .map(|result| {
                        format!(
                            "[tool_result id={} error={}]\n{}",
                            result.call_id, result.is_error, result.content
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n\n"),
            },
            user => user,
        })
        .collect();
    CompletionRequest {
        system,
        messages,
        tools: Vec::new(),
        max_output_tokens: request.max_output_tokens,
    }
}

/// Appends each call as a fenced block after the text.
fn render_calls(mut text: String, calls: &[ToolCall]) -> String {
    for call in calls {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&format!(
            "{OPEN_FENCE}{{\"name\": {}, \"arguments\": {}}}{CLOSE_FENCE}",
            serde_json::Value::String(call.name.clone()),
            call.arguments_json
        ));
    }
    text
}

/// Finds fenced tool blocks in streamed text.
///
/// Text is accumulated in `carry` and scanned from there, so a fence split
/// across deltas is still recognized. Outside a block, text is released as
/// soon as it can no longer be the start of a fence.
#[derive(Debug, Default)]
pub struct FenceParser {
    /// Whether the scanner is between an opening and a closing fence.
    inside: bool,
    /// Text received but not yet released.
    carry: String,
    /// How many calls have been emitted, for synthesized ids.
    calls_emitted: u32,
}

impl FenceParser {
    /// Creates an empty parser.
    ///
    /// # Returns
    ///
    /// The parser.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one text delta.
    ///
    /// # Arguments
    ///
    /// * `text` - The delta.
    ///
    /// # Returns
    ///
    /// Text deltas and tool calls that are now certain, in order.
    pub fn push_text(&mut self, text: &str) -> Vec<Event> {
        self.carry.push_str(text);
        let mut out = Vec::new();
        loop {
            let progressed = if self.inside {
                self.scan_inside(&mut out)
            } else {
                self.scan_outside(&mut out)
            };
            if !progressed {
                return out;
            }
        }
    }

    /// Releases whatever is still buffered as text.
    ///
    /// # Returns
    ///
    /// At most one text delta: an unterminated block is returned verbatim,
    /// opening fence included.
    pub fn finish(&mut self) -> Vec<Event> {
        let carry = std::mem::take(&mut self.carry);
        let text = if self.inside {
            format!("{OPEN_FENCE}{carry}")
        } else {
            carry
        };
        self.inside = false;
        if text.is_empty() {
            Vec::new()
        } else {
            vec![Event::TextDelta(text)]
        }
    }

    /// How many tool calls this parser has emitted.
    ///
    /// # Returns
    ///
    /// The count.
    pub fn calls_emitted(&self) -> u32 {
        self.calls_emitted
    }

    /// Looks for an opening fence; returns whether the loop should continue.
    fn scan_outside(&mut self, out: &mut Vec<Event>) -> bool {
        let Some(position) = self.carry.find("```") else {
            let held = held_suffix_len(&self.carry);
            let release = self.carry.len() - held;
            if release > 0 {
                let kept = self.carry.split_off(release);
                out.push(Event::TextDelta(std::mem::replace(&mut self.carry, kept)));
            }
            return false;
        };
        let tail = &self.carry[position..];
        if tail.starts_with(OPEN_FENCE) {
            let rest = self.carry.split_off(position + OPEN_FENCE.len());
            self.carry.truncate(position);
            if !self.carry.is_empty() {
                out.push(Event::TextDelta(std::mem::take(&mut self.carry)));
            }
            self.carry = rest;
            self.inside = true;
            return true;
        }
        if OPEN_FENCE.starts_with(tail) {
            // Could still become an opening fence; wait for more text.
            if position > 0 {
                let kept = self.carry.split_off(position);
                out.push(Event::TextDelta(std::mem::replace(&mut self.carry, kept)));
            }
            return false;
        }
        // Some other fence: release it as text and keep scanning after it.
        let rest = self.carry.split_off(position + 3);
        out.push(Event::TextDelta(std::mem::replace(&mut self.carry, rest)));
        true
    }

    /// Looks for the closing fence; returns whether the loop should continue.
    fn scan_inside(&mut self, out: &mut Vec<Event>) -> bool {
        let Some(position) = self.carry.find(CLOSE_FENCE) else {
            return false;
        };
        let mut rest = self.carry.split_off(position + CLOSE_FENCE.len());
        if let Some(stripped) = rest.strip_prefix('\n') {
            rest = stripped.to_owned();
        }
        self.carry.truncate(position);
        let body = std::mem::replace(&mut self.carry, rest);
        self.inside = false;
        match parse_tool_block(&body) {
            Some((name, arguments_json)) => {
                self.calls_emitted += 1;
                out.push(Event::ToolCall(ToolCall {
                    id: format!("text_call_{}", self.calls_emitted),
                    name,
                    arguments_json,
                }));
            }
            None => out.push(Event::TextDelta(format!(
                "{OPEN_FENCE}{body}{CLOSE_FENCE}\n"
            ))),
        }
        true
    }
}

/// Length of the longest suffix of `text` that could still grow into
/// [`OPEN_FENCE`].
fn held_suffix_len(text: &str) -> usize {
    (1..OPEN_FENCE.len())
        .rev()
        .find(|len| text.ends_with(&OPEN_FENCE[..*len]))
        .unwrap_or(0)
}

/// Parses the body of a tool block into a name and serialized arguments.
fn parse_tool_block(body: &str) -> Option<(String, String)> {
    let value: serde_json::Value = serde_json::from_str(body.trim()).ok()?;
    let name = value.get("name")?.as_str()?.to_owned();
    let arguments = value
        .get("arguments")
        .filter(|arguments| arguments.is_object())
        .cloned()
        .unwrap_or_else(|| serde_json::Value::Object(Default::default()));
    Some((name, arguments.to_string()))
}

/// An event stream whose text deltas are scanned for tool blocks.
pub struct TextToolStream {
    /// The adapter's raw stream.
    inner: EventStream,
    /// The fence scanner.
    parser: FenceParser,
    /// Items ready to yield.
    pending: VecDeque<Result<Event, ProviderError>>,
    /// Whether `inner` has ended.
    finished: bool,
}

impl TextToolStream {
    /// Wraps a raw stream.
    ///
    /// # Arguments
    ///
    /// * `inner` - The stream to scan.
    ///
    /// # Returns
    ///
    /// The wrapper.
    pub fn new(inner: EventStream) -> Self {
        Self {
            inner,
            parser: FenceParser::new(),
            pending: VecDeque::new(),
            finished: false,
        }
    }

    /// Queues parser output.
    fn queue(&mut self, events: Vec<Event>) {
        self.pending.extend(events.into_iter().map(Ok));
    }
}

impl Stream for TextToolStream {
    type Item = Result<Event, ProviderError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            if let Some(item) = self.pending.pop_front() {
                return Poll::Ready(Some(item));
            }
            if self.finished {
                return Poll::Ready(None);
            }
            match self.inner.as_mut().poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => {
                    self.finished = true;
                    let events = self.parser.finish();
                    self.queue(events);
                }
                Poll::Ready(Some(Ok(Event::TextDelta(text)))) => {
                    let events = self.parser.push_text(&text);
                    self.queue(events);
                }
                Poll::Ready(Some(Ok(Event::Done { stop_reason }))) => {
                    let events = self.parser.finish();
                    self.queue(events);
                    let stop_reason =
                        if stop_reason == StopReason::EndTurn && self.parser.calls_emitted() > 0 {
                            StopReason::ToolUse
                        } else {
                            stop_reason
                        };
                    self.pending.push_back(Ok(Event::Done { stop_reason }));
                }
                Poll::Ready(Some(other)) => self.pending.push_back(other),
            }
        }
    }
}

/// A provider wrapper that speaks the fenced text protocol.
pub struct TextTools<P> {
    /// The adapter doing the HTTP work.
    inner: P,
}

impl<P> TextTools<P> {
    /// Wraps an adapter.
    ///
    /// # Arguments
    ///
    /// * `inner` - The adapter.
    ///
    /// # Returns
    ///
    /// The wrapper.
    pub fn new(inner: P) -> Self {
        Self { inner }
    }
}

impl<P: Provider> Provider for TextTools<P> {
    fn complete(&self, request: CompletionRequest) -> EventStream {
        let inner = self.inner.complete(rewrite_request(request));
        Box::pin(TextToolStream::new(inner))
    }
}

#[cfg(test)]
mod tests {
    use futures_util::{StreamExt, stream};

    use super::*;
    use crate::provider::fake::FakeProvider;
    use crate::provider::types::ToolResult;

    fn tools() -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "read_file".to_owned(),
            description: "Reads a file.".to_owned(),
            parameters: serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        }]
    }

    fn block() -> &'static str {
        "```tool\n{\"name\": \"read_file\", \"arguments\": {\"path\": \"a.rs\"}}\n```"
    }

    fn call(n: u32) -> Event {
        Event::ToolCall(ToolCall {
            id: format!("text_call_{n}"),
            name: "read_file".to_owned(),
            arguments_json: "{\"path\":\"a.rs\"}".to_owned(),
        })
    }

    fn text(s: &str) -> Event {
        Event::TextDelta(s.to_owned())
    }

    #[test]
    fn instruction_lists_every_tool_and_fence_format() {
        let instruction = tools_instruction(&tools());
        assert!(instruction.contains("## read_file"));
        assert!(instruction.contains("Reads a file."));
        assert!(instruction.contains("\"path\""));
        assert!(instruction.contains("```tool\n{\"name\": \"<tool name>\""));
    }

    #[test]
    fn rewrite_request_moves_tools_into_system_and_flattens_results() {
        let request = CompletionRequest {
            system: "base".to_owned(),
            messages: vec![
                Message::user("do it"),
                Message::Assistant {
                    text: "ok".to_owned(),
                    reasoning: Vec::new(),
                    tool_calls: vec![ToolCall {
                        id: "x".to_owned(),
                        name: "read_file".to_owned(),
                        arguments_json: "{\"path\":\"a.rs\"}".to_owned(),
                    }],
                },
                Message::ToolResults {
                    results: vec![ToolResult {
                        call_id: "x".to_owned(),
                        content: "contents".to_owned(),
                        is_error: false,
                    }],
                },
            ],
            tools: tools(),
            max_output_tokens: Some(5),
        };
        let rewritten = rewrite_request(request);
        assert!(rewritten.tools.is_empty());
        assert!(rewritten.system.starts_with("base\n\n# Tools"));
        assert_eq!(rewritten.max_output_tokens, Some(5));
        assert_eq!(
            rewritten.messages[1],
            Message::Assistant {
                text: "ok\n```tool\n{\"name\": \"read_file\", \"arguments\": {\"path\":\"a.rs\"}}\n```"
                    .to_owned(),
                reasoning: Vec::new(),
                tool_calls: Vec::new(),
            }
        );
        assert_eq!(
            rewritten.messages[2],
            Message::user("[tool_result id=x error=false]\ncontents")
        );
    }

    #[test]
    fn fence_plain_text_passes_through() {
        let mut parser = FenceParser::new();
        assert_eq!(parser.push_text("hello "), vec![text("hello ")]);
        assert_eq!(parser.push_text("world"), vec![text("world")]);
        assert!(parser.finish().is_empty());
    }

    #[test]
    fn fence_single_delta_tool_call() {
        let mut parser = FenceParser::new();
        assert_eq!(parser.push_text(block()), vec![call(1)]);
        assert_eq!(parser.calls_emitted(), 1);
        assert!(parser.finish().is_empty());
    }

    #[test]
    fn fence_split_across_deltas() {
        let mut parser = FenceParser::new();
        let mut events = Vec::new();
        for piece in [
            "``",
            "`to",
            "ol\n{\"name\":",
            " \"read_file\", \"arguments\": {\"path\": \"a.rs\"}}",
            "\n``",
            "`",
        ] {
            events.extend(parser.push_text(piece));
        }
        assert_eq!(events, vec![call(1)]);
    }

    #[test]
    fn fence_text_before_and_after_block() {
        let mut parser = FenceParser::new();
        let input = format!("before\n{}\nafter", block());
        let mut events = parser.push_text(&input);
        events.extend(parser.finish());
        assert_eq!(events, vec![text("before\n"), call(1), text("after")]);
    }

    #[test]
    fn fence_two_blocks_get_distinct_ids() {
        let mut parser = FenceParser::new();
        let input = format!("{}\n{}", block(), block());
        assert_eq!(parser.push_text(&input), vec![call(1), call(2)]);
    }

    #[test]
    fn fence_other_language_fence_is_text() {
        let mut parser = FenceParser::new();
        let mut events = parser.push_text("see ```rust\nfn x() {}\n```");
        events.extend(parser.finish());
        let joined: String = events
            .iter()
            .map(|event| match event {
                Event::TextDelta(t) => t.as_str(),
                _ => panic!("unexpected {event:?}"),
            })
            .collect();
        assert_eq!(joined, "see ```rust\nfn x() {}\n```");
    }

    #[test]
    fn fence_invalid_json_emits_raw_block() {
        let mut parser = FenceParser::new();
        let events = parser.push_text("```tool\nnot json\n```\n");
        assert_eq!(events, vec![text("```tool\nnot json\n```\n")]);
        assert_eq!(parser.calls_emitted(), 0);
    }

    #[test]
    fn fence_unterminated_block_flushed_on_finish() {
        let mut parser = FenceParser::new();
        assert!(parser.push_text("```tool\n{\"name\": \"x\"").is_empty());
        assert_eq!(parser.finish(), vec![text("```tool\n{\"name\": \"x\"")]);
    }

    #[test]
    fn fence_held_prefix_flushed_on_finish() {
        let mut parser = FenceParser::new();
        assert_eq!(parser.push_text("a``"), vec![text("a")]);
        assert_eq!(parser.finish(), vec![text("``")]);
    }

    #[test]
    fn fence_missing_arguments_default_to_empty_object() {
        let mut parser = FenceParser::new();
        let events = parser.push_text("```tool\n{\"name\": \"list_dir\"}\n```");
        assert_eq!(
            events,
            vec![Event::ToolCall(ToolCall {
                id: "text_call_1".to_owned(),
                name: "list_dir".to_owned(),
                arguments_json: "{}".to_owned(),
            })]
        );
    }

    async fn run_stream(
        items: Vec<Result<Event, ProviderError>>,
    ) -> Vec<Result<Event, ProviderError>> {
        let inner: EventStream = Box::pin(stream::iter(items));
        TextToolStream::new(inner).collect().await
    }

    #[tokio::test]
    async fn stream_rewrites_end_turn_to_tool_use() {
        let items = run_stream(vec![
            Ok(text(block())),
            Ok(Event::Done {
                stop_reason: StopReason::EndTurn,
            }),
        ])
        .await;
        assert_eq!(
            items,
            vec![
                Ok(call(1)),
                Ok(Event::Done {
                    stop_reason: StopReason::ToolUse
                })
            ]
        );
    }

    #[tokio::test]
    async fn stream_keeps_end_turn_without_calls() {
        let items = run_stream(vec![
            Ok(text("just ``")),
            Ok(Event::Done {
                stop_reason: StopReason::EndTurn,
            }),
        ])
        .await;
        assert_eq!(
            items,
            vec![
                Ok(text("just ")),
                Ok(text("``")),
                Ok(Event::Done {
                    stop_reason: StopReason::EndTurn
                })
            ]
        );
    }

    #[tokio::test]
    async fn stream_passes_errors_through() {
        let items = run_stream(vec![
            Ok(text("partial ```tool\n{")),
            Err(ProviderError::Timeout { after_s: 3 }),
        ])
        .await;
        assert_eq!(
            items,
            vec![
                Ok(text("partial ")),
                Err(ProviderError::Timeout { after_s: 3 }),
                Ok(text("```tool\n{")),
            ]
        );
    }

    #[tokio::test]
    async fn text_tools_provider_rewrites_request() {
        let fake = FakeProvider::new(vec![FakeProvider::text_turn("hi")]);
        let provider = TextTools::new(fake);
        let request = CompletionRequest {
            tools: tools(),
            ..CompletionRequest::default()
        };
        let _: Vec<_> = provider.complete(request).collect().await;
        let recorded = provider.inner.requests();
        assert!(recorded[0].tools.is_empty());
        assert!(recorded[0].system.contains("## read_file"));
    }
}
