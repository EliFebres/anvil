//! Model providers: one trait, two wire protocols, and the wrappers that make
//! them retry, time out, respect a budget and work without native tool calls.
//!
//! [`from_config`] builds the whole stack for one `[providers.<name>]` entry.
//! Tests use [`FakeProvider`] and never touch the network.

pub mod anthropic;
pub mod budget;
pub mod error;
pub mod fake;
mod http;
pub mod models;
pub mod openai;
pub mod retry;
pub mod sse;
pub mod text_tools;
pub mod types;

use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anvil_types::{LimitsConfig, Protocol, ProviderConfig};
use futures_util::Stream;

pub use crate::net::Egress;

pub use anthropic::AnthropicProvider;
pub use budget::{Budgeted, SharedBudget, TokenBudget};
pub use error::ProviderError;
pub use fake::{FakeProvider, Script};
pub use openai::OpenAiProvider;
pub use text_tools::TextTools;
pub use types::{
    CompletionRequest, Event, Message, ReasoningBlock, StopReason, ToolCall, ToolResult, ToolSpec,
    Usage,
};

/// The stream a completion produces.
///
/// Yields zero or more non-terminal events, then exactly one terminal item,
/// either `Ok(Event::Done)` or `Err`, then ends.
pub type EventStream = Pin<Box<dyn Stream<Item = Result<Event, ProviderError>> + Send + 'static>>;

/// Something that turns a request into a stream of events.
///
/// The method is synchronous so the trait is usable as `dyn Provider`; the
/// work happens inside the returned stream.
pub trait Provider: Send + Sync {
    /// Starts one completion.
    ///
    /// # Arguments
    ///
    /// * `request` - The prompt, history and tools.
    ///
    /// # Returns
    ///
    /// The event stream; see [`EventStream`] for its contract.
    fn complete(&self, request: CompletionRequest) -> EventStream;
}

impl<P: Provider + ?Sized> Provider for Box<P> {
    fn complete(&self, request: CompletionRequest) -> EventStream {
        (**self).complete(request)
    }
}

impl<P: Provider + ?Sized> Provider for Arc<P> {
    fn complete(&self, request: CompletionRequest) -> EventStream {
        (**self).complete(request)
    }
}

/// How long to wait for a TCP connection before giving up on an attempt.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Builds the provider stack for one config entry.
///
/// The result is the protocol adapter, wrapped in [`TextTools`] when the
/// model has no native tool calling, wrapped in [`Budgeted`].
///
/// # Arguments
///
/// * `name` - The entry's key under `[providers]`, used in logs.
/// * `cfg` - The entry.
/// * `limits` - Retry, timeout and budget settings.
/// * `budget` - The session budget shared by every provider.
/// * `egress` - Where requests may go and how connections are made. The
///   entry's `base_url` is checked here, before the key is read, and every
///   request is checked again when it is sent.
///
/// # Returns
///
/// The provider.
///
/// # Errors
///
/// [`ProviderError::Refused`] when `egress` does not allow the endpoint,
/// [`ProviderError::MissingApiKey`] when the configured environment variable
/// is unset or empty, or [`ProviderError::Transport`] when the HTTP client
/// cannot be built (such as an unreadable `[network] ca_bundle`).
pub fn from_config(
    name: &str,
    cfg: &ProviderConfig,
    limits: &LimitsConfig,
    budget: SharedBudget,
    egress: &Egress,
) -> Result<Box<dyn Provider>, ProviderError> {
    // A fake provider replays its script, sends nothing and has no key.
    let api_key = match cfg.protocol {
        Protocol::Fake => String::new(),
        Protocol::Openai | Protocol::Anthropic => {
            egress
                .check(&cfg.base_url)
                .map_err(|error| ProviderError::Refused {
                    detail: format!("provider `{name}`: {error}"),
                })?;
            read_api_key(&cfg.api_key_env)?
        }
    };
    build(name, cfg, api_key, limits, budget, egress)
}

/// Reads the API key from the environment.
fn read_api_key(env: &str) -> Result<String, ProviderError> {
    match std::env::var(env) {
        Ok(value) if !value.trim().is_empty() => Ok(value),
        _ => Err(ProviderError::MissingApiKey {
            env: env.to_owned(),
        }),
    }
}

