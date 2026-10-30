//! The streaming pipeline shared by the HTTP adapters.
//!
//! [`start`] spawns one task per completion. The task retries the request
//! phase, decodes the SSE body through a protocol-specific [`Decoder`], and
//! forwards events over a bounded channel. A single deadline covers backoff,
//! the request and the whole stream. [`send_attempt`] is the only function
//! that touches reqwest, so everything else is testable with scripted bytes.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use futures_util::{Stream, StreamExt};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use super::EventStream;
use super::error::ProviderError;
use super::retry::{Attempt, RetryPolicy, is_retryable_status, parse_retry_after, retry};
use super::sse::{SseFrame, SseParser};
use super::types::Event;

/// Raw response bytes, chunked however the network delivered them.
pub(crate) type ByteStream = Pin<Box<dyn Stream<Item = Result<Vec<u8>, ProviderError>> + Send>>;

/// Items a decoder produces for the channel.
pub(crate) type Items = Vec<Result<Event, ProviderError>>;

/// How many bytes of an error body are kept in the error message.
const MAX_ERROR_BODY_BYTES: usize = 2048;

/// How many events may queue before the HTTP read stalls.
const CHANNEL_CAPACITY: usize = 64;

/// Turns SSE frames into events for one wire protocol.
pub(crate) trait Decoder: Send + 'static {
    /// Handles one frame.
    fn on_frame(&mut self, frame: SseFrame) -> Items;

    /// Handles end of input when no terminal item was produced yet.
    fn on_end(&mut self) -> Items;
}

/// Settings for one adapter's pipeline.
#[derive(Debug, Clone)]
pub(crate) struct StreamConfig {
    /// Provider name, for tracing only.
    pub name: String,
    /// Deadline for the whole completion.
    pub timeout: Duration,
    /// Retry policy for the request phase.
    pub policy: RetryPolicy,
}

/// Starts a completion and returns its event stream.
///
/// `attempt` is called with a zero-based attempt index each time the request
/// is (re)sent. Retries stop once the first byte is decoded; a later error is
/// forwarded as the stream's terminal item. Dropping the stream ends the task.
///
/// # Arguments
///
/// * `cfg` - Deadline and retry policy.
/// * `attempt` - Sends the request and classifies the outcome.
/// * `decoder` - Protocol-specific frame decoder.
///
/// # Returns
///
/// The event stream.
///
/// # Panics
///
/// When called outside a tokio runtime.
pub(crate) fn start<D, F, Fut>(cfg: StreamConfig, mut attempt: F, mut decoder: D) -> EventStream
where
    D: Decoder,
    F: FnMut(u32) -> Fut + Send + 'static,
    Fut: Future<Output = Attempt<ByteStream>> + Send,
{
    let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
    let after_s = cfg.timeout.as_secs();
    tokio::spawn(async move {
        let span = tracing::info_span!("provider.complete", provider = %cfg.name);
        let _entered = span.enter();
        let outcome = tokio::time::timeout(
            cfg.timeout,
            run(&cfg.policy, &mut attempt, &mut decoder, &tx),
        )
        .await;
        match outcome {
            Err(_elapsed) => {
                tracing::warn!(after_s, "provider request timed out");
                let _ = tx.send(Err(ProviderError::Timeout { after_s })).await;
            }
            Ok(Err(error)) => {
                tracing::warn!(%error, "provider request failed");
                let _ = tx.send(Err(error)).await;
            }
            Ok(Ok(())) => {}
        }
    });
    Box::pin(ReceiverStream::new(rx))
}

/// Whether the task should keep reading after forwarding a batch of items.
#[derive(Debug, PartialEq, Eq)]
enum Flow {
    Continue,
    Stop,
}

/// Requests, decodes and forwards until a terminal item or end of input.
async fn run<D, F, Fut>(
    policy: &RetryPolicy,
    attempt: &mut F,
    decoder: &mut D,
    tx: &mpsc::Sender<Result<Event, ProviderError>>,
) -> Result<(), ProviderError>
where
    D: Decoder,
    F: FnMut(u32) -> Fut,
    Fut: Future<Output = Attempt<ByteStream>>,
{
    let mut bytes = retry(policy, attempt).await?;
    let mut parser = SseParser::default();
    while let Some(chunk) = bytes.next().await {
        let chunk = chunk?;
        for frame in parser.push(&chunk) {
            if forward(tx, decoder.on_frame(frame)).await == Flow::Stop {
                return Ok(());
            }
        }
    }
    if let Some(frame) = parser.finish()
        && forward(tx, decoder.on_frame(frame)).await == Flow::Stop
    {
        return Ok(());
    }
    forward(tx, decoder.on_end()).await;
    Ok(())
}

