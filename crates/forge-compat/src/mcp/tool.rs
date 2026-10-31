//! MCP tools as forge tools, connecting to their server on first use.

use std::path::PathBuf;
use std::sync::Arc;

use forge_core::tools::{Access, Tool, ToolContext, ToolFuture, ToolOutput};
use serde_json::Value;
use tokio::sync::OnceCell;

use super::McpServer;
use super::client::{McpClient, ToolInfo};
use super::jsonrpc::McpError;

/// A server that connects the first time a tool needs it.
///
/// A failed connection is not remembered, so the next call tries again.
pub struct LazyServer {
    /// The definition.
    server: McpServer,
    /// The working directory for stdio servers.
    cwd: PathBuf,
    /// The connection, once made.
    client: OnceCell<McpClient>,
}

impl LazyServer {
    /// A server that is not connected yet.
    ///
    /// # Arguments
    ///
    /// * `server` - The definition.
    /// * `cwd` - The working directory for stdio servers.
    ///
    /// # Returns
    ///
    /// The lazy server.
    pub fn new(server: McpServer, cwd: PathBuf) -> Self {
        Self {
            server,
            cwd,
            client: OnceCell::new(),
        }
    }

    /// A server that is already connected.
    ///
    /// # Arguments
    ///
    /// * `server` - The definition.
    /// * `cwd` - The working directory for stdio servers.
    /// * `client` - The live connection.
    ///
    /// # Returns
    ///
    /// The server, with no connection to make later.
    pub fn connected(server: McpServer, cwd: PathBuf, client: McpClient) -> Self {
        Self {
            server,
            cwd,
            client: OnceCell::from(client),
        }
    }

    /// The server name.
    ///
    /// # Returns
    ///
    /// The name.
    pub fn name(&self) -> &str {
        &self.server.name
    }

    /// The connection, made on first use.
    ///
    /// # Returns
    ///
    /// The client.
    ///
    /// # Errors
    ///
    /// Any [`McpError`] from connecting.
    pub async fn client(&self) -> Result<&McpClient, McpError> {
        self.client
            .get_or_try_init(|| async {
                tracing::info!(server = self.server.name, "connecting to mcp server");
                McpClient::connect(&self.server, &self.cwd).await
            })
            .await
    }

    /// Whether a connection exists right now.
    ///
    /// # Returns
    ///
    /// `true` after a successful connect.
    pub fn is_connected(&self) -> bool {
        self.client.initialized()
    }
}

/// The registry name for a server's tool.
///
/// # Arguments
///
/// * `server` - The server name.
/// * `tool` - The tool's name on the server.
///
/// # Returns
///
/// `mcp__<server>__<tool>`.
pub fn qualified_name(server: &str, tool: &str) -> String {
    format!("mcp__{server}__{tool}")
}

/// One MCP tool in the registry.
pub struct McpTool {
    /// Its server.
    server: Arc<LazyServer>,
    /// The tool's name on the server.
    remote_name: String,
    /// The registry name.
    name: String,
    /// What it does, prefixed with the server name.
    description: String,
    /// Its argument schema.
    schema: Value,
    /// Whether the server marked it read-only.
    read_only: bool,
}

impl McpTool {
    /// Wraps an advertised tool.
    ///
    /// # Arguments
    ///
    /// * `server` - The server it belongs to.
    /// * `info` - What the server advertised.
    ///
    /// # Returns
    ///
    /// The tool.
    pub fn new(server: Arc<LazyServer>, info: &ToolInfo) -> Self {
        let description = if info.description.trim().is_empty() {
            format!(
                "Tool `{}` of the `{}` MCP server.",
                info.name,
                server.name()
            )
        } else {
            format!("[{} MCP server] {}", server.name(), info.description.trim())
        };
        Self {
            name: qualified_name(server.name(), &info.name),
            remote_name: info.name.clone(),
            description,
            schema: info.input_schema.clone(),
            read_only: info.read_only,
            server,
        }
    }
}

impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> Value {
        self.schema.clone()
    }

    /// Read when the server says so; otherwise Write, so the permission
    /// model asks before the first call unless a rule allows it.
    fn access(&self) -> Access {
        if self.read_only {
            Access::Read
        } else {
            Access::Write
        }
    }

    fn target(&self, _args: &Value) -> Option<String> {
        None
    }

    fn run<'a>(&'a self, args: Value, _ctx: &'a ToolContext) -> ToolFuture<'a> {
        Box::pin(async move {
            let client = match self.server.client().await {
                Ok(client) => client,
                Err(error) => return ToolOutput::error(error.to_string()),
            };
            match client.call_tool(&self.remote_name, args).await {
                Ok(result) if result.is_error => ToolOutput::error(result.text),
                Ok(result) => ToolOutput::ok(result.text),
                Err(error) => ToolOutput::error(error.to_string()),
            }
        })
    }
}
