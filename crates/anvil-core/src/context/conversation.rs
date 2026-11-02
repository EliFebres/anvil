//! The coder-side conversation, with token accounting and compaction.
//!
//! Context size is measured from the last `Usage` the provider reported,
//! plus an estimate for what was appended since. When it reaches 80% of
//! the context window, the summarizer replaces every turn but the last
//! four with a structured summary.

use std::path::{Path, PathBuf};

use crate::provider::{CompletionRequest, Message, Provider, ProviderError, Usage};

/// Rough characters per token, used for text not yet measured by a provider.
pub const CHARS_PER_TOKEN: u64 = 4;

/// Fraction of the context window at which compaction runs.
pub const COMPACTION_THRESHOLD: f64 = 0.8;

/// Turns (assistant messages and what follows them) kept verbatim.
pub const KEEP_LAST_TURNS: usize = 4;

/// The summarizer's prompt.
pub const SUMMARIZE: &str = include_str!("../../prompts/summarize.md");

/// Characters of one tool result kept in the transcript sent to the summarizer.
const TRANSCRIPT_RESULT_CHARS: usize = 1500;

/// Characters of one tool call's arguments kept in the transcript.
const TRANSCRIPT_ARGS_CHARS: usize = 500;

/// What a provider told us about the prompt size at some point.
#[derive(Debug, Clone, Copy)]
struct Measurement {
    /// How many messages the measured request contained.
    at_len: usize,
    /// Prompt tokens of that request (input plus cached).
    prompt_tokens: u64,
}

/// What one compaction did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compaction {
    /// Messages replaced by the summary.
    pub dropped_messages: usize,
    /// The summary text.
    pub summary: String,
    /// Where the summary was written.
    pub summary_path: PathBuf,
    /// Tokens the summarizer used.
    pub usage: Option<Usage>,
}

/// The messages sent to the coder, with the bookkeeping to compact them.
#[derive(Debug, Default)]
pub struct Conversation {
    messages: Vec<Message>,
    measured: Option<Measurement>,
    compactions: u32,
}

impl Conversation {
    /// An empty conversation.
    ///
    /// # Returns
    ///
    /// The conversation.
    pub fn new() -> Self {
        Self::default()
    }

    /// A conversation restored from a session log.
    ///
    /// # Arguments
    ///
    /// * `messages` - The messages, oldest first.
    ///
    /// # Returns
    ///
    /// The conversation.
    pub fn from_messages(messages: Vec<Message>) -> Self {
        Self {
            messages,
            ..Self::default()
        }
    }

    /// The messages, oldest first.
    ///
    /// # Returns
    ///
    /// A slice.
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Whether there are no messages.
    ///
    /// # Returns
    ///
    /// `true` when empty.
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// Appends a message.
    ///
    /// # Arguments
    ///
    /// * `message` - The message.
    pub fn push(&mut self, message: Message) {
        self.messages.push(message);
    }

    /// Drops every message past `len`, as when a turn is cancelled and its
    /// partial messages must not reach the next request. A measurement
    /// that counted dropped messages is forgotten with them.
    ///
    /// # Arguments
    ///
    /// * `len` - How many messages to keep; more than there are keeps all.
    pub fn truncate(&mut self, len: usize) {
        self.messages.truncate(len);
        if self
            .measured
            .is_some_and(|measurement| measurement.at_len > len)
        {
            self.measured = None;
        }
    }

    /// Records what the provider measured for a request.
    ///
    /// # Arguments
    ///
    /// * `request_len` - How many messages the request contained.
    /// * `usage` - The usage the provider reported for it.
    pub fn record_usage(&mut self, request_len: usize, usage: Usage) {
        self.measured = Some(Measurement {
            at_len: request_len.min(self.messages.len()),
            prompt_tokens: usage.input.saturating_add(usage.cached),
        });
    }

    /// Estimated size of the next request.
    ///
    /// # Arguments
    ///
    /// * `system` - The system prompt that will be sent.
    ///
    /// # Returns
    ///
    /// Tokens: the last measurement plus an estimate of what was appended
    /// since, or a pure estimate when nothing was measured yet.
    pub fn estimated_tokens(&self, system: &str) -> u64 {
        match self.measured {
            Some(measurement) => measurement
                .prompt_tokens
                .saturating_add(estimate_messages(&self.messages[measurement.at_len..])),
            None => estimate_tokens(system).saturating_add(estimate_messages(&self.messages)),
        }
    }