/// Sends items in order; stops at the first terminal item or a closed channel.
async fn forward(tx: &mpsc::Sender<Result<Event, ProviderError>>, items: Items) -> Flow {
    for item in items {
        let terminal = match &item {
            Ok(event) => event.is_terminal(),
            Err(_) => true,
        };
        if tx.send(item).await.is_err() || terminal {
            return Flow::Stop;
        }
    }
    Flow::Continue
}

/// Sends one HTTP request and classifies the outcome for [`retry`].
///
/// # Arguments
///
/// * `builder` - The fully prepared request.
///
/// # Returns
///
/// The body byte stream on 2xx; `Retry` on connection failures, 408, 429 and
/// 5xx; `Fatal` on every other status or an unbuildable request.
pub(crate) async fn send_attempt(builder: reqwest::RequestBuilder) -> Attempt<ByteStream> {
    let response = match builder.send().await {
        Ok(response) => response,
        Err(error) if error.is_builder() => {
            return Attempt::Fatal(ProviderError::transport(error.to_string()));
        }
        Err(error) => {
            return Attempt::Retry {
                error: ProviderError::transport(error.to_string()),
                retry_after: None,
            };
        }
    };
    let status = response.status().as_u16();
    if response.status().is_success() {
        let stream = response.bytes_stream().map(|chunk| {
            chunk
                .map(|bytes| bytes.to_vec())
                .map_err(|error| ProviderError::transport(error.to_string()))
        });
        return Attempt::Ok(Box::pin(stream));
    }
    let retry_after = parse_retry_after(
        response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok()),
    );
    let body = response.text().await.unwrap_or_default();
    let error = ProviderError::Http {
        status,
        body: truncate(body),
    };
    if is_retryable_status(status) {
        Attempt::Retry { error, retry_after }
    } else {
        Attempt::Fatal(error)
    }
}

