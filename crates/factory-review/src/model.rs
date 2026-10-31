//! Reviewers backed by a model: one non-streaming call per attempt on the
//! OpenAI or the Anthropic protocol.
//!
//! anvil-core speaks the same two protocols with streaming; this crate may
//! not depend on it, so the request and response shapes are repeated here in
//! their simplest form.

use std::collections::BTreeMap;
use std::time::Duration;

use anvil_types::{AnvilConfig, FactoryConfig, Price, Protocol, ProviderConfig, Stance};
use serde_json::{Value, json};

use crate::error::ReviewError;
use crate::prompt::{retry_message, system_prompt, user_message};
use crate::reviewer::{ReviewFuture, ReviewRequest, Reviewed, Reviewer};
use crate::rules::Rule;
use crate::verdict::{ReviewerId, Usage, parse_review};

/// The Anthropic API version header; kept in step with anvil-core by a test there and here.
pub const ANTHROPIC_VERSION: &str = "2026-10-21";

/// Output cap when the provider sets none; the Anthropic API requires one.
const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 16384;

/// Replies tried before a reviewer gives up on malformed JSON.
pub const MAX_ATTEMPTS: u32 = 2;

/// How much of an error body is kept in the error.
const BODY_HEAD: usize = 500;

/// One turn of the conversation with a reviewer.
#[derive(Debug, Clone)]
struct Turn {
    role: &'static str,
    text: String,
}

/// A reviewer that asks a model.
pub struct ModelReviewer {
    id: ReviewerId,
    protocol: Protocol,
    url: String,
    api_key: String,
    max_output_tokens: Option<u32>,
    price: Option<Price>,
    system: String,
    client: reqwest::Client,
}

impl ModelReviewer {
    /// Builds a reviewer for one provider and one stance.
    ///
    /// # Arguments
    ///
    /// * `provider_name` - The name from `factory.toml`, for attribution.
    /// * `stance` - Which stance this reviewer takes.
    /// * `provider` - The provider entry from the anvil configuration.
    /// * `api_key` - The key for the provider.
    /// * `rules` - The standing rules; used by the style stance.
    /// * `client` - The HTTP client, usually shared per provider.
    ///
    /// # Returns
    ///
    /// The reviewer.
    pub fn new(
        provider_name: &str,
        stance: Stance,
        provider: &ProviderConfig,
        api_key: String,
        rules: &[Rule],
        client: reqwest::Client,
    ) -> Self {
        let base = provider.base_url.trim_end_matches('/');
        let url = match provider.protocol {
            Protocol::Openai => format!("{base}/chat/completions"),
            Protocol::Anthropic => format!("{base}/messages"),
        };
        Self {
            id: ReviewerId {
                provider: provider_name.to_owned(),
                stance,
                model: provider.model.clone(),
            },
            protocol: provider.protocol,
            url,
            api_key,
            max_output_tokens: provider.max_output_tokens,
            price: provider.price_per_mtoken,
            system: system_prompt(stance, rules),
            client,
        }
    }

    /// Builds every reviewer `factory.toml` lists, keys taken from the environment.
    ///
    /// # Arguments
    ///
    /// * `factory` - The factory configuration.
    /// * `config` - The anvil configuration the provider names resolve against.
    /// * `rules` - The standing rules for the style stance.
    ///
    /// # Returns
    ///
    /// The reviewers in `[[reviewers]]` order.
    ///
    /// # Errors
    ///
    /// [`ReviewError::UnknownProvider`] for a name not in `[providers]`,
    /// [`ReviewError::MissingApiKey`] for an unset key variable, and
    /// [`ReviewError::Client`] when the HTTP client cannot be built.
    pub fn from_config(
        factory: &FactoryConfig,
        config: &AnvilConfig,
        rules: &[Rule],
    ) -> Result<Vec<Box<dyn Reviewer>>, ReviewError> {
        Self::from_config_with(factory, config, rules, |name| std::env::var(name).ok())
    }

