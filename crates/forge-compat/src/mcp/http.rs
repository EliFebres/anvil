//! The streamable HTTP transport: one POST per message, JSON or SSE back.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use forge_core::provider::sse::SseParser;
use futures_util::StreamExt;
use serde_json::Value;

use super::client::{RpcFuture, RpcTransport};
use super::jsonrpc::{
    Incoming, McpError, Notification, PROTOCOL_VERSION, Request, Response, parse_incoming,
};

/// The header that carries the server's session id.
const SESSION_HEADER: &str = "Mcp-Session-Id";

/// A remote server reached over HTTP.
pub struct HttpTransport {
    /// The server name, for errors.
    name: String,
    /// The endpoint.
    url: String,
    /// Extra request headers, such as `Authorization`.
    headers: BTreeMap<String, String>,
    /// The HTTP client.
    client: reqwest::Client,
    /// The session id the server assigned, once it has.
    session_id: Mutex<Option<String>>,
    /// The next request id.
    next_id: AtomicU64,
}

impl HttpTransport {
    /// Builds a transport. Nothing is sent until the first request.
    ///
    /// # Arguments
    ///
    /// * `name` - The server name.
    /// * `url` - The endpoint URL.
    /// * `headers` - Extra request headers.
    ///
    /// # Returns
    ///
    /// The transport.
    ///
    /// # Errors
    ///
    /// [`McpError::Transport`] when the HTTP client cannot be built.
    pub fn new(
        name: &str,
        url: &str,
        headers: &BTreeMap<String, String>,
    ) -> Result<Self, McpError> {
        let client = reqwest::Client::builder()
            .build()
            .map_err(|error| McpError::Transport {
                server: name.to_owned(),
                detail: error.to_string(),
            })?;
        Ok(Self {
            name: name.to_owned(),
            url: url.to_owned(),
            headers: headers.clone(),
            client,
            session_id: Mutex::new(None),
            next_id: AtomicU64::new(1),
        })
    }

    /// Sends one JSON-RPC message and returns the response, if any.
    async fn post(&self, body: String, want_id: Option<u64>) -> Result<Option<Response>, McpError> {
        let mut request = self
            .client
            .post(&self.url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", PROTOCOL_VERSION);
        for (key, value) in &self.headers {
            request = request.header(key, value);
        }
        if let Some(session) = self.session_id.lock().ok().and_then(|id| id.clone()) {
            request = request.header(SESSION_HEADER, session);
        }
        let response = request
            .body(body)
            .send()
            .await
            .map_err(|error| McpError::Transport {
                server: self.name.clone(),
                detail: error.to_string(),
            })?;
        if let Some(session) = response
            .headers()
            .get(SESSION_HEADER)
            .and_then(|value| value.to_str().ok())
            && let Ok(mut slot) = self.session_id.lock()
        {
            *slot = Some(session.to_owned());
        }
        let status = response.status();
        if !status.is_success() {
            let detail = response.text().await.unwrap_or_default();
            return Err(McpError::Http {
                server: self.name.clone(),
                status: status.as_u16(),
                detail: detail.trim().chars().take(500).collect(),
            });
        }
        let Some(want_id) = want_id else {
            return Ok(None);
        };
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if content_type.starts_with("text/event-stream") {
            let mut stream = response.bytes_stream();
            let mut parser = SseParser::default();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|error| McpError::Transport {
                    server: self.name.clone(),
                    detail: error.to_string(),
                })?;
                for frame in parser.push(&chunk) {
                    if let Some(response) = response_with_id(&frame.data, want_id) {
                        return Ok(Some(response));
                    }
                }
            }
            if let Some(frame) = parser.finish()
                && let Some(response) = response_with_id(&frame.data, want_id)
            {
                return Ok(Some(response));
            }
            return Err(McpError::Protocol {
                server: self.name.clone(),
                detail: format!("event stream ended without a response to request {want_id}"),
            });
        }
        let text = response.text().await.map_err(|error| McpError::Transport {
            server: self.name.clone(),
            detail: error.to_string(),
        })?;
        match parse_incoming(&text) {
            Ok(Incoming::Response(response)) => Ok(Some(response)),
            Ok(_) => Err(McpError::Protocol {
                server: self.name.clone(),
                detail: "expected a response, got a request or notification".to_owned(),
            }),
            Err(detail) => Err(McpError::Protocol {
                server: self.name.clone(),
                detail: format!("response is not JSON-RPC: {detail}"),
            }),
        }
    }
}

/// Parses an SSE data payload and keeps it when it answers `want_id`.
fn response_with_id(data: &str, want_id: u64) -> Option<Response> {
    match parse_incoming(data) {
        Ok(Incoming::Response(response)) if response.numeric_id() == Some(want_id) => {
            Some(response)
        }
        _ => None,
    }
}

impl RpcTransport for HttpTransport {
    fn request<'a>(&'a self, method: &'a str, params: Option<Value>) -> RpcFuture<'a, Response> {
        Box::pin(async move {
            let id = self.next_id.fetch_add(1, Ordering::SeqCst);
            let body =
                serde_json::to_string(&Request::new(id, method, params)).map_err(|error| {
                    McpError::Protocol {
                        server: self.name.clone(),
                        detail: error.to_string(),
                    }
                })?;
            self.post(body, Some(id))
                .await?
                .ok_or_else(|| McpError::Protocol {
                    server: self.name.clone(),
                    detail: format!("no response body for `{method}`"),
                })
        })
    }

    fn notify<'a>(&'a self, method: &'a str, params: Option<Value>) -> RpcFuture<'a, ()> {
        Box::pin(async move {
            let body =
                serde_json::to_string(&Notification::new(method, params)).map_err(|error| {
                    McpError::Protocol {
                        server: self.name.clone(),
                        detail: error.to_string(),
                    }
                })?;
            self.post(body, None).await.map(|_| ())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_frame_that_answers_our_request_out_of_a_stream() {
        let body = b"event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\n\
                     data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"other\":true}}\n\n\
                     data: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"tools\":[]}}\n\n";
        let mut parser = SseParser::default();
        let found: Vec<Response> = parser
            .push(body)
            .iter()
            .filter_map(|frame| response_with_id(&frame.data, 2))
            .collect();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].result, Some(serde_json::json!({"tools": []})));
    }
}
