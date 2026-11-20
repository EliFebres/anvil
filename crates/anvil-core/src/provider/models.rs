//! Asking an OpenAI-compatible server which models it serves, for
//! `anvil setup --endpoint`.

use std::time::Duration;

use serde_json::Value;

use super::ProviderError;
use crate::net::{self, Egress};

/// How long the probe waits for a TCP connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// One model a server lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServedModel {
    /// The id to send as `model`.
    pub id: String,
    /// The context size the server reports, when it reports one
    /// (`max_model_len` from vLLM, `context_length` from others).
    pub context_window: Option<u32>,
}

/// Lists the models at `{base_url}/models`.
///
/// # Arguments
///
/// * `base_url` - The endpoint, such as `https://gpu.example/v1`.
/// * `api_key` - Sent as a bearer token when given.
/// * `egress` - Where requests may go and how connections are made;
///   checked before anything is sent.
/// * `timeout` - Wall clock for the whole request.
///
/// # Returns
///
/// The models, in the server's order.
///
/// # Errors
///
/// [`ProviderError::Refused`] when `egress` refuses the URL,
/// [`ProviderError::Http`] for a non-success status (any echoed key is
/// redacted), [`ProviderError::Timeout`], [`ProviderError::Transport`] when
/// the server cannot be reached, and [`ProviderError::Malformed`] when the
/// answer is not a model list.
pub async fn list_models(
    base_url: &str,
    api_key: Option<&str>,
    egress: &Egress,
    timeout: Duration,
) -> Result<Vec<ServedModel>, ProviderError> {
    let base = base_url.trim_end_matches('/');
    egress.check(base).map_err(|error| ProviderError::Refused {
        detail: error.to_string(),
    })?;
    let client = net::http_client(egress, CONNECT_TIMEOUT)
        .map_err(|error| ProviderError::transport(error.to_string()))?;
    let mut request = client.get(&format!("{base}/models"));
    if let Some(key) = api_key {
        request = request.api_key("authorization", &format!("Bearer {key}"), base);
    }
    let response = tokio::time::timeout(timeout, request.send())
        .await
        .map_err(|_| ProviderError::Timeout {
            after_s: timeout.as_secs(),
        })?
        .map_err(|error| match error {
            net::SendError::Refused(error) => ProviderError::Refused {
                detail: error.to_string(),
            },
            other => ProviderError::transport(other.to_string()),
        })?;
    let status = response.status().as_u16();
    let body = response
        .text()
        .await
        .map_err(|error| ProviderError::transport(error.to_string()))?;
    if !(200..300).contains(&status) {
        let body = match api_key {
            Some(key) if !key.is_empty() => body.replace(key, "[redacted]"),
            _ => body,
        };
        return Err(ProviderError::Http { status, body });
    }
    parse_models(&body)
}

/// Reads an OpenAI-style `{"data": [{"id": ...}, ...]}` model list.
fn parse_models(body: &str) -> Result<Vec<ServedModel>, ProviderError> {
    let value: Value = serde_json::from_str(body).map_err(|error| {
        ProviderError::malformed(format!("the model list is not JSON: {error}"))
    })?;
    let entries = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| ProviderError::malformed("the model list has no `data` array"))?;
    let models: Vec<ServedModel> = entries
        .iter()
        .filter_map(|entry| {
            let id = entry.get("id")?.as_str()?.to_owned();
            let context_window = ["max_model_len", "context_length", "context_window"]
                .iter()
                .find_map(|key| entry.get(*key).and_then(Value::as_u64))
                .and_then(|size| u32::try_from(size).ok());
            Some(ServedModel { id, context_window })
        })
        .collect();
    if models.is_empty() {
        return Err(ProviderError::malformed("the server lists no models"));
    }
    Ok(models)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn reads_ids_and_reported_context_sizes() {
        let models = parse_models(
            r#"{"object": "list", "data": [
                {"id": "qwen", "max_model_len": 32768},
                {"id": "llama", "context_length": 8192},
                {"id": "plain"}
            ]}"#,
        )
        .expect("parses");
        assert_eq!(models[0].context_window, Some(32768));
        assert_eq!(models[1].context_window, Some(8192));
        assert_eq!(
            models[2],
            ServedModel {
                id: "plain".to_owned(),
                context_window: None
            }
        );
        assert!(parse_models(r#"{"data": []}"#).is_err());
        assert!(parse_models("not json").is_err());
    }

    #[tokio::test]
    async fn lists_the_models_of_a_server_with_the_key_as_a_bearer_token() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("authorization", "Bearer k1"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(r#"{"data": [{"id": "m"}]}"#, "application/json"),
            )
            .mount(&server)
            .await;
        let base = format!("{}/v1/", server.uri());
        let models = list_models(
            &base,
            Some("k1"),
            &Egress::personal([base.as_str()], Vec::new()),
            Duration::from_secs(5),
        )
        .await
        .expect("lists");
        assert_eq!(models[0].id, "m");
    }

    #[tokio::test]
    async fn refuses_an_endpoint_the_rules_do_not_allow_before_sending() {
        let managed = Egress::managed(&["models.corp.example:443".to_owned()], true);
        let error = list_models(
            "http://127.0.0.1:9/v1",
            None,
            &managed,
            Duration::from_secs(1),
        )
        .await
        .expect_err("refused");
        assert!(matches!(error, ProviderError::Refused { .. }), "{error}");
    }
}
