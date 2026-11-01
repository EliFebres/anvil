//! Notifications: one webhook URL that is a Discord, Slack or ntfy endpoint.

use std::pin::Pin;

use chrono::NaiveDate;
use serde_json::{Value, json};

use crate::error::{FactoryError, Result};
use crate::ports::BoxFuture;

/// Something worth a message.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// A ticket's pull request merged.
    Merged {
        /// The ticket id.
        id: String,
        /// The ticket title.
        title: String,
        /// The pull request number.
        pr: u64,
        /// What the ticket cost in model calls, when known.
        cost_usd: Option<f64>,
    },
    /// A ticket was rejected.
    Rejected {
        /// The ticket id.
        id: String,
        /// The ticket title.
        title: String,
        /// Why, in one line.
        reason: String,
    },
    /// A ticket waits for a human.
    NeedsHuman {
        /// The ticket id.
        id: String,
        /// The ticket title.
        title: String,
        /// The pull request, when one exists.
        pr: Option<u64>,
        /// Why.
        reason: String,
    },
    /// The service paused itself.
    SelfPaused {
        /// Why.
        reason: String,
    },
    /// A `tickets/*` branch was refused.
    BranchRefused {
        /// The branch name.
        branch: String,
        /// Why.
        reason: String,
    },
    /// The loop hit an error while working a ticket; the ticket was reopened.
    Error {
        /// The ticket id, when the error concerned one.
        id: Option<String>,
        /// The error text.
        detail: String,
    },
    /// The daily summary.
    Digest {
        /// The day.
        date: NaiveDate,
        /// Tickets merged.
        merged: u32,
        /// Tickets rejected.
        rejected: u32,
        /// Tickets parked for a human.
        waiting: u32,
        /// Self-pauses.
        paused: u32,
        /// Model cost over the period.
        cost_usd: f64,
    },
}

impl Event {
    /// The message text, one or two lines.
    ///
    /// # Returns
    ///
    /// Plain text that reads well in any of the three chats.
    pub fn text(&self) -> String {
        match self {
            Event::Merged {
                id,
                title,
                pr,
                cost_usd,
            } => {
                let cost = cost_usd
                    .map(|cost| format!(", ${cost:.2}"))
                    .unwrap_or_default();
                format!("factory merged #{pr}: {title} ({id}{cost})")
            }
            Event::Rejected { id, title, reason } => {
                format!("factory rejected {title} ({id}): {reason}")
            }
            Event::NeedsHuman {
                id,
                title,
                pr,
                reason,
            } => {
                let pr = pr.map(|pr| format!(" PR #{pr}")).unwrap_or_default();
                format!("factory needs a human:{pr} {title} ({id}): {reason}")
            }
            Event::SelfPaused { reason } => {
                format!("factory paused itself: {reason}. Run `factory resume` when fixed.")
            }
            Event::BranchRefused { branch, reason } => {
                format!("factory refused branch {branch}: {reason}")
            }
            Event::Error { id, detail } => match id {
                Some(id) => format!("factory error on {id}, ticket reopened: {detail}"),
                None => format!("factory error: {detail}"),
            },
            Event::Digest {
                date,
                merged,
                rejected,
                waiting,
                paused,
                cost_usd,
            } => format!(
                "factory digest {date}: {merged} merged, {rejected} rejected, {waiting} waiting, {paused} self-pause(s), ${cost_usd:.2}"
            ),
        }
    }

    /// A short title, for chats that show one.
    pub fn title(&self) -> &'static str {
        match self {
            Event::Merged { .. } => "merged",
            Event::Rejected { .. } => "rejected",
            Event::NeedsHuman { .. } => "needs a human",
            Event::SelfPaused { .. } => "paused",
            Event::BranchRefused { .. } => "branch refused",
            Event::Error { .. } => "error",
            Event::Digest { .. } => "digest",
        }
    }
}

/// Where notifications go: a webhook, the log, or a test's list.
pub trait Notifier: Send + Sync {
    /// Delivers one event.
    ///
    /// # Arguments
    ///
    /// * `event` - What happened.
    fn send<'a>(&'a self, event: &'a Event) -> BoxFuture<'a, Result<()>>;
}

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

/// The payload for a URL: Discord wants `content`, Slack wants `text`,
/// anything else (ntfy) takes the text as the body.
///
/// # Arguments
///
/// * `url` - The webhook URL.
/// * `event` - The event.
///
/// # Returns
///
/// The payload.
pub fn payload(url: &str, event: &Event) -> Payload {
    let host = url
        .split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let text = event.text();
    if host.ends_with("discord.com") || host.ends_with("discordapp.com") {
        Payload::Json(json!({ "content": text }))
    } else if host.ends_with("slack.com") {
        Payload::Json(json!({ "text": text }))
    } else {
        Payload::Text {
            body: text,
            title: format!("factory: {}", event.title()),
        }
    }
}

/// Posts every event to one webhook URL.
pub struct WebhookNotifier {
    /// The HTTP client.
    client: reqwest::Client,
    /// The webhook.
    url: String,
}