    /// Whether the next request would cross the compaction threshold.
    ///
    /// # Arguments
    ///
    /// * `system` - The system prompt that will be sent.
    /// * `context_window` - The provider's context window in tokens.
    ///
    /// # Returns
    ///
    /// `true` at or above 80% of the window.
    pub fn needs_compaction(&self, system: &str, context_window: u32) -> bool {
        let threshold = (f64::from(context_window) * COMPACTION_THRESHOLD) as u64;
        self.estimated_tokens(system) >= threshold
    }

    /// How many compactions have run.
    ///
    /// # Returns
    ///
    /// The count.
    pub fn compactions(&self) -> u32 {
        self.compactions
    }

    /// Index where the last [`KEEP_LAST_TURNS`] turns begin.
    ///
    /// # Returns
    ///
    /// The index, or `None` when there are not enough turns to drop any.
    pub fn split_point(&self) -> Option<usize> {
        let assistant_indices: Vec<usize> = self
            .messages
            .iter()
            .enumerate()
            .filter(|(_, message)| matches!(message, Message::Assistant { .. }))
            .map(|(index, _)| index)
            .collect();
        if assistant_indices.len() <= KEEP_LAST_TURNS {
            return None;
        }
        let start = assistant_indices[assistant_indices.len() - KEEP_LAST_TURNS];
        (start > 0).then_some(start)
    }

    /// Replaces every turn but the last four with a summary.
    ///
    /// # Arguments
    ///
    /// * `summarizer` - The model that writes the summary.
    /// * `task` - The user's task, given to the summarizer.
    /// * `session_dir` - Where `compaction-<n>.md` is written.
    /// * `max_transcript_chars` - Cap on the transcript sent to the summarizer.
    ///
    /// # Returns
    ///
    /// What was done, or `None` when there were too few turns to compact.
    ///
    /// # Errors
    ///
    /// The summarizer's error; the conversation is left unchanged.
    pub async fn compact(
        &mut self,
        summarizer: &dyn Provider,
        task: &str,
        session_dir: &Path,
        max_transcript_chars: usize,
    ) -> Result<Option<Compaction>, ProviderError> {
        let Some(split) = self.split_point() else {
            return Ok(None);
        };
        let transcript = render_transcript(&self.messages[..split], max_transcript_chars);
        let request = CompletionRequest {
            system: SUMMARIZE.to_owned(),
            messages: vec![Message::user(format!(
                "# Task\n\n{task}\n\n# Transcript to summarize\n\n{transcript}"
            ))],
            tools: Vec::new(),
            max_output_tokens: None,
        };
        let (summary, usage) = collect_text(summarizer, request).await?;
        self.compactions += 1;
        let summary_path = session_dir.join(format!("compaction-{}.md", self.compactions));
        if let Err(error) = std::fs::write(&summary_path, &summary) {
            tracing::warn!(%error, path = %summary_path.display(), "cannot write the compaction summary");
        }
        let kept = self.messages.split_off(split);
        let dropped_messages = self.messages.len();
        self.messages = Vec::with_capacity(kept.len() + 1);
        self.messages.push(Message::user(format!(
            "Summary of the conversation so far (older turns were compacted):\n\n{summary}"
        )));
        self.messages.extend(kept);
        self.measured = None;
        Ok(Some(Compaction {
            dropped_messages,
            summary,
            summary_path,
            usage,
        }))
    }
}

/// Runs a completion without tools and returns its text.
///
/// # Arguments
///
/// * `provider` - The model.
/// * `request` - The request.
///
/// # Returns
///
/// The concatenated text deltas and the usage, if reported.
///
/// # Errors
///
/// The provider's error, or a malformed-stream error when the stream ends
/// without a `Done` event.
pub async fn collect_text(
    provider: &dyn Provider,
    request: CompletionRequest,
) -> Result<(String, Option<Usage>), ProviderError> {
    use futures_util::StreamExt;

    use crate::provider::Event;

    let mut stream = provider.complete(request);
    let mut text = String::new();
    let mut usage = None;
    while let Some(item) = stream.next().await {
        match item? {
            Event::TextDelta(delta) => text.push_str(&delta),
            Event::Usage(reported) => usage = Some(reported),
            Event::Done { .. } => return Ok((text, usage)),
            _ => {}
        }
    }
    Err(ProviderError::malformed(
        "the stream ended without a Done event",
    ))
}