/// Keeps the first [`MAX_ERROR_BODY_BYTES`] of an error body.
fn truncate(mut body: String) -> String {
    if body.len() > MAX_ERROR_BODY_BYTES {
        let mut cut = MAX_ERROR_BODY_BYTES;
        while !body.is_char_boundary(cut) {
            cut -= 1;
        }
        body.truncate(cut);
        body.push_str("...");
    }
    body
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    use futures_util::stream;

    use super::*;
    use crate::provider::types::StopReason;

    /// Treats every `data:` payload as text and `[DONE]` as the end.
    #[derive(Default)]
    struct TextDecoder {
        ended: bool,
    }

    impl Decoder for TextDecoder {
        fn on_frame(&mut self, frame: SseFrame) -> Items {
            if frame.data == "[DONE]" {
                return vec![Ok(Event::Done {
                    stop_reason: StopReason::EndTurn,
                })];
            }
            vec![Ok(Event::TextDelta(frame.data))]
        }

        fn on_end(&mut self) -> Items {
            self.ended = true;
            vec![Err(ProviderError::malformed("ended early"))]
        }
    }

    fn config(timeout_s: u64, max_retries: u32) -> StreamConfig {
        StreamConfig {
            name: "test".to_owned(),
            timeout: Duration::from_secs(timeout_s),
            policy: RetryPolicy {
                max_retries,
                base_delay: Duration::from_millis(500),
                max_delay: Duration::from_secs(30),
            },
        }
    }

    fn bytes(chunks: Vec<&'static str>) -> ByteStream {
        Box::pin(stream::iter(
            chunks
                .into_iter()
                .map(|chunk| Ok(chunk.as_bytes().to_vec())),
        ))
    }

    async fn collect(mut events: EventStream) -> Vec<Result<Event, ProviderError>> {
        let mut out = Vec::new();
        while let Some(item) = events.next().await {
            out.push(item);
        }
        out
    }

    #[tokio::test]
    async fn pipeline_decodes_split_frames_and_stops_after_terminal() {
        let events = start(
            config(5, 0),
            |_| async {
                Attempt::Ok(bytes(vec![
                    "data: hel",
                    "lo\n\ndata: wor",
                    "ld\n\ndata: [DONE]\n\ndata: after\n\n",
                ]))
            },
            TextDecoder::default(),
        );
        assert_eq!(
            collect(events).await,
            vec![
                Ok(Event::TextDelta("hello".to_owned())),
                Ok(Event::TextDelta("world".to_owned())),
                Ok(Event::Done {
                    stop_reason: StopReason::EndTurn
                }),
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn pipeline_retries_then_streams() {
        let attempts = Arc::new(AtomicU32::new(0));
        let seen = Arc::clone(&attempts);
        let events = start(
            config(60, 3),
            move |index| {
                seen.fetch_add(1, Ordering::SeqCst);
                async move {
                    if index == 0 {
                        Attempt::Retry {
                            error: ProviderError::Http {
                                status: 429,
                                body: String::new(),
                            },
                            retry_after: None,
                        }
                    } else {
                        Attempt::Ok(bytes(vec!["data: ok\n\ndata: [DONE]\n\n"]))
                    }
                }
            },
            TextDecoder::default(),
        );
        let items = collect(events).await;
        assert_eq!(items[0], Ok(Event::TextDelta("ok".to_owned())));
        assert_eq!(items.len(), 2);
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn pipeline_timeout_yields_timeout_error() {
        let events = start(
            config(1, 0),
            |_| async { Attempt::Ok(Box::pin(stream::pending()) as ByteStream) },
            TextDecoder::default(),
        );
        assert_eq!(
            collect(events).await,
            vec![Err(ProviderError::Timeout { after_s: 1 })]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn pipeline_timeout_covers_backoff() {
        let events = start(
            config(5, 10),
            |_| async {
                Attempt::Retry {
                    error: ProviderError::Http {
                        status: 503,
                        body: String::new(),
                    },
                    retry_after: Some(Duration::from_secs(60)),
                }
            },
            TextDecoder::default(),
        );
        assert_eq!(
            collect(events).await,
            vec![Err(ProviderError::Timeout { after_s: 5 })]
        );
    }

    #[tokio::test]
    async fn pipeline_mid_stream_error_not_retried() {
        let attempts = Arc::new(AtomicU32::new(0));
        let seen = Arc::clone(&attempts);
        let events = start(
            config(5, 3),
            move |_| {
                seen.fetch_add(1, Ordering::SeqCst);
                async {
                    let chunks: Vec<Result<Vec<u8>, ProviderError>> = vec![
                        Ok(b"data: first\n\n".to_vec()),
                        Err(ProviderError::transport("reset")),
                    ];
                    Attempt::Ok(Box::pin(stream::iter(chunks)) as ByteStream)
                }
            },
            TextDecoder::default(),
        );
        assert_eq!(
            collect(events).await,
            vec![
                Ok(Event::TextDelta("first".to_owned())),
                Err(ProviderError::transport("reset")),
            ]
        );
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn pipeline_drop_receiver_stops_task() {
        /// Flags when the byte stream (and so the task) is dropped.
        struct Guard(Arc<AtomicBool>);
        impl Drop for Guard {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let dropped = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&dropped);
        let mut events = start(
            config(60, 0),
            move |_| {
                let guard = Guard(Arc::clone(&flag));
                async move {
                    let endless = stream::unfold(guard, |guard| async move {
                        Some((Ok(b"data: x\n\n".to_vec()), guard))
                    });
                    Attempt::Ok(Box::pin(endless) as ByteStream)
                }
            },
            TextDecoder::default(),
        );
        assert_eq!(
            events.next().await,
            Some(Ok(Event::TextDelta("x".to_owned())))
        );
        drop(events);
        for _ in 0..1000 {
            if dropped.load(Ordering::SeqCst) {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("task kept running after the receiver was dropped");
    }

    #[tokio::test]
    async fn pipeline_on_end_called_without_terminal() {
        let events = start(
            config(5, 0),
            |_| async { Attempt::Ok(bytes(vec!["data: only\n\n"])) },
            TextDecoder::default(),
        );
        assert_eq!(
            collect(events).await,
            vec![
                Ok(Event::TextDelta("only".to_owned())),
                Err(ProviderError::malformed("ended early")),
            ]
        );
    }

    #[tokio::test]
    async fn pipeline_fatal_attempt_is_forwarded() {
        let events = start(
            config(5, 3),
            |_| async {
                Attempt::Fatal(ProviderError::Http {
                    status: 401,
                    body: "bad key".to_owned(),
                })
            },
            TextDecoder::default(),
        );
        assert_eq!(
            collect(events).await,
            vec![Err(ProviderError::Http {
                status: 401,
                body: "bad key".to_owned()
            })]
        );
    }

    #[test]
    fn truncate_keeps_char_boundaries() {
        let body = "é".repeat(MAX_ERROR_BODY_BYTES);
        let cut = truncate(body);
        assert!(cut.ends_with("..."));
        assert!(cut.len() <= MAX_ERROR_BODY_BYTES + 3);
        assert_eq!(truncate("short".to_owned()), "short");
    }
}
