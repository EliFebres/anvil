//! One short webhook message when a session needs the owner, is blocked,
//! finishes or fails.
//!
//! The payload shapes mirror `factory-core`'s `notify.rs` (Discord wants
//! `content`, Slack wants `text`, anything else is ntfy with a `Title`
//! header). The code cannot be shared: `anvil-cli` may not depend on a
//! `factory-*` crate, and `anvil-types` may not use reqwest.

use std::time::Duration;

use anvil_types::SessionState;
use serde_json::{Value, json};

/// How long one POST may take before it is abandoned.
const TIMEOUT: Duration = Duration::from_secs(5);

/// The request body a webhook host expects.
#[derive(Debug, Clone, PartialEq)]
pub enum Payload {
    /// A JSON object.
    Json(Value),
    /// A plain-text body with a title header, for ntfy.
    Text {
        /// The body.
        body: String,
        /// The `Title` header.
        title: String,
    },
}

/// A message to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// The text, one line.
    pub text: String,
    /// A short title, for hosts that show one.
    pub title: String,
}

impl Message {
    /// The message for a session entering `state`.
    ///
    /// # Arguments
    ///
    /// * `state` - The state entered.
    /// * `task` - The first line of the task.
    /// * `detail` - Why, in one line.
    ///
    /// # Returns
    ///
    /// The message, or `None` for `working`, which is never announced.
    pub fn for_state(state: SessionState, task: &str, detail: &str) -> Option<Self> {
        let title = match state {
            SessionState::Working => return None,
            SessionState::NeedsInput => "needs you",
            SessionState::Blocked => "blocked",
            SessionState::Done => "done",
            SessionState::Failed => "failed",
        };
        Some(Self {
            text: format!("anvil {title}: {task} ({detail})"),
            title: format!("anvil: {title}"),
        })
    }
}

/// The payload for a URL.
///
/// # Arguments
///
/// * `url` - The webhook URL.
/// * `message` - The message.
///
/// # Returns
///
/// The payload for the host the URL names.
pub fn payload(url: &str, message: &Message) -> Payload {
    let host = url
        .split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if host.ends_with("discord.com") || host.ends_with("discordapp.com") {
        Payload::Json(json!({ "content": message.text }))
    } else if host.ends_with("slack.com") {
        Payload::Json(json!({ "text": message.text }))
    } else {
        Payload::Text {
            body: message.text.clone(),
            title: message.title.clone(),
        }
    }
}

/// The webhook URL, when the configuration names a variable that holds one.
///
/// # Arguments
///
/// * `variable` - The `[notify] webhook_env` name, if configured.
/// * `get` - Reads an environment variable.
///
/// # Returns
///
/// The URL, or `None` when nothing is configured or the variable is unset
/// or not an http(s) URL.
pub fn webhook_url(variable: Option<&str>, get: impl Fn(&str) -> Option<String>) -> Option<String> {
    let url = get(variable?)?;
    let url = url.trim();
    (url.starts_with("http://") || url.starts_with("https://")).then(|| url.to_owned())
}

/// Posts messages to one webhook.
#[derive(Debug, Clone)]
pub struct Webhook {
    client: reqwest::Client,
    url: String,
}

impl Webhook {
    /// A sender for a URL.
    ///
    /// # Arguments
    ///
    /// * `url` - The webhook URL.
    ///
    /// # Returns
    ///
    /// The sender, or `None` when the HTTP client cannot be built.
    pub fn new(url: &str) -> Option<Self> {
        let client = reqwest::Client::builder()
            .timeout(TIMEOUT)
            .user_agent(concat!("anvil/", env!("CARGO_PKG_VERSION")))
            .build()
            .inspect_err(|error| tracing::warn!(%error, "cannot build the notification client"))
            .ok()?;
        Some(Self {
            client,
            url: url.to_owned(),
        })
    }

    /// Posts one message. Never fails the caller: a problem is logged.
    ///
    /// # Arguments
    ///
    /// * `message` - What to say.
    pub async fn send(&self, message: &Message) {
        let request = match payload(&self.url, message) {
            Payload::Json(value) => self.client.post(&self.url).json(&value),
            Payload::Text { body, title } => self
                .client
                .post(&self.url)
                .header("Title", title)
                .header("Content-Type", "text/plain; charset=utf-8")
                .body(body),
        };
        match request.send().await {
            Ok(response) if response.status().is_success() => {}
            Ok(response) => {
                tracing::warn!(
                    status = response.status().as_u16(),
                    "the webhook refused the message"
                );
            }
            Err(error) => tracing::warn!(%error, "cannot post the notification"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn message() -> Message {
        Message::for_state(SessionState::Blocked, "fix it", "budget exhausted").expect("message")
    }

    #[test]
    fn working_is_never_announced() {
        assert_eq!(Message::for_state(SessionState::Working, "t", "d"), None);
        for state in [
            SessionState::NeedsInput,
            SessionState::Blocked,
            SessionState::Done,
            SessionState::Failed,
        ] {
            assert!(Message::for_state(state, "t", "d").is_some());
        }
    }

    #[test]
    fn payload_shape_follows_the_host() {
        let text = "anvil blocked: fix it (budget exhausted)";
        assert_eq!(message().text, text);
        assert_eq!(
            payload("https://discord.com/api/webhooks/1/x", &message()),
            Payload::Json(json!({ "content": text }))
        );
        assert_eq!(
            payload("https://hooks.slack.com/services/T/B/x", &message()),
            Payload::Json(json!({ "text": text }))
        );
        assert_eq!(
            payload("https://ntfy.sh/anvil", &message()),
            Payload::Text {
                body: text.to_owned(),
                title: "anvil: blocked".to_owned()
            }
        );
    }

    #[test]
    fn the_url_comes_from_the_named_variable_only_when_it_is_a_url() {
        let env = |name: &str| match name {
            "GOOD" => Some(" https://ntfy.sh/x \n".to_owned()),
            "BAD" => Some("not a url".to_owned()),
            _ => None,
        };
        assert_eq!(
            webhook_url(Some("GOOD"), env),
            Some("https://ntfy.sh/x".to_owned())
        );
        assert_eq!(webhook_url(Some("BAD"), env), None);
        assert_eq!(webhook_url(Some("UNSET"), env), None);
        assert_eq!(webhook_url(None, env), None);
    }

    #[tokio::test]
    async fn a_text_host_gets_one_post_with_a_title() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(header("Title", "anvil: blocked"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        Webhook::new(&server.uri())
            .expect("client")
            .send(&message())
            .await;
    }

    #[tokio::test]
    async fn an_unreachable_or_refusing_webhook_is_not_an_error() {
        Webhook::new("http://127.0.0.1:9/hook")
            .expect("client")
            .send(&message())
            .await;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        Webhook::new(&server.uri())
            .expect("client")
            .send(&message())
            .await;
    }
}