/// Rough token count of a text.
///
/// # Arguments
///
/// * `text` - The text.
///
/// # Returns
///
/// `len / 4`, rounded up.
pub fn estimate_tokens(text: &str) -> u64 {
    (text.len() as u64).div_ceil(CHARS_PER_TOKEN)
}

/// Rough token count of messages, including a small per-message overhead.
fn estimate_messages(messages: &[Message]) -> u64 {
    messages
        .iter()
        .map(|message| match message {
            Message::User { text } => estimate_tokens(text) + 4,
            Message::Assistant {
                text,
                reasoning,
                tool_calls,
            } => {
                estimate_tokens(text)
                    + reasoning
                        .iter()
                        .map(|r| estimate_tokens(&r.text))
                        .sum::<u64>()
                    + tool_calls
                        .iter()
                        .map(|c| estimate_tokens(&c.arguments_json) + 8)
                        .sum::<u64>()
                    + 4
            }
            Message::ToolResults { results } => results
                .iter()
                .map(|r| estimate_tokens(&r.content) + 8)
                .sum::<u64>(),
        })
        .sum()
}

/// Renders messages as text for a summarizer, oldest first, dropping the
/// oldest messages when the cap is exceeded.
///
/// # Arguments
///
/// * `messages` - The conversation.
/// * `max_chars` - Cap on the text; a leading note counts the omitted messages.
///
/// # Returns
///
/// The transcript.
pub fn render_transcript(messages: &[Message], max_chars: usize) -> String {
    let mut parts: Vec<String> = messages
        .iter()
        .map(|message| match message {
            Message::User { text } => format!("## User\n{text}"),
            Message::Assistant {
                text, tool_calls, ..
            } => {
                let mut part = format!("## Assistant\n{text}");
                for call in tool_calls {
                    part.push_str(&format!(
                        "\n[tool call {}: {}]",
                        call.name,
                        cut(&call.arguments_json, TRANSCRIPT_ARGS_CHARS)
                    ));
                }
                part
            }
            Message::ToolResults { results } => results
                .iter()
                .map(|r| {
                    format!(
                        "[tool result {}{}]\n{}",
                        r.call_id,
                        if r.is_error { " (error)" } else { "" },
                        cut(&r.content, TRANSCRIPT_RESULT_CHARS)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
        })
        .collect();
    let mut total: usize = parts.iter().map(|p| p.len() + 2).sum();
    let mut omitted = 0;
    while total > max_chars && parts.len() > 1 {
        total -= parts.remove(0).len() + 2;
        omitted += 1;
    }
    let mut text = parts.join("\n\n");
    if omitted > 0 {
        text = format!("[{omitted} earlier messages omitted]\n\n{text}");
    }
    text
}

/// Cuts text to `max` characters with an ellipsis.
fn cut(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let kept: String = text.chars().take(max).collect();
    format!("{kept}...")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{FakeProvider, ToolCall, ToolResult};

    fn assistant(text: &str) -> Message {
        Message::Assistant {
            text: text.to_owned(),
            reasoning: Vec::new(),
            tool_calls: Vec::new(),
        }
    }

    fn tool_turn(id: &str) -> [Message; 2] {
        [
            Message::Assistant {
                text: String::new(),
                reasoning: Vec::new(),
                tool_calls: vec![ToolCall {
                    id: id.to_owned(),
                    name: "list_dir".to_owned(),
                    arguments_json: "{}".to_owned(),
                }],
            },
            Message::ToolResults {
                results: vec![ToolResult {
                    call_id: id.to_owned(),
                    content: "a.txt\nb.txt".to_owned(),
                    is_error: false,
                }],
            },
        ]
    }

    /// Task, then six tool turns, then a text turn: seven assistant messages.
    fn seven_turns() -> Conversation {
        let mut conversation = Conversation::new();
        conversation.push(Message::user("do the thing"));
        for i in 0..6 {
            for message in tool_turn(&format!("c{i}")) {
                conversation.push(message);
            }
        }
        conversation.push(assistant("done"));
        conversation
    }

    #[test]
    fn truncate_drops_the_tail_and_a_measurement_past_it() {
        let mut conversation = seven_turns();
        let usage = Usage {
            input: 900,
            output: 5,
            cached: 0,
        };
        conversation.record_usage(13, usage);
        conversation.truncate(13);
        assert_eq!(conversation.messages().len(), 13);
        assert_eq!(conversation.estimated_tokens(""), 900);
        conversation.truncate(100);
        assert_eq!(conversation.messages().len(), 13);
        conversation.truncate(3);
        assert_eq!(conversation.messages().len(), 3);
        assert!(
            conversation.estimated_tokens("") < 900,
            "measurement forgotten"
        );
        conversation.truncate(0);
        assert!(conversation.is_empty());
    }

    #[test]
    fn estimates_then_trusts_measurements() {
        let mut conversation = Conversation::new();
        conversation.push(Message::user("x".repeat(400)));
        assert_eq!(
            conversation.estimated_tokens("s".repeat(40).as_str()),
            10 + 100 + 4
        );
        conversation.record_usage(
            1,
            Usage {
                input: 900,
                output: 5,
                cached: 100,
            },
        );
        assert_eq!(conversation.estimated_tokens(""), 1000);
        conversation.push(assistant(&"y".repeat(80)));
        assert_eq!(conversation.estimated_tokens(""), 1000 + 20 + 4);
        assert!(conversation.needs_compaction("", 1280));
        assert!(!conversation.needs_compaction("", 1290));
    }

    #[test]
    fn split_point_keeps_the_last_four_turns() {
        let conversation = seven_turns();
        // Messages: user, then pairs (assistant, results) at 1..=12, then assistant at 13.
        // Assistant indices: 1,3,5,7,9,11,13; keep the last four from index 7.
        assert_eq!(conversation.split_point(), Some(7));
        let mut short = Conversation::new();
        short.push(Message::user("t"));
        for _ in 0..4 {
            short.push(assistant("a"));
        }
        assert_eq!(short.split_point(), None);
    }

    #[tokio::test]
    async fn compaction_replaces_old_turns_with_a_summary() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut conversation = seven_turns();
        let summarizer = FakeProvider::new(vec![FakeProvider::text_turn(
            "## Task\nthing\n## Decisions\nnone\n## Files touched\n- a.txt\n## Open items\n- finish",
        )]);
        let compaction = conversation
            .compact(&summarizer, "do the thing", dir.path(), 100_000)
            .await
            .expect("summarizer ok")
            .expect("compacted");
        assert_eq!(compaction.dropped_messages, 7);
        assert!(compaction.summary.contains("## Open items"));
        assert_eq!(compaction.summary_path, dir.path().join("compaction-1.md"));
        assert!(compaction.summary_path.exists());
        assert_eq!(conversation.compactions(), 1);

        let messages = conversation.messages();
        assert_eq!(messages.len(), 1 + 7);
        assert!(
            matches!(&messages[0], Message::User { text } if text.contains("compacted") && text.contains("## Decisions"))
        );
        assert!(
            matches!(&messages[1], Message::Assistant { tool_calls, .. } if tool_calls[0].id == "c3")
        );
        assert!(matches!(messages.last(), Some(Message::Assistant { text, .. }) if text == "done"));
        let request = &summarizer.requests()[0];
        assert!(request.system.contains("## Files touched"));
        assert!(
            matches!(&request.messages[0], Message::User { text } if text.contains("do the thing") && text.contains("[tool call list_dir") && text.contains("c2") && !text.contains("c3"))
        );

        assert_eq!(
            conversation
                .compact(&summarizer, "t", dir.path(), 100_000)
                .await
                .expect("ok"),
            None,
            "four turns left: nothing to drop"
        );
    }

    #[tokio::test]
    async fn summarizer_failure_leaves_the_conversation_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut conversation = seven_turns();
        let summarizer = FakeProvider::new(vec![vec![Err(ProviderError::Timeout { after_s: 1 })]]);
        let result = conversation
            .compact(&summarizer, "t", dir.path(), 100_000)
            .await;
        assert_eq!(result, Err(ProviderError::Timeout { after_s: 1 }));
        assert_eq!(conversation.messages().len(), 14);
        assert_eq!(conversation.compactions(), 0);
    }

    #[test]
    fn transcript_is_capped_from_the_front() {
        let messages: Vec<Message> = (0..5)
            .map(|i| Message::user(format!("m{i} {}", "x".repeat(50))))
            .collect();
        let text = render_transcript(&messages, 200);
        assert!(text.starts_with("[2 earlier messages omitted]"), "{text}");
        assert!(text.contains("m4") && !text.contains("m1"));
        let full = render_transcript(&messages, 10_000);
        assert!(full.contains("m0"));
        assert_eq!(cut("abcdef", 3), "abc...");
    }
}
