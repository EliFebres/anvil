//! JSON-RPC 2.0 messages and the MCP error type.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

/// The MCP protocol revision forge speaks.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// The JSON-RPC error code for an unknown method.
pub const METHOD_NOT_FOUND: i64 = -32601;

/// A request that expects a response.
#[derive(Debug, Clone, Serialize)]
pub struct Request {
    /// Always `"2.0"`.
    pub jsonrpc: &'static str,
    /// Matches the response.
    pub id: u64,
    /// The method name.
    pub method: String,
    /// The parameters, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl Request {
    /// Builds a request.
    ///
    /// # Arguments
    ///
    /// * `id` - The request id.
    /// * `method` - The method name.
    /// * `params` - The parameters, if any.
    ///
    /// # Returns
    ///
    /// The request.
    pub fn new(id: u64, method: &str, params: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            method: method.to_owned(),
            params,
        }
    }
}

/// A message that expects no response.
#[derive(Debug, Clone, Serialize)]
pub struct Notification {
    /// Always `"2.0"`.
    pub jsonrpc: &'static str,
    /// The method name.
    pub method: String,
    /// The parameters, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl Notification {
    /// Builds a notification.
    ///
    /// # Arguments
    ///
    /// * `method` - The method name.
    /// * `params` - The parameters, if any.
    ///
    /// # Returns
    ///
    /// The notification.
    pub fn new(method: &str, params: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0",
            method: method.to_owned(),
            params,
        }
    }
}

/// An error object inside a response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RpcError {
    /// The error code.
    pub code: i64,
    /// The message.
    pub message: String,
    /// Extra detail, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// A response to one of our requests.
#[derive(Debug, Clone, Deserialize)]
pub struct Response {
    /// The request id it answers.
    pub id: Value,
    /// The result on success.
    #[serde(default)]
    pub result: Option<Value>,
    /// The error on failure.
    #[serde(default)]
    pub error: Option<RpcError>,
}

impl Response {
    /// The numeric id, when the server echoed ours.
    ///
    /// # Returns
    ///
    /// The id.
    pub fn numeric_id(&self) -> Option<u64> {
        match &self.id {
            Value::Number(number) => number.as_u64(),
            Value::String(text) => text.parse().ok(),
            _ => None,
        }
    }
}

/// Any message a server may send.
#[derive(Debug, Clone)]
pub enum Incoming {
    /// A response to one of our requests.
    Response(Response),
    /// A request from the server that wants an answer.
    ServerRequest {
        /// Its id, to echo in the reply.
        id: Value,
        /// The method it asked for.
        method: String,
    },
    /// A notification from the server.
    Notification {
        /// The method name.
        method: String,
    },
}

/// Classifies one JSON-RPC message.
///
/// # Arguments
///
/// * `text` - One JSON document.
///
/// # Returns
///
/// The message.
///
/// # Errors
///
/// A description when the text is not a JSON-RPC message.
pub fn parse_incoming(text: &str) -> Result<Incoming, String> {
    let value: Value = serde_json::from_str(text).map_err(|error| error.to_string())?;
    let Some(object) = value.as_object() else {
        return Err("not a JSON object".to_owned());
    };
    match (object.get("method"), object.get("id")) {
        (Some(method), Some(id)) if !id.is_null() => Ok(Incoming::ServerRequest {
            id: id.clone(),
            method: method.as_str().unwrap_or_default().to_owned(),
        }),
        (Some(method), _) => Ok(Incoming::Notification {
            method: method.as_str().unwrap_or_default().to_owned(),
        }),
        (None, Some(_)) => serde_json::from_value(value)
            .map(Incoming::Response)
            .map_err(|error| error.to_string()),
        (None, None) => Err("neither a request nor a response".to_owned()),
    }
}

/// Why an MCP server could not be configured, reached or used.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum McpError {
    /// A server definition could not be read.
    #[error("{}: {detail}", path.display())]
    Config {
        /// The file.
        path: PathBuf,
        /// What was wrong.
        detail: String,
    },

    /// The server process could not be started.
    #[error("mcp server `{server}`: cannot start `{command}`: {detail}")]
    Spawn {
        /// The server name.
        server: String,
        /// The command that failed.
        command: String,
        /// The operating system's message.
        detail: String,
    },

    /// The server process ended while we still needed it.
    #[error("mcp server `{server}` exited{}", stderr_tail.as_deref().map(|tail| format!("; its last output was:\n{tail}")).unwrap_or_default())]
    ServerExited {
        /// The server name.
        server: String,
        /// The last lines it wrote to stderr, if any.
        stderr_tail: Option<String>,
    },

    /// Bytes could not be sent or received.
    #[error("mcp server `{server}`: transport error: {detail}")]
    Transport {
        /// The server name.
        server: String,
        /// What happened.
        detail: String,
    },

    /// The server took too long to answer.
    #[error("mcp server `{server}`: `{method}` timed out after {secs}s")]
    Timeout {
        /// The server name.
        server: String,
        /// The method that was waiting.
        method: String,
        /// The timeout that applied.
        secs: u64,
    },

    /// The server answered with a JSON-RPC error.
    #[error("mcp server `{server}`: `{method}` failed with code {code}: {message}")]
    Rpc {
        /// The server name.
        server: String,
        /// The method.
        method: String,
        /// The error code.
        code: i64,
        /// The error message.
        message: String,
    },

    /// The server sent something outside the protocol.
    #[error("mcp server `{server}`: protocol error: {detail}")]
    Protocol {
        /// The server name.
        server: String,
        /// What was wrong.
        detail: String,
    },

    /// An HTTP server answered with a failure status.
    #[error("mcp server `{server}`: HTTP {status}: {detail}")]
    Http {
        /// The server name.
        server: String,
        /// The status code.
        status: u16,
        /// The response body, trimmed.
        detail: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_messages() {
        assert!(matches!(
            parse_incoming(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#),
            Ok(Incoming::Response(response)) if response.numeric_id() == Some(1)
        ));
        assert!(matches!(
            parse_incoming(r#"{"jsonrpc":"2.0","id":"7","error":{"code":-1,"message":"no"}}"#),
            Ok(Incoming::Response(response)) if response.numeric_id() == Some(7) && response.error.is_some()
        ));
        assert!(matches!(
            parse_incoming(r#"{"jsonrpc":"2.0","id":3,"method":"roots/list"}"#),
            Ok(Incoming::ServerRequest { method, .. }) if method == "roots/list"
        ));
        assert!(matches!(
            parse_incoming(r#"{"jsonrpc":"2.0","method":"notifications/progress"}"#),
            Ok(Incoming::Notification { method }) if method == "notifications/progress"
        ));
        assert!(parse_incoming("[]").is_err());
        assert!(parse_incoming("nope").is_err());
    }

    #[test]
    fn requests_serialize_without_null_params() {
        let text = serde_json::to_string(&Request::new(1, "ping", None)).expect("json");
        assert_eq!(text, r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#);
    }
}