/// Builds the provider stack with an explicit API key.
pub(crate) fn build(
    name: &str,
    cfg: &ProviderConfig,
    api_key: String,
    limits: &LimitsConfig,
    budget: SharedBudget,
    egress: &Egress,
) -> Result<Box<dyn Provider>, ProviderError> {
    let client = crate::net::http_client(egress, CONNECT_TIMEOUT)
        .map_err(|error| ProviderError::transport(error.to_string()))?;
    let adapter: Box<dyn Provider> = match cfg.protocol {
        Protocol::Openai => Box::new(OpenAiProvider::new(name, cfg, api_key, limits, client)),
        Protocol::Anthropic => Box::new(AnthropicProvider::new(name, cfg, api_key, limits, client)),
        Protocol::Fake => Box::new(fake::FakeProvider::from_script_file(std::path::Path::new(
            cfg.script.as_deref().unwrap_or_default(),
        ))?),
    };
    let with_tools: Box<dyn Provider> = if cfg.supports_tools {
        adapter
    } else {
        Box::new(TextTools::new(adapter))
    };
    Ok(Box::new(Budgeted::new(with_tools, budget)))
}

/// Locks a mutex, recovering the data if another holder panicked.
pub(crate) fn lock_or_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(protocol: Protocol, supports_tools: bool) -> ProviderConfig {
        ProviderConfig {
            protocol,
            base_url: "http://localhost:8000/v1".to_owned(),
            api_key_env: "ANVIL_TEST_KEY_THAT_IS_NEVER_SET".to_owned(),
            model: "m".to_owned(),
            context_window: 1000,
            supports_tools,
            supports_reasoning: false,
            max_output_tokens: None,
            price_per_mtoken: None,
            script: None,
        }
    }

    #[test]
    fn from_config_missing_env_is_missing_api_key() {
        let result = from_config(
            "test",
            &config(Protocol::Openai, true),
            &LimitsConfig::default(),
            TokenBudget::shared(10),
            &Egress::personal(["http://localhost:8000/v1"], Vec::new()),
        );
        assert!(matches!(
            result,
            Err(ProviderError::MissingApiKey { env }) if env == "ANVIL_TEST_KEY_THAT_IS_NEVER_SET"
        ));
    }

    #[test]
    fn build_constructs_each_protocol() {
        for protocol in [Protocol::Openai, Protocol::Anthropic] {
            for supports_tools in [true, false] {
                let provider = build(
                    "test",
                    &config(protocol, supports_tools),
                    "key".to_owned(),
                    &LimitsConfig::default(),
                    TokenBudget::shared(10),
                    &Egress::personal(["http://localhost:8000/v1"], Vec::new()),
                );
                assert!(provider.is_ok(), "{protocol:?} tools={supports_tools}");
            }
        }
    }

    #[tokio::test]
    async fn a_provider_redirected_elsewhere_sends_its_key_nowhere_else() {
        use futures_util::StreamExt;
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let home = MockServer::start().await;
        let other = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("location", format!("{}/v1/chat/completions", other.uri())),
            )
            .mount(&home)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&other)
            .await;
        let mut cfg = config(Protocol::Openai, true);
        cfg.base_url = format!("{}/v1", home.uri());
        let provider = build(
            "test",
            &cfg,
            "k1".to_owned(),
            &LimitsConfig::default(),
            TokenBudget::shared(1000),
            &Egress::personal([home.uri(), other.uri()], Vec::new()),
        )
        .expect("builds");
        let items: Vec<_> = provider
            .complete(CompletionRequest::default())
            .collect()
            .await;
        assert!(
            matches!(
                items.last(),
                Some(Err(ProviderError::Http { status: 302, .. }))
            ),
            "{items:?}"
        );
        let sent = home.received_requests().await.expect("recorded");
        assert_eq!(sent.len(), 1);
        assert_eq!(
            sent[0]
                .headers
                .get("authorization")
                .and_then(|v| v.to_str().ok()),
            Some("Bearer k1"),
            "the key went to its own host"
        );
        let leaked = other.received_requests().await.expect("recorded");
        assert!(leaked.is_empty(), "the other host received nothing");
    }

    #[tokio::test]
    async fn boxed_provider_is_provider() {
        use futures_util::StreamExt;
        let boxed: Box<dyn Provider> =
            Box::new(FakeProvider::new(vec![FakeProvider::text_turn("x")]));
        let shared: Arc<dyn Provider> =
            Arc::new(FakeProvider::new(vec![FakeProvider::text_turn("y")]));
        let wrapped = Budgeted::new(boxed, TokenBudget::shared(100));
        let items: Vec<_> = wrapped
            .complete(CompletionRequest::default())
            .collect()
            .await;
        assert_eq!(items[0], Ok(Event::TextDelta("x".to_owned())));
        let items: Vec<_> = shared
            .complete(CompletionRequest::default())
            .collect()
            .await;
        assert_eq!(items[0], Ok(Event::TextDelta("y".to_owned())));
    }
}