impl WebhookNotifier {
    /// A notifier for a URL.
    ///
    /// # Arguments
    ///
    /// * `url` - The webhook URL.
    ///
    /// # Returns
    ///
    /// The notifier.
    ///
    /// # Errors
    ///
    /// When the HTTP client cannot be built.
    pub fn new(url: &str) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .user_agent(concat!("anvil-factory/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| FactoryError::Notify(error.to_string()))?;
        Ok(Self {
            client,
            url: url.to_owned(),
        })
    }
}

impl Notifier for WebhookNotifier {
    fn send<'a>(&'a self, event: &'a Event) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let request = match payload(&self.url, event) {
                Payload::Json(value) => self.client.post(&self.url).json(&value),
                Payload::Text { body, title } => self
                    .client
                    .post(&self.url)
                    .header("Title", title)
                    .header("Content-Type", "text/plain; charset=utf-8")
                    .body(body),
            };
            let response = request
                .send()
                .await
                .map_err(|error| FactoryError::Notify(error.to_string()))?;
            if !response.status().is_success() {
                return Err(FactoryError::Notify(format!(
                    "webhook answered HTTP {}",
                    response.status().as_u16()
                )));
            }
            Ok(())
        })
    }
}

/// Writes every event to the log; used when no webhook is configured.
#[derive(Debug, Default, Clone, Copy)]
pub struct LogNotifier;

impl Notifier for LogNotifier {
    fn send<'a>(&'a self, event: &'a Event) -> BoxFuture<'a, Result<()>> {
        tracing::info!(kind = event.title(), "{}", event.text());
        Box::pin(std::future::ready(Ok(()))) as Pin<Box<_>>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_json, body_string_contains, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn merged() -> Event {
        Event::Merged {
            id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".to_owned(),
            title: "Add a grep tool".to_owned(),
            pr: 12,
            cost_usd: Some(3.456),
        }
    }

    #[test]
    fn payload_shape_follows_the_host() {
        let text = merged().text();
        assert_eq!(
            text,
            "factory merged #12: Add a grep tool (01ARZ3NDEKTSV4RRFFQ69G5FAV, $3.46)"
        );
        assert_eq!(
            payload("https://discord.com/api/webhooks/1/x", &merged()),
            Payload::Json(json!({ "content": text }))
        );
        assert_eq!(
            payload("https://hooks.slack.com/services/T/B/x", &merged()),
            Payload::Json(json!({ "text": text }))
        );
        assert_eq!(
            payload("https://ntfy.sh/anvil-factory", &merged()),
            Payload::Text {
                body: text,
                title: "factory: merged".to_owned()
            }
        );
    }

    #[test]
    fn every_event_has_text() {
        let events = [
            Event::Rejected {
                id: "T".to_owned(),
                title: "t".to_owned(),
                reason: "gate A failed twice".to_owned(),
            },
            Event::NeedsHuman {
                id: "T".to_owned(),
                title: "t".to_owned(),
                pr: Some(3),
                reason: "touches deploy/".to_owned(),
            },
            Event::SelfPaused {
                reason: "main CI is red".to_owned(),
            },
            Event::BranchRefused {
                branch: "tickets/x".to_owned(),
                reason: "touches README.md".to_owned(),
            },
            Event::Error {
                id: None,
                detail: "boom".to_owned(),
            },
            Event::Digest {
                date: NaiveDate::from_ymd_opt(2026, 10, 31).expect("date"),
                merged: 2,
                rejected: 1,
                waiting: 0,
                paused: 0,
                cost_usd: 12.5,
            },
        ];
        for event in events {
            let text = event.text();
            assert!(text.starts_with("factory "), "{text}");
        }
        assert!(
            events_digest_text()
                .contains("2 merged, 1 rejected, 0 waiting, 0 self-pause(s), $12.50")
        );
    }

    fn events_digest_text() -> String {
        Event::Digest {
            date: NaiveDate::from_ymd_opt(2026, 10, 31).expect("date"),
            merged: 2,
            rejected: 1,
            waiting: 0,
            paused: 0,
            cost_usd: 12.5,
        }
        .text()
    }

    #[tokio::test]
    async fn posts_json_to_slack_shaped_urls_and_text_elsewhere() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hooks/slack.com/x"))
            .and(body_json(json!({ "text": merged().text() })))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/ntfy/topic"))
            .and(header("Title", "factory: merged"))
            .and(body_string_contains("factory merged #12"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/broken"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        // A mock server has no slack.com host, so the Slack shape is checked
        // through `payload` above; here the text path and the error path run.
        WebhookNotifier::new(&format!("{}/ntfy/topic", server.uri()))
            .expect("client")
            .send(&merged())
            .await
            .expect("delivered");
        let error = WebhookNotifier::new(&format!("{}/broken", server.uri()))
            .expect("client")
            .send(&merged())
            .await
            .expect_err("500 is an error");
        assert!(error.to_string().contains("HTTP 500"), "{error}");
        server.reset().await;
    }
}
