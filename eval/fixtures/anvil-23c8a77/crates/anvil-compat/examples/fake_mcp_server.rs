//! A minimal MCP server over stdio, used by the integration tests.
//!
//! It answers `initialize`, `tools/list` (one tool, `echo`) and
//! `tools/call`, and exits when stdin closes. With the environment variable
//! `FAKE_MCP_DIE_ON_CALL` set it exits without answering the first
//! `tools/call`, so the client's "server exited" path can be tested.

use std::io::{BufRead, Write};

use serde_json::{Value, json};

fn main() {
    let die_on_call = std::env::var_os("FAKE_MCP_DIE_ON_CALL").is_some();
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            eprintln!("fake server: not JSON: {line}");
            continue;
        };
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        let Some(id) = message.get("id").filter(|id| !id.is_null()) else {
            // A notification; nothing to answer.
            continue;
        };
        let reply = match method {
            "initialize" => json!({"jsonrpc": "2.0", "id": id, "result": {
                "protocolVersion": "2025-06-18",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "fake-mcp-server", "version": "0.1.0"}
            }}),
            "tools/list" => json!({"jsonrpc": "2.0", "id": id, "result": {"tools": [{
                "name": "echo",
                "description": "Echoes the text back.",
                "inputSchema": {
                    "type": "object",
                    "properties": {"text": {"type": "string", "description": "What to echo."}},
                    "required": ["text"]
                },
                "annotations": {"readOnlyHint": true}
            }]}}),
            "tools/call" => {
                if die_on_call {
                    eprintln!("fake server: dying on purpose");
                    std::process::exit(3);
                }
                let text = message
                    .pointer("/params/arguments/text")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                json!({"jsonrpc": "2.0", "id": id, "result": {
                    "content": [{"type": "text", "text": format!("echo: {text}")}],
                    "isError": false
                }})
            }
            "ping" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
            other => json!({"jsonrpc": "2.0", "id": id, "error": {
                "code": -32601, "message": format!("unknown method `{other}`")
            }}),
        };
        if writeln!(stdout, "{reply}")
            .and_then(|()| stdout.flush())
            .is_err()
        {
            break;
        }
    }
}
