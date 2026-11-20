//! The streamable HTTP transport: one POST per message, JSON or an SSE
//! stream back.
//!
//! Requests go through `anvil_core::net::HttpClient`, so the egress rules
//! are checked on every POST and redirects are not followed. The
//! definition's headers are bound to the server's own `host:port`. anvil
//! does not open the optional GET stream, and answers any request the
//! server sends inside a response stream with "method not found", as the
//! stdio transport does.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use anvil_core::net::{HttpClient, HttpResponse, SendError};
use anvil_core::provider::sse::SseParser;
use serde_json::{Value, json};

use super::client::{RpcFuture, RpcTransport};
use super::jsonrpc::{
    Incoming, METHOD_NOT_FOUND, McpError, Notification, PROTOCOL_VERSION, Request, Response,
    parse_incoming,
};

/// The header that carries the server's session id.
const SESSION_HEADER: &str = "mcp-session-id";

/// The most response bytes read for one message.
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

/// Removes from an error body every header value this transport sends,
/// and each word of one (`t0k3n` of `Bearer t0k3n`) long enough to be a
/// secret, then the built-in secret shapes, so a server that echoes its
/// credentials puts none of them into an error, a tool result or a log.
fn scrub(body: &str, headers: &BTreeMap<String, String>) -> String {
    let mut values: Vec<&str> = headers
        .values()
        .flat_map(|value| {
            std::iter::once(value.trim()).chain(value.split_whitespace().filter(|w| w.len() >= 4))
        })
        .filter(|value| !value.is_empty())
        .collect();
    values.sort_by_key(|value| std::cmp::Reverse(value.len()));
    let mut text = body.to_owned();
    for value in values {
        text = text.replace(value, &anvil_types::redact::marker("header"));
    }
    anvil_types::redact::Redactor::builtin().redact(&text).text
}

/// A remote server reached over HTTP.
pub struct HttpTransport {
    /// The server name, for errors.
    name: String,
    /// The endpoint.
    url: String,
    /// Extra request headers, such as `Authorization`, already expanded.
    headers: BTreeMap<String, String>,
    /// The client, which checks every request against the egress rules.
    client: HttpClient,
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
    /// * `headers` - Extra request headers, sent only to the URL's host.
    /// * `client` - The client from `anvil_core::net::http_client`.
    ///
    /// # Returns
    ///
    /// The transport.
    pub fn new(
        name: &str,
        url: &str,
        headers: BTreeMap<String, String>,
        client: HttpClient,
    ) -> Self {
        Self {
            name: name.to_owned(),
            url: url.to_owned(),
            headers,
            client,
            session_id: Mutex::new(None),
            next_id: AtomicU64::new(1),
        }
    }