    /// [`Self::from_config`] with the key lookup supplied, so tests need not touch the environment.
    ///
    /// # Arguments
    ///
    /// * `factory` - The factory configuration.
    /// * `config` - The anvil configuration the provider names resolve against.
    /// * `rules` - The standing rules for the style stance.
    /// * `key_lookup` - Maps an `api_key_env` name to its value.
    ///
    /// # Returns
    ///
    /// The reviewers in `[[reviewers]]` order.
    ///
    /// # Errors
    ///
    /// As for [`Self::from_config`].
    pub fn from_config_with(
        factory: &FactoryConfig,
        config: &AnvilConfig,
        rules: &[Rule],
        key_lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<Vec<Box<dyn Reviewer>>, ReviewError> {
        let timeout = Duration::from_secs(config.limits.request_timeout_s);
        let mut clients: BTreeMap<&str, reqwest::Client> = BTreeMap::new();
        let mut reviewers: Vec<Box<dyn Reviewer>> = Vec::new();
        for entry in &factory.reviewers {
            let name = entry.provider.as_str();
            let provider =
                config
                    .providers
                    .get(name)
                    .ok_or_else(|| ReviewError::UnknownProvider {
                        provider: name.to_owned(),
                    })?;
            let api_key = key_lookup(&provider.api_key_env)
                .filter(|key| !key.trim().is_empty())
                .ok_or_else(|| ReviewError::MissingApiKey {
                    provider: name.to_owned(),
                    env: provider.api_key_env.clone(),
                })?;
            let client = match clients.get(name) {
                Some(client) => client.clone(),
                None => {
                    let client = build_client(timeout)?;
                    clients.insert(name, client.clone());
                    client
                }
            };
            reviewers.push(Box::new(Self::new(
                name,
                entry.stance,
                provider,
                api_key,
                rules,
                client,
            )));
        }
        Ok(reviewers)
    }

    /// Asks the model until it returns a review or [`MAX_ATTEMPTS`] is spent.
    async fn review_with_retry(&self, request: &ReviewRequest) -> Result<Reviewed, ReviewError> {
        let mut turns = vec![Turn {
            role: "user",
            text: user_message(request),
        }];
        let mut usage = Usage {
            cost_usd: self.price.map(|_| 0.0),
            ..Usage::default()
        };
        let mut last_error = String::new();
        for attempt in 1..=MAX_ATTEMPTS {
            let (text, used) = self.complete(&turns).await?;
            usage.add(used);
            match parse_review(&text) {
                Ok(review) => return Ok(Reviewed { review, usage }),
                Err(detail) => {
                    tracing::warn!(
                        reviewer = %self.id,
                        attempt,
                        detail = %detail,
                        "reviewer reply was not a review"
                    );
                    turns.push(Turn {
                        role: "assistant",
                        text,
                    });
                    turns.push(Turn {
                        role: "user",
                        text: retry_message(&detail),
                    });
                    last_error = detail;
                }
            }
        }
        Err(ReviewError::Malformed {
            provider: self.id.provider.clone(),
            attempts: MAX_ATTEMPTS,
            detail: last_error,
        })
    }

    /// One non-streaming completion: the reply text and its usage.
    async fn complete(&self, turns: &[Turn]) -> Result<(String, Usage), ReviewError> {
        let body = match self.protocol {
            Protocol::Openai => self.openai_body(turns),
            Protocol::Anthropic => self.anthropic_body(turns),
        };
        let builder = self.client.post(&self.url).json(&body);
        let builder = match self.protocol {
            Protocol::Openai => builder.bearer_auth(&self.api_key),
            Protocol::Anthropic => builder
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", ANTHROPIC_VERSION),
        };
        tracing::debug!(reviewer = %self.id, url = %self.url, "sending review request");
        let transport = |source| ReviewError::Transport {
            provider: self.id.provider.clone(),
            source,
        };
        let response = builder.send().await.map_err(transport)?;
        let status = response.status();
        let text = response.text().await.map_err(transport)?;
        if !status.is_success() {
            return Err(ReviewError::Http {
                provider: self.id.provider.clone(),
                status: status.as_u16(),
                body: head(&text),
            });
        }
        let value: Value = serde_json::from_str(&text).map_err(|error| self.shape(error))?;
        match self.protocol {
            Protocol::Openai => self.openai_reply(&value),
            Protocol::Anthropic => self.anthropic_reply(&value),
        }
    }

    fn openai_body(&self, turns: &[Turn]) -> Value {
        let mut messages = vec![json!({"role": "system", "content": self.system})];
        messages.extend(
            turns
                .iter()
                .map(|turn| json!({"role": turn.role, "content": turn.text})),
        );
        let mut body = json!({
            "model": self.id.model,
            "stream": false,
            "messages": messages,
        });
        if let Some(max) = self.max_output_tokens {
            body["max_completion_tokens"] = json!(max);
        }
        body
    }

    fn anthropic_body(&self, turns: &[Turn]) -> Value {
        let messages: Vec<Value> = turns
            .iter()
            .map(
                |turn| json!({"role": turn.role, "content": [{"type": "text", "text": turn.text}]}),
            )
            .collect();
        json!({
            "model": self.id.model,
            "max_tokens": self.max_output_tokens.unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS),
            "system": self.system,
            "messages": messages,
        })
    }

