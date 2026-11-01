//! The MCP client: handshake, tool listing and tool calls over a transport.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::http::HttpTransport;
use super::jsonrpc::{McpError, PROTOCOL_VERSION, Response};
use super::stdio::StdioTransport;
use super::{McpServer, Transport};

/// A boxed future returned by a transport.
pub type RpcFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, McpError>> + Send + 'a>>;

/// How long the handshake may take.
const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(30);

/// Something that carries JSON-RPC messages to one server.
pub trait RpcTransport: Send + Sync {
    /// Sends a request and waits for its response.
    ///
    /// # Arguments
    ///
    /// * `method` - The method name.
    /// * `params` - The parameters, if any.
    ///
    /// # Returns
    ///
    /// The response, which may carry an error object.
    fn request<'a>(&'a self, method: &'a str, params: Option<Value>) -> RpcFuture<'a, Response>;

    /// Sends a notification.
    ///
    /// # Arguments
    ///
    /// * `method` - The method name.
    /// * `params` - The parameters, if any.
    ///
    /// # Returns
    ///
    /// Nothing; delivery is best effort.
    fn notify<'a>(&'a self, method: &'a str, params: Option<Value>) -> RpcFuture<'a, ()>;
}

/// One tool a server advertises.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolInfo {
    /// The tool's name on the server.
    pub name: String,
    /// What it does.
    #[serde(default)]
    pub description: String,
    /// JSON Schema of its arguments.
    #[serde(default = "empty_object_schema")]
    pub input_schema: Value,
    /// Whether the server marked it read-only (`annotations.readOnlyHint`).
    #[serde(default)]
    pub read_only: bool,
}

/// `{"type": "object", "properties": {}}`.
fn empty_object_schema() -> Value {
    json!({"type": "object", "properties": {}})
}

/// What a tool call returned, flattened to text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallResult {
    /// The content parts joined with blank lines.
    pub text: String,
    /// Whether the server flagged the result as an error.
    pub is_error: bool,
}

/// A connected server.
pub struct McpClient {
    /// The server name.
    name: String,
    /// The wire.
    transport: Box<dyn RpcTransport>,
    /// Per-request timeout.
    timeout: Duration,
    /// What the server said about itself.
    server_info: Option<Value>,
}