    /// Sends one JSON-RPC message and, when `want_id` is set, returns the
    /// response that answers it.
    async fn post(&self, body: &Value, want_id: Option<u64>) -> Result<Option<Response>, McpError> {
        let mut response = self.send(body).await?;
        let Some(want_id) = want_id else {
            return Ok(None);
        };
        let streamed = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.to_ascii_lowercase().starts_with("text/event-stream"));
        if streamed {
            return self.read_stream(&mut response, want_id).await.map(Some);
        }
        let body = self.read_body(&mut response).await?;
        match parse_incoming(&String::from_utf8_lossy(&body)) {
            Ok(Incoming::Response(response)) if response.numeric_id() == Some(want_id) => {
                Ok(Some(response))
            }
            Ok(_) => Err(self.protocol("expected the response to our request".to_owned())),
            Err(detail) => Err(self.protocol(format!("response is not JSON-RPC: {detail}"))),
        }
    }

    /// POSTs one message, remembers the session id the server assigns, and
    /// turns a non-success status into an error.
    async fn send(&self, body: &Value) -> Result<HttpResponse, McpError> {
        let mut request = self
            .client
            .post(&self.url)
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", PROTOCOL_VERSION)
            .json(body);
        for (key, value) in &self.headers {
            request = request.api_key(key, value, &self.url);
        }
        if let Some(session) = self.session_id.lock().ok().and_then(|id| id.clone()) {
            request = request.header(SESSION_HEADER, &session);
        }
        let mut response = request
            .send()
            .await
            .map_err(|error| self.send_error(error))?;
        if let Some(session) = response
            .headers()
            .get(SESSION_HEADER)
            .and_then(|value| value.to_str().ok())
            && let Ok(mut slot) = self.session_id.lock()
        {
            *slot = Some(session.to_owned());
        }
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            let body = self.read_body(&mut response).await.unwrap_or_default();
            let detail: String = scrub(&String::from_utf8_lossy(&body), &self.headers)
                .trim()
                .chars()
                .take(500)
                .collect();
            return Err(McpError::Http {
                server: self.name.clone(),
                status,
                detail,
            });
        }
        Ok(response)
    }

    /// Reads an SSE response until the frame that answers `want_id`.
    async fn read_stream(
        &self,
        response: &mut HttpResponse,
        want_id: u64,
    ) -> Result<Response, McpError> {
        let mut parser = SseParser::default();
        let mut read = 0;
        loop {
            let chunk = response
                .chunk()
                .await
                .map_err(|error| self.transport(error.to_string()))?;
            let (frames, ended) = match chunk {
                Some(bytes) => {
                    read += bytes.len();
                    if read > MAX_BODY_BYTES {
                        return Err(self.too_large());
                    }
                    (parser.push(&bytes), false)
                }
                None => (parser.finish().into_iter().collect(), true),
            };
            for frame in frames {
                match parse_incoming(&frame.data) {
                    Ok(Incoming::Response(answer)) if answer.numeric_id() == Some(want_id) => {
                        return Ok(answer);
                    }
                    Ok(Incoming::ServerRequest { id, method }) => {
                        self.refuse_server_request(id, &method).await;
                    }
                    _ => {}
                }
            }
            if ended {
                break;
            }
        }
        Err(self.protocol(format!(
            "event stream ended without a response to request {want_id}"
        )))
    }

    /// Reads a whole response body, up to [`MAX_BODY_BYTES`].
    async fn read_body(&self, response: &mut HttpResponse) -> Result<Vec<u8>, McpError> {
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| self.transport(error.to_string()))?
        {
            body.extend_from_slice(&chunk);
            if body.len() > MAX_BODY_BYTES {
                return Err(self.too_large());
            }
        }
        Ok(body)
    }

    /// Answers a server-to-client request with "method not found".
    async fn refuse_server_request(&self, id: Value, method: &str) {
        let reply = json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": METHOD_NOT_FOUND, "message": format!("anvil does not support `{method}`")}
        });
        if let Err(error) = self.send(&reply).await {
            tracing::debug!(server = self.name, %error, "could not refuse a server request");
        }
    }

    /// Turns a send failure into an MCP error.
    fn send_error(&self, error: SendError) -> McpError {
        match error {
            SendError::Refused(refused) => McpError::Transport {
                server: self.name.clone(),
                detail: refused.to_string(),
            },
            other => self.transport(other.to_string()),
        }
    }

    /// A transport error for this server.
    fn transport(&self, detail: String) -> McpError {
        McpError::Transport {
            server: self.name.clone(),
            detail,
        }
    }

    /// A protocol error for this server.
    fn protocol(&self, detail: String) -> McpError {
        McpError::Protocol {
            server: self.name.clone(),
            detail,
        }
    }

    /// The error for a response over [`MAX_BODY_BYTES`].
    fn too_large(&self) -> McpError {
        self.protocol(format!("a response exceeded {MAX_BODY_BYTES} bytes"))
    }
}

impl RpcTransport for HttpTransport {
    fn request<'a>(&'a self, method: &'a str, params: Option<Value>) -> RpcFuture<'a, Response> {
        Box::pin(async move {
            let id = self.next_id.fetch_add(1, Ordering::SeqCst);
            let body = serde_json::to_value(Request::new(id, method, params))
                .map_err(|error| self.protocol(error.to_string()))?;
            self.post(&body, Some(id))
                .await?
                .ok_or_else(|| self.protocol(format!("no response body for `{method}`")))
        })
    }

    fn notify<'a>(&'a self, method: &'a str, params: Option<Value>) -> RpcFuture<'a, ()> {
        Box::pin(async move {
            let body = serde_json::to_value(Notification::new(method, params))
                .map_err(|error| self.protocol(error.to_string()))?;
            self.post(&body, None).await.map(|_| ())
        })
    }
}