    /// `choices[0].message.content` and `usage`; cached tokens are inside `prompt_tokens`.
    fn openai_reply(&self, value: &Value) -> Result<(String, Usage), ReviewError> {
        let text = value["choices"][0]["message"]["content"]
            .as_str()
            .ok_or_else(|| self.shape("no choices[0].message.content"))?
            .to_owned();
        let usage = &value["usage"];
        let prompt = usage["prompt_tokens"].as_u64().unwrap_or(0);
        let cached = usage["prompt_tokens_details"]["cached_tokens"]
            .as_u64()
            .unwrap_or(0)
            .min(prompt);
        let output = usage["completion_tokens"].as_u64().unwrap_or(0);
        Ok((text, self.usage(prompt - cached, output, cached)))
    }

    /// The concatenated `content[].text` blocks and `usage`.
    fn anthropic_reply(&self, value: &Value) -> Result<(String, Usage), ReviewError> {
        let blocks = value["content"]
            .as_array()
            .ok_or_else(|| self.shape("no content array"))?;
        let text: String = blocks
            .iter()
            .filter_map(|block| block["text"].as_str())
            .collect();
        let usage = &value["usage"];
        let input = usage["input_tokens"].as_u64().unwrap_or(0);
        let cached = usage["cache_read_input_tokens"].as_u64().unwrap_or(0);
        let output = usage["output_tokens"].as_u64().unwrap_or(0);
        Ok((text, self.usage(input, output, cached)))
    }

    fn usage(&self, input: u64, output: u64, cached: u64) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            cached_tokens: cached,
            cost_usd: self.price.map(|price| price.cost(input, output, cached)),
        }
    }

    fn shape(&self, detail: impl ToString) -> ReviewError {
        ReviewError::Shape {
            provider: self.id.provider.clone(),
            detail: detail.to_string(),
        }
    }
}

impl Reviewer for ModelReviewer {
    fn id(&self) -> &ReviewerId {
        &self.id
    }

    fn review<'a>(&'a self, request: &'a ReviewRequest) -> ReviewFuture<'a> {
        Box::pin(self.review_with_retry(request))
    }
}

