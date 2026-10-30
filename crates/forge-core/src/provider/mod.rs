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
pub mod openai;
pub mod retry;
pub mod sse;
pub mod text_tools;
pub mod types;

use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use forge_types::{LimitsConfig, Protocol, ProviderConfig};
use futures_util::Stream;

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
///
/// # Returns
///
/// The provider.
///
/// # Errors
///
/// [`ProviderError::MissingApiKey`] when the configured environment variable
/// is unset or empty, or [`ProviderError::Transport`] when the HTTP client
/// cannot be built.
pub fn from_config(
    name: &str,
    cfg: &ProviderConfig,
    limits: &LimitsConfig,
    budget: SharedBudget,
) -> Result<Box<dyn Provider>, ProviderError> {
    let api_key = read_api_key(&cfg.api_key_env)?;
    build(name, cfg, api_key, limits, budget)
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
) -> Result<Box<dyn Provider>, ProviderError> {
    let client = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .map_err(|error| ProviderError::transport(error.to_string()))?;
    let adapter: Box<dyn Provider> = match cfg.protocol {
        Protocol::Openai => Box::new(OpenAiProvider::new(name, cfg, api_key, limits, client)),
        Protocol::Anthropic => Box::new(AnthropicProvider::new(name, cfg, api_key, limits, client)),
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
            api_key_env: "FORGE_TEST_KEY_THAT_IS_NEVER_SET".to_owned(),
            model: "m".to_owned(),
            context_window: 1000,
            supports_tools,
            supports_reasoning: false,
            max_output_tokens: None,
        }
    }

    #[test]
    fn from_config_missing_env_is_missing_api_key() {
        let result = from_config(
            "test",
            &config(Protocol::Openai, true),
            &LimitsConfig::default(),
            TokenBudget::shared(10),
        );
        assert!(matches!(
            result,
            Err(ProviderError::MissingApiKey { env }) if env == "FORGE_TEST_KEY_THAT_IS_NEVER_SET"
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
                );
                assert!(provider.is_ok(), "{protocol:?} tools={supports_tools}");
            }
        }
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
