//! The swarm end to end: reviewers built from `factory.toml` and the anvil
//! configuration, talking to one mock server per protocol.

use anvil_types::{AnvilConfig, FactoryConfig};
use factory_review::{Decision, ModelReviewer, ReviewRequest, Severity, TicketSummary};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The anvil configuration with the two providers pointed at the mocks.
fn anvil_config(openai: &str, anthropic: &str) -> AnvilConfig {
    let text = format!(
        r#"
[providers.coder]
protocol = "anthropic"
base_url = "https://coder.example/v1"
api_key_env = "CODER_KEY"
model = "coder-1"
context_window = 100000
supports_tools = true
supports_reasoning = false

[providers.alpha]
protocol = "openai"
base_url = "{openai}/v1"
api_key_env = "ALPHA_KEY"
model = "alpha-1"
context_window = 100000
supports_tools = true
supports_reasoning = false
price_per_mtoken = {{ input = 1.0, output = 2.0 }}

[providers.beta]
protocol = "anthropic"
base_url = "{anthropic}/v1"
api_key_env = "BETA_KEY"
model = "beta-1"
context_window = 100000
supports_tools = true
supports_reasoning = false

[roles]
coder = "coder"
summarizer = "coder"
"#
    );
    toml::from_str(&text).expect("config parses")
}

fn factory_config() -> FactoryConfig {
    toml::from_str(
        r#"
[roles]
coder = "coder"

[[reviewers]]
provider = "alpha"
stance = "correctness"

[[reviewers]]
provider = "beta"
stance = "safety"

[[reviewers]]
provider = "alpha"
stance = "style"

[github]
repo = "o/r"

[notify]
webhook_env = "W"
"#,
    )
    .expect("factory parses")
}

#[tokio::test]
async fn reviewers_from_config_run_concurrently_on_both_protocols() {
    let openai = MockServer::start().await;
    let anthropic = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"content": "{\"verdict\":\"approve\",\"findings\":[{\"severity\":\"minor\",\"file\":\"a.rs\",\"line\":1,\"message\":\"nit\"}]}"}}],
            "usage": {"prompt_tokens": 100, "completion_tokens": 10}
        })))
        .expect(2)
        .mount(&openai)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": [{"type": "text", "text": "{\"verdict\":\"changes\",\"findings\":[{\"severity\":\"blocker\",\"file\":\"a.rs\",\"line\":2,\"message\":\"leaks the key\"}]}"}],
            "usage": {"input_tokens": 100, "output_tokens": 10}
        })))
        .expect(1)
        .mount(&anthropic)
        .await;

    let factory = factory_config();
    let config = anvil_config(&openai.uri(), &anthropic.uri());
    factory
        .check_independence(&config)
        .expect("alpha and beta are independent of coder and each other");
    let reviewers =
        ModelReviewer::from_config_with(&factory, &config, &[], |_| Some("k".to_owned()))
            .expect("reviewers");
    assert_eq!(reviewers.len(), 3);

    let request = ReviewRequest {
        ticket: TicketSummary::from_pull("Add thing", "- [ ] thing works"),
        diff: "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1,2 @@\n x\n+y\n"
            .to_owned(),
        changed_files: vec!["a.rs".to_owned()],
        guidelines: None,
    };
    let verdict = factory_review::review(&request, &reviewers).await;

    assert_eq!(verdict.decision, Decision::Changes);
    assert_eq!(verdict.reason, "1 blocker");
    assert_eq!(verdict.findings.len(), 1);
    assert_eq!(verdict.findings[0].reviewer.to_string(), "beta/safety");
    assert_eq!(verdict.findings[0].finding.severity, Severity::Blocker);
    assert_eq!(verdict.minors.len(), 2);
    let order: Vec<String> = verdict.raw.iter().map(|r| r.reviewer.to_string()).collect();
    assert_eq!(
        order,
        vec!["alpha/correctness", "beta/safety", "alpha/style"]
    );
    assert_eq!(verdict.raw[0].reviewer.model, "alpha-1");
    assert!(verdict.raw[0].usage.cost_usd.is_some(), "alpha is priced");
    assert!(verdict.raw[1].usage.cost_usd.is_none(), "beta is not");
    assert_eq!(verdict.usage().input_tokens, 300);
}
