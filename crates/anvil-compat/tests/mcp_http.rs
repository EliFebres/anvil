//! The streamable HTTP MCP transport against a mock server: handshake,
//! session and protocol headers, JSON and SSE answers, the configured
//! headers, and the egress rules applied before anything is sent.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use anvil_compat::mcp::{McpClient, McpServer, McpStore, McpTools, ServerSet, Transport};
use anvil_core::net::{Egress, HttpClient, http_client};
use serde_json::json;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// An HTTP server definition for `url` with one bearer header.
fn http_server(url: &str) -> McpServer {
    McpServer {
        name: "remote".to_owned(),
        transport: Transport::Http {
            url: url.to_owned(),
            headers: BTreeMap::from([("Authorization".to_owned(), "Bearer t0k3n".to_owned())]),
        },
        timeout: Duration::from_secs(10),
        source: PathBuf::from("test"),
    }
}

/// A client allowing exactly `endpoints`, as the wiring builds one.
fn client(endpoints: &[String]) -> HttpClient {
    http_client(
        &Egress::personal(endpoints.iter(), Vec::new()),
        Duration::from_secs(5),
    )
    .expect("client")
}

/// Mounts a server that answers the handshake with a session id, lists one
/// tool over an SSE stream and answers a call with JSON.
async fn mount_server(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(body_partial_json(json!({"method": "initialize"})))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("mcp-session-id", "s-1")
                .set_body_json(json!({
                    "jsonrpc": "2.0", "id": 1,
                    "result": {"protocolVersion": "2025-06-18", "capabilities": {}, "serverInfo": {"name": "mock"}}
                })),
        )
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(body_partial_json(
            json!({"method": "notifications/initialized"}),
        ))
        .and(header("mcp-session-id", "s-1"))
        .respond_with(ResponseTemplate::new(202))
        .mount(server)
        .await;
    let stream = concat!(
        "event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{}}\n\n",
        "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"tools\":[{\"name\":\"echo\",\"description\":\"Echoes.\",\"inputSchema\":{\"type\":\"object\"}}]}}\n\n",
    );
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(body_partial_json(json!({"method": "tools/list"})))
        .and(header("mcp-session-id", "s-1"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(stream, "text/event-stream"))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .and(body_partial_json(json!({"method": "tools/call"})))
        .and(header("mcp-session-id", "s-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0", "id": 3,
            "result": {"content": [{"type": "text", "text": "echo: hi"}]}
        })))
        .mount(server)
        .await;
}

#[tokio::test]
async fn an_http_server_round_trips_through_the_one_client() {
    let server = MockServer::start().await;
    mount_server(&server).await;
    let url = format!("{}/mcp", server.uri());
    let http = client(std::slice::from_ref(&url));
    let project = tempfile::tempdir().expect("tempdir");

    let mcp = McpClient::connect(&http_server(&url), project.path(), &http)
        .await
        .expect("connects");
    assert_eq!(mcp.server_info(), Some(&json!({"name": "mock"})));
    let tools = mcp.list_tools().await.expect("lists");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "echo");
    let result = mcp
        .call_tool("echo", json!({"text": "hi"}))
        .await
        .expect("calls");
    assert_eq!(result.text, "echo: hi");

    let requests = server.received_requests().await.expect("recorded");
    assert_eq!(requests.len(), 4, "initialize, initialized, list, call");
    for request in &requests {
        let get = |name: &str| {
            request
                .headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        };
        assert_eq!(get("authorization").as_deref(), Some("Bearer t0k3n"));
        assert_eq!(get("mcp-protocol-version").as_deref(), Some("2025-06-18"));
        assert_eq!(
            get("accept").as_deref(),
            Some("application/json, text/event-stream")
        );
    }
}

#[tokio::test]
async fn an_http_server_outside_the_allowed_set_receives_nothing() {
    let server = MockServer::start().await;
    mount_server(&server).await;
    let url = format!("{}/mcp", server.uri());
    let elsewhere = client(&["https://models.example/v1".to_owned()]);
    let project = tempfile::tempdir().expect("tempdir");
    let set = ServerSet::from_servers(vec![http_server(&url)]);
    let store = McpStore::new(None, project.path());
    let tools = McpTools::load(&set, project.path(), &elsewhere, true, &store).await;
    let error = tools.reports()[0].tools.clone().expect_err("refused");
    assert!(error.to_string().contains("is not allowed"), "{error}");
    let managed = http_client(
        &Egress::managed(&["models.corp.example:443".to_owned()], true),
        Duration::from_secs(5),
    )
    .expect("client");
    let tools = McpTools::load(&set, project.path(), &managed, true, &store).await;
    assert!(tools.reports()[0].tools.is_err());
    assert!(
        server
            .received_requests()
            .await
            .expect("recorded")
            .is_empty(),
        "no request reached the server"
    );
}

#[tokio::test]
async fn a_redirecting_http_server_does_not_pass_its_headers_on() {
    let server = MockServer::start().await;
    let other = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(307).insert_header("location", format!("{}/mcp", other.uri())),
        )
        .mount(&server)
        .await;
    let url = format!("{}/mcp", server.uri());
    let http = client(&[url.clone(), other.uri()]);
    let project = tempfile::tempdir().expect("tempdir");
    let error = McpClient::connect(&http_server(&url), project.path(), &http)
        .await
        .err()
        .expect("a redirect is an error");
    assert!(error.to_string().contains("307"), "{error}");
    assert!(
        other
            .received_requests()
            .await
            .expect("recorded")
            .is_empty(),
        "the redirect target received nothing"
    );
}

#[tokio::test]
async fn a_server_that_echoes_its_bearer_token_in_an_error_body_leaks_nothing() {
    let token = "Bearer t0k3n";
    let server = MockServer::start().await;
    mount_server(&server).await;
    Mock::given(method("POST"))
        .and(path("/bad"))
        .respond_with(ResponseTemplate::new(401).set_body_string(format!(
            "invalid credentials: got '{token}' (token t0k3n) and sk-{}",
            "x".repeat(24)
        )))
        .mount(&server)
        .await;
    let good = format!("{}/mcp", server.uri());
    let bad = format!("{}/bad", server.uri());
    let http = client(&[good.clone(), bad.clone()]);
    let project = tempfile::tempdir().expect("tempdir");

    let error = McpClient::connect(&http_server(&bad), project.path(), &http)
        .await
        .err()
        .expect("401 is an error")
        .to_string();
    assert!(error.contains("401"), "{error}");
    assert!(!error.contains("t0k3n"), "the header value leaked: {error}");
    assert!(!error.contains("sk-xxxx"), "a key shape leaked: {error}");
    assert!(error.contains("[REDACTED:header]"), "{error}");

    let set = ServerSet::from_servers(vec![http_server(&bad)]);
    let store = McpStore::new(None, project.path());
    let tools = McpTools::load(&set, project.path(), &http, true, &store).await;
    let report = tools.reports()[0].tools.clone().expect_err("refused");
    assert!(!report.to_string().contains("t0k3n"), "{report}");
}