/// A client with the configured timeout and a factory user agent.
fn build_client(timeout: Duration) -> Result<reqwest::Client, ReviewError> {
    reqwest::Client::builder()
        .timeout(timeout)
        .user_agent(format!("factory-review/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(ReviewError::Client)
}

/// The first [`BODY_HEAD`] characters on one line.
fn head(text: &str) -> String {
    let one_line: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= BODY_HEAD {
        one_line
    } else {
        one_line.chars().take(BODY_HEAD).collect::<String>() + "…"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reviewer::TicketSummary;
    use crate::verdict::{Decision, Severity};
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    const REVIEW: &str = r#"{"verdict":"changes","findings":[{"severity":"major","file":"src/lib.rs","line":3,"message":"m","suggested_fix":"f"}]}"#;

    fn provider(protocol: Protocol, base_url: &str) -> ProviderConfig {
        ProviderConfig {
            protocol,
            base_url: base_url.to_owned(),
            api_key_env: "KEY".to_owned(),
            model: "m-1".to_owned(),
            context_window: 100_000,
            supports_tools: true,
            supports_reasoning: false,
            max_output_tokens: Some(2048),
            price_per_mtoken: Some(Price {
                input: 1.0,
                output: 10.0,
                cached: Some(0.1),
            }),
        }
    }

    fn reviewer(protocol: Protocol, base_url: &str) -> ModelReviewer {
        ModelReviewer::new(
            "p",
            Stance::Correctness,
            &provider(protocol, base_url),
            "sk-test".to_owned(),
            &[],
            build_client(Duration::from_secs(5)).expect("client"),
        )
    }

    fn request() -> ReviewRequest {
        ReviewRequest {
            ticket: TicketSummary::from_pull("t", "- [ ] works"),
            diff: "+x".to_owned(),
            changed_files: vec!["src/lib.rs".to_owned()],
            guidelines: None,
        }
    }

    fn openai_reply(text: &str) -> Value {
        json!({
            "choices": [{"message": {"role": "assistant", "content": text}}],
            "usage": {"prompt_tokens": 1000, "completion_tokens": 50, "prompt_tokens_details": {"cached_tokens": 400}}
        })
    }

    fn anthropic_reply(text: &str) -> Value {
        json!({
            "content": [{"type": "text", "text": "Here: "}, {"type": "text", "text": text}],
            "usage": {"input_tokens": 600, "output_tokens": 50, "cache_read_input_tokens": 400}
        })
    }

    #[tokio::test]
    async fn openai_protocol_sends_bearer_and_reads_the_choice() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("authorization", "Bearer sk-test"))
            .and(body_partial_json(
                json!({"model": "m-1", "stream": false, "max_completion_tokens": 2048}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(openai_reply(REVIEW)))
            .expect(1)
            .mount(&server)
            .await;
        let reviewed = reviewer(Protocol::Openai, &format!("{}/v1/", server.uri()))
            .review(&request())
            .await
            .expect("reviewed");
        assert_eq!(reviewed.review.verdict, Decision::Changes);
        assert_eq!(reviewed.review.findings[0].severity, Severity::Major);
        assert_eq!(reviewed.usage.input_tokens, 600);
        assert_eq!(reviewed.usage.cached_tokens, 400);
        assert_eq!(reviewed.usage.output_tokens, 50);
        let expected = 600.0 * 1e-6 + 50.0 * 10e-6 + 400.0 * 0.1e-6;
        assert!((reviewed.usage.cost_usd.expect("priced") - expected).abs() < 1e-12);

        let sent = &server.received_requests().await.expect("recorded")[0];
        let body: Value = serde_json::from_slice(&sent.body).expect("json");
        assert_eq!(body["messages"][0]["role"], "system");
        assert!(
            body["messages"][0]["content"]
                .as_str()
                .expect("text")
                .contains("correctness")
        );
        assert_eq!(body["messages"][1]["role"], "user");
    }

    #[tokio::test]
    async fn anthropic_protocol_sends_key_and_version_and_joins_text_blocks() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .and(header("x-api-key", "sk-test"))
            .and(header("anthropic-version", ANTHROPIC_VERSION))
            .and(body_partial_json(
                json!({"model": "m-1", "max_tokens": 2048}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(anthropic_reply(REVIEW)))
            .expect(1)
            .mount(&server)
            .await;
        let reviewed = reviewer(Protocol::Anthropic, &server.uri())
            .review(&request())
            .await
            .expect("reviewed");
        assert_eq!(reviewed.review.findings.len(), 1);
        assert_eq!(reviewed.usage.input_tokens, 600);
        assert_eq!(reviewed.usage.cached_tokens, 400);
        let sent = &server.received_requests().await.expect("recorded")[0];
        let body: Value = serde_json::from_slice(&sent.body).expect("json");
        assert!(
            body["system"]
                .as_str()
                .expect("system")
                .contains("correctness")
        );
        assert_eq!(body["messages"].as_array().expect("messages").len(), 1);
    }

    #[tokio::test]
    async fn one_malformed_reply_is_retried_with_the_reply_in_the_conversation() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(move |request: &Request| {
                let body: Value = serde_json::from_slice(&request.body).expect("json");
                let turns = body["messages"].as_array().expect("messages").len();
                let text = if turns == 2 { "I approve." } else { REVIEW };
                ResponseTemplate::new(200).set_body_json(openai_reply(text))
            })
            .expect(2)
            .mount(&server)
            .await;
        let reviewed = reviewer(Protocol::Openai, &server.uri())
            .review(&request())
            .await
            .expect("reviewed on the retry");
        assert_eq!(reviewed.review.verdict, Decision::Changes);
        assert_eq!(reviewed.usage.output_tokens, 100, "both calls are counted");
        let requests = server.received_requests().await.expect("recorded");
        let second: Value = serde_json::from_slice(&requests[1].body).expect("json");
        let messages = second["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[2]["role"], "assistant");
        assert_eq!(messages[2]["content"], "I approve.");
        assert!(
            messages[3]["content"]
                .as_str()
                .expect("text")
                .contains("not a well-formed review")
        );
    }

    #[tokio::test]
    async fn two_malformed_replies_fail() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(openai_reply("nope")))
            .expect(2)
            .mount(&server)
            .await;
        let error = reviewer(Protocol::Openai, &server.uri())
            .review(&request())
            .await
            .expect_err("fails");
        assert!(
            matches!(&error, ReviewError::Malformed { attempts: 2, .. }),
            "{error}"
        );
    }

    #[tokio::test]
    async fn http_errors_and_bad_shapes_are_named() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/messages"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom  boom"))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"choices": []})))
            .mount(&server)
            .await;
        let error = reviewer(Protocol::Anthropic, &server.uri())
            .review(&request())
            .await
            .expect_err("fails");
        assert!(
            matches!(&error, ReviewError::Http { status: 500, body, .. } if body == "boom boom"),
            "{error}"
        );
        let error = reviewer(Protocol::Openai, &server.uri())
            .review(&request())
            .await
            .expect_err("fails");
        assert!(matches!(&error, ReviewError::Shape { .. }), "{error}");
    }

    #[test]
    fn from_config_resolves_providers_keys_and_stances() {
        let factory: FactoryConfig =
            toml::from_str(&FactoryConfig::template("o/r")).expect("parses");
        let config: AnvilConfig =
            toml::from_str(anvil_types::config::SETUP_PROVIDERS).expect("parses");
        let reviewers =
            ModelReviewer::from_config_with(&factory, &config, &[], |_| Some("k".to_owned()))
                .expect("built");
        let ids: Vec<String> = reviewers.iter().map(|r| r.id().to_string()).collect();
        assert_eq!(
            ids,
            vec![
                "openai/correctness",
                "google/safety",
                "openai/style",
                "google/simplicity"
            ]
        );
        assert_eq!(reviewers[1].id().model, "gemini-4-argon");

        let error = ModelReviewer::from_config_with(&factory, &config, &[], |env| {
            (env != "GEMINI_API_KEY").then(|| "k".to_owned())
        })
        .err()
        .expect("missing key");
        assert!(
            matches!(&error, ReviewError::MissingApiKey { provider, env } if provider == "google" && env == "GEMINI_API_KEY"),
            "{error}"
        );

        let mut unknown = factory.clone();
        unknown.reviewers[0].provider = "nope".to_owned();
        let error =
            ModelReviewer::from_config_with(&unknown, &config, &[], |_| Some("k".to_owned()))
                .err()
                .expect("unknown");
        assert!(matches!(&error, ReviewError::UnknownProvider { provider } if provider == "nope"));
    }

    #[test]
    fn anthropic_version_is_pinned() {
        assert_eq!(ANTHROPIC_VERSION, "2026-10-21");
    }
}
