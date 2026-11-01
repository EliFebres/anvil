//! MCP round trips against the fake stdio server in `examples/`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use anvil_compat::mcp::{McpError, McpServer, McpTools, ServerSet, Transport};
use anvil_core::provider::ToolCall;
use anvil_core::tools::{Permissions, PromptAnswer, StaticPrompt, ToolOutcome, ToolRegistry};
use serde_json::json;

/// The fake server binary, built by `cargo test` next to the test binaries.
fn fake_server_path() -> PathBuf {
    let exe = std::env::current_exe().expect("current exe");
    let debug_dir = exe
        .parent()
        .and_then(|deps| deps.parent())
        .expect("target/debug");
    let name = if cfg!(windows) {
        "fake_mcp_server.exe"
    } else {
        "fake_mcp_server"
    };
    let path = debug_dir.join("examples").join(name);
    assert!(
        path.is_file(),
        "fake server not built at {}; cargo test builds examples",
        path.display()
    );
    path
}

fn fake_server(env: BTreeMap<String, String>) -> McpServer {
    McpServer {
        name: "fake".to_owned(),
        transport: Transport::Stdio {
            command: fake_server_path().to_string_lossy().into_owned(),
            args: Vec::new(),
            env,
        },
        timeout: Duration::from_secs(10),
        source: PathBuf::from("test"),
    }
}

fn fresh_registry() -> ToolRegistry {
    ToolRegistry::new(
        Permissions::empty(),
        Box::new(StaticPrompt(PromptAnswer::Yes)),
    )
}

fn echo_call(text: &str) -> ToolCall {
    ToolCall {
        id: "c1".to_owned(),
        name: "mcp__fake__echo".to_owned(),
        arguments_json: json!({"text": text}).to_string(),
    }
}

#[tokio::test]
async fn registers_the_servers_tools_and_round_trips_a_call() {
    let project = tempfile::tempdir().expect("tempdir");
    let set = ServerSet::from_servers(vec![fake_server(BTreeMap::new())]);

    let tools = McpTools::load(&set, project.path(), false).await;
    assert_eq!(tools.reports().len(), 1);
    assert_eq!(tools.reports()[0].tools, Ok(1));
    assert!(!tools.reports()[0].from_cache);
    assert_eq!(tools.names(), ["mcp__fake__echo"]);

    let mut registry = fresh_registry();
    tools.register(&mut registry);
    assert_eq!(registry.names(), ["mcp__fake__echo"]);
    let spec = registry
        .specs()
        .into_iter()
        .find(|spec| spec.name == "mcp__fake__echo")
        .expect("spec");
    assert_eq!(spec.parameters["required"], json!(["text"]));
    assert!(spec.description.contains("Echoes the text back."));

    let ctx = anvil_core::tools::ToolContext::new(project.path());
    let execution = registry.execute(&echo_call("hi there"), &ctx).await;
    assert_eq!(execution.outcome, ToolOutcome::Completed);
    assert_eq!(execution.result.content, "echo: hi there");

    // The second load registers from the cache without connecting, and the
    // first call connects lazily.
    let cached = McpTools::load(&set, project.path(), false).await;
    assert!(cached.reports()[0].from_cache);
    let mut registry = fresh_registry();
    cached.register(&mut registry);
    let execution = registry.execute(&echo_call("again"), &ctx).await;
    assert_eq!(execution.result.content, "echo: again");
}

#[tokio::test]
async fn a_server_that_dies_mid_call_gives_a_clear_error() {
    let project = tempfile::tempdir().expect("tempdir");
    let env = BTreeMap::from([("FAKE_MCP_DIE_ON_CALL".to_owned(), "1".to_owned())]);
    let set = ServerSet::from_servers(vec![fake_server(env)]);
    let tools = McpTools::load(&set, project.path(), true).await;
    assert_eq!(tools.reports()[0].tools, Ok(1));

    let mut registry = fresh_registry();
    tools.register(&mut registry);
    let ctx = anvil_core::tools::ToolContext::new(project.path());
    let execution = registry.execute(&echo_call("boom"), &ctx).await;
    assert_eq!(execution.outcome, ToolOutcome::Failed);
    assert!(
        execution
            .result
            .content
            .contains("mcp server `fake` exited"),
        "{}",
        execution.result.content
    );
    assert!(
        execution.result.content.contains("dying on purpose"),
        "{}",
        execution.result.content
    );
}

#[tokio::test]
async fn a_missing_command_is_reported_without_stopping_the_others() {
    let project = tempfile::tempdir().expect("tempdir");
    let missing = McpServer {
        name: "ghost".to_owned(),
        transport: Transport::Stdio {
            command: "anvil-definitely-not-a-program".to_owned(),
            args: Vec::new(),
            env: BTreeMap::new(),
        },
        timeout: Duration::from_secs(5),
        source: PathBuf::from("test"),
    };
    let set = ServerSet::from_servers(vec![missing, fake_server(BTreeMap::new())]);
    let tools = McpTools::load(&set, project.path(), true).await;
    assert!(matches!(
        tools.reports()[0].tools,
        Err(McpError::Spawn { .. })
    ));
    assert_eq!(tools.reports()[1].tools, Ok(1));
    assert_eq!(tools.names(), ["mcp__fake__echo"]);
}
