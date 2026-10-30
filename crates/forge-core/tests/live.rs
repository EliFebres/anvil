//! A manual smoke test against a real endpoint. Ignored by default.
//!
//! Run with:
//!
//! ```text
//! FORGE_LIVE_PROTOCOL=openai FORGE_LIVE_BASE_URL=https://host/v1 \
//! FORGE_LIVE_MODEL=name FORGE_LIVE_KEY_ENV=FORGE_RUNPOD_KEY \
//! cargo test -p forge-core --test live -- --ignored --nocapture
//! ```

use forge_core::provider::{self, CompletionRequest, Event, Message, TokenBudget};
use forge_types::{LimitsConfig, Protocol, ProviderConfig};
use futures_util::StreamExt;

#[tokio::test]
#[ignore = "needs a live endpoint; see the module docs"]
async fn live_endpoint_answers_pong() {
    let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("set {name}"));
    let protocol = match var("FORGE_LIVE_PROTOCOL").as_str() {
        "anthropic" => Protocol::Anthropic,
        _ => Protocol::Openai,
    };
    let cfg = ProviderConfig {
        protocol,
        base_url: var("FORGE_LIVE_BASE_URL"),
        api_key_env: var("FORGE_LIVE_KEY_ENV"),
        model: var("FORGE_LIVE_MODEL"),
        context_window: 32_000,
        supports_tools: std::env::var("FORGE_LIVE_NO_TOOLS").is_err(),
        supports_reasoning: std::env::var("FORGE_LIVE_REASONING").is_ok(),
        max_output_tokens: Some(256),
    };
    let provider = provider::from_config(
        "live",
        &cfg,
        &LimitsConfig::default(),
        TokenBudget::shared(100_000),
    )
    .expect("provider builds");
    let request = CompletionRequest {
        system: "You are terse.".to_owned(),
        messages: vec![Message::user("Reply with the single word: pong")],
        ..CompletionRequest::default()
    };
    let mut events = provider.complete(request);
    let mut text = String::new();
    let mut done = false;
    while let Some(item) = events.next().await {
        let event = item.expect("stream item");
        println!("{event:?}");
        match event {
            Event::TextDelta(delta) => text.push_str(&delta),
            Event::Done { .. } => done = true,
            _ => {}
        }
    }
    assert!(done, "stream must end with Done");
    assert!(text.to_lowercase().contains("pong"), "got: {text}");
}