impl McpClient {
    /// Opens the transport and performs the `initialize` handshake.
    ///
    /// # Arguments
    ///
    /// * `server` - The server definition.
    /// * `cwd` - The working directory for stdio servers.
    ///
    /// # Returns
    ///
    /// The ready client.
    ///
    /// # Errors
    ///
    /// Any [`McpError`] from spawning, connecting or the handshake.
    pub async fn connect(server: &McpServer, cwd: &std::path::Path) -> Result<Self, McpError> {
        let transport: Box<dyn RpcTransport> = match &server.transport {
            Transport::Stdio { command, args, env } => Box::new(StdioTransport::spawn(
                &server.name,
                command,
                args,
                env,
                cwd,
            )?),
            Transport::Http { url, headers } => {
                Box::new(HttpTransport::new(&server.name, url, headers)?)
            }
        };
        let mut client = Self {
            name: server.name.clone(),
            transport,
            timeout: server.timeout,
            server_info: None,
        };
        let params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {"name": "anvil", "version": env!("CARGO_PKG_VERSION")}
        });
        let result = client
            .call_with_timeout("initialize", Some(params), INITIALIZE_TIMEOUT)
            .await?;
        client.server_info = result.get("serverInfo").cloned();
        client
            .transport
            .notify("notifications/initialized", None)
            .await?;
        Ok(client)
    }

    /// The server name.
    ///
    /// # Returns
    ///
    /// The name from the configuration.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// What the server reported in `serverInfo`, if anything.
    ///
    /// # Returns
    ///
    /// The object.
    pub fn server_info(&self) -> Option<&Value> {
        self.server_info.as_ref()
    }

    /// Lists every tool, following pagination.
    ///
    /// # Returns
    ///
    /// The tools in server order.
    ///
    /// # Errors
    ///
    /// Any [`McpError`] from the transport or the server.
    pub async fn list_tools(&self) -> Result<Vec<ToolInfo>, McpError> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = cursor.as_ref().map(|cursor| json!({"cursor": cursor}));
            let result = self.call("tools/list", params).await?;
            let page = result
                .get("tools")
                .and_then(Value::as_array)
                .ok_or_else(|| McpError::Protocol {
                    server: self.name.clone(),
                    detail: "tools/list result has no `tools` array".to_owned(),
                })?;
            for tool in page {
                let Some(name) = tool.get("name").and_then(Value::as_str) else {
                    continue;
                };
                tools.push(ToolInfo {
                    name: name.to_owned(),
                    description: tool
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    input_schema: tool
                        .get("inputSchema")
                        .cloned()
                        .filter(Value::is_object)
                        .unwrap_or_else(empty_object_schema),
                    read_only: tool
                        .get("annotations")
                        .and_then(|a| a.get("readOnlyHint"))
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                });
            }
            cursor = result
                .get("nextCursor")
                .and_then(Value::as_str)
                .filter(|cursor| !cursor.is_empty())
                .map(str::to_owned);
            if cursor.is_none() {
                return Ok(tools);
            }
        }
    }

    /// Calls a tool.
    ///
    /// # Arguments
    ///
    /// * `tool` - The tool's name on the server.
    /// * `arguments` - The arguments object.
    ///
    /// # Returns
    ///
    /// The result flattened to text.
    ///
    /// # Errors
    ///
    /// Any [`McpError`] from the transport or the server. A tool-level
    /// failure (`isError`) is not an error here; it is a result.
    pub async fn call_tool(&self, tool: &str, arguments: Value) -> Result<CallResult, McpError> {
        let result = self
            .call(
                "tools/call",
                Some(json!({"name": tool, "arguments": arguments})),
            )
            .await?;
        Ok(flatten_result(&result))
    }

    /// Sends a request with the configured timeout and unwraps the result.
    async fn call(&self, method: &str, params: Option<Value>) -> Result<Value, McpError> {
        self.call_with_timeout(method, params, self.timeout).await
    }

    /// Sends a request and unwraps the result or the error object.
    async fn call_with_timeout(
        &self,
        method: &str,
        params: Option<Value>,
        timeout: Duration,
    ) -> Result<Value, McpError> {
        let response = tokio::time::timeout(timeout, self.transport.request(method, params))
            .await
            .map_err(|_| McpError::Timeout {
                server: self.name.clone(),
                method: method.to_owned(),
                secs: timeout.as_secs(),
            })??;
        if let Some(error) = response.error {
            return Err(McpError::Rpc {
                server: self.name.clone(),
                method: method.to_owned(),
                code: error.code,
                message: error.message,
            });
        }
        Ok(response.result.unwrap_or(Value::Null))
    }
}

/// Turns a `tools/call` result into text the model can read.
fn flatten_result(result: &Value) -> CallResult {
    let mut parts = Vec::new();
    for item in result
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match item.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = item.get("text").and_then(Value::as_str) {
                    parts.push(text.to_owned());
                }
            }
            Some("image") | Some("audio") => parts.push(format!(
                "[{} {}]",
                item.get("type").and_then(Value::as_str).unwrap_or("media"),
                item.get("mimeType")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown type")
            )),
            Some("resource") => {
                let resource = item.get("resource").cloned().unwrap_or(Value::Null);
                match resource.get("text").and_then(Value::as_str) {
                    Some(text) => parts.push(text.to_owned()),
                    None => parts.push(format!(
                        "[resource {}]",
                        resource.get("uri").and_then(Value::as_str).unwrap_or("?")
                    )),
                }
            }
            Some("resource_link") => parts.push(format!(
                "[resource link {}]",
                item.get("uri").and_then(Value::as_str).unwrap_or("?")
            )),
            _ => parts.push(item.to_string()),
        }
    }
    if parts.is_empty()
        && let Some(structured) = result.get("structuredContent")
    {
        parts.push(serde_json::to_string_pretty(structured).unwrap_or_default());
    }
    let mut text = parts.join("\n\n");
    if text.is_empty() {
        text.push_str("(no content)");
    }
    CallResult {
        text,
        is_error: result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flattens_content_parts() {
        let result = json!({
            "content": [
                {"type": "text", "text": "hello"},
                {"type": "image", "mimeType": "image/png", "data": "..."},
                {"type": "resource", "resource": {"uri": "file:///x", "text": "inline"}},
                {"type": "resource_link", "uri": "file:///y"}
            ],
            "isError": true
        });
        let flat = flatten_result(&result);
        assert_eq!(
            flat.text,
            "hello\n\n[image image/png]\n\ninline\n\n[resource link file:///y]"
        );
        assert!(flat.is_error);

        let structured = flatten_result(&json!({"structuredContent": {"a": 1}}));
        assert!(structured.text.contains("\"a\": 1"));
        assert_eq!(flatten_result(&json!({})).text, "(no content)");
    }
}
