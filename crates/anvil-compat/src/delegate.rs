//! The `delegate` tool: runs a subagent in a fresh loop and returns only
//! its final message.
//!
//! A delegated run is a plain turn loop on
//! [`anvil_core::agent::turn::complete_turn`]: the agent's body is the
//! system prompt, its allow list filters the tools, and the project's
//! permission rules apply with nobody at the keyboard (an `ask` decision is
//! a `no`, unless the agent says `permissions: inherit`). The parent
//! conversation sees one tool result, nothing else; the model turns the run
//! made travel with it so the loop can count them as worker spend.

use std::collections::BTreeSet;
use std::sync::Arc;

use anvil_core::agent::EventSink;
use anvil_core::agent::turn::complete_turn;
use anvil_core::context::Conversation;
use anvil_core::provider::{
    CompletionRequest, Message, Provider, StopReason, ToolResult, ToolSpec,
};
use anvil_core::tools::{
    Access, ModelTurn, Tool, ToolContext, ToolFuture, ToolOutput, ToolRegistry, parse_args,
    schema_for,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::agents::{AgentSet, SubagentDef};

/// Most model turns one delegated run may take.
pub const MAX_TURNS: usize = 50;

/// What a delegated run needs from the host: providers, tools and paths.
///
/// The CLI implements this on its wiring so anvil-compat never reads
/// config or builds providers itself.
pub trait SubagentEnv: Send + Sync {
    /// The provider for an agent's `model`.
    ///
    /// # Arguments
    ///
    /// * `model` - The agent's `model` key: a role or provider name, or
    ///   `None` for the coder.
    ///
    /// # Returns
    ///
    /// The provider.
    ///
    /// # Errors
    ///
    /// A message for the model when no provider can be built.
    fn provider(&self, model: Option<&str>) -> Result<Arc<dyn Provider>, String>;

    /// A fresh registry with the project's permission rules, without the
    /// `delegate` tool.
    ///
    /// # Arguments
    ///
    /// * `ask_user` - Whether an `ask` decision prompts the user as in the
    ///   main session; otherwise the prompt answers `no`.
    ///
    /// # Returns
    ///
    /// The registry.
    ///
    /// # Errors
    ///
    /// A message for the model when the permissions file cannot be read.
    fn registry(&self, ask_user: bool) -> Result<ToolRegistry, String>;

    /// Where the subagent's tools run.
    ///
    /// # Returns
    ///
    /// The context.
    fn tool_context(&self) -> ToolContext;

    /// The context window to compact against, in tokens.
    ///
    /// # Returns
    ///
    /// Tokens.
    fn context_window(&self) -> u32;
}

/// Arguments of the `delegate` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct DelegateArgs {
    /// The subagent's name, from the system prompt's subagent list.
    pub agent: String,
    /// A self-contained description of what to do and what to report back.
    pub task: String,
}

/// Hands a task to a subagent and returns its final message.
pub struct DelegateTool {
    /// The agents on offer.
    agents: Arc<AgentSet>,
    /// How to build providers and registries.
    env: Arc<dyn SubagentEnv>,
}

impl DelegateTool {
    /// Builds the tool.
    ///
    /// # Arguments
    ///
    /// * `agents` - The loaded agents.
    /// * `env` - The host's providers, tools and paths.
    ///
    /// # Returns
    ///
    /// The tool.
    pub fn new(agents: Arc<AgentSet>, env: Arc<dyn SubagentEnv>) -> Self {
        Self { agents, env }
    }
}

impl Tool for DelegateTool {
    fn name(&self) -> &str {
        "delegate"
    }

    fn description(&self) -> &str {
        "Runs a subagent from the system prompt's subagent list on a self-contained task \
         and returns only its final message. The subagent starts with no memory of this \
         conversation, so the task must say what to do, where to look and what to report."
    }

    fn schema(&self) -> Value {
        schema_for::<DelegateArgs>()
    }

    /// Read access: the subagent's own calls are still governed by the
    /// permission rules, and nothing is written by the delegation itself.
    fn access(&self) -> Access {
        Access::Read
    }

    /// Whether the target agent can only read, so the call may overlap
    /// others. An agent without a tool list may write, so it never does.
    fn parallel_safe(&self, args: &Value) -> bool {
        let Some(agent) = args
            .get("agent")
            .and_then(Value::as_str)
            .and_then(|name| self.agents.get(name.trim()))
        else {
            return false;
        };
        let Some(tools) = &agent.tools else {
            return false;
        };
        let Ok(registry) = self.env.registry(false) else {
            return false;
        };
        tools
            .iter()
            .filter_map(|name| registry.get(name))
            .all(|tool| tool.access() == Access::Read)
    }

    fn target(&self, args: &Value) -> Option<String> {
        args.get("agent").and_then(Value::as_str).map(str::to_owned)
    }

    fn run<'a>(&'a self, args: Value, _ctx: &'a ToolContext) -> ToolFuture<'a> {
        Box::pin(async move {
            let args: DelegateArgs = match parse_args(args) {
                Ok(args) => args,
                Err(output) => return output,
            };
            let Some(agent) = self.agents.get(args.agent.trim()) else {
                return ToolOutput::error(format!(
                    "unknown subagent `{}`; available: {}",
                    args.agent,
                    self.agents.names().join(", ")
                ));
            };
            let agent = agent.clone();
            let env = Arc::clone(&self.env);
            let handle =
                tokio::spawn(async move { run_subagent(&agent, &args.task, env.as_ref()).await });
            match handle.await {
                Ok(run) => {
                    let mut output = match run.result {
                        Ok(text) => ToolOutput::ok(text),
                        Err(reason) => ToolOutput::error(reason),
                    };
                    output.model_turns = run.turns;
                    output
                }
                Err(error) => ToolOutput::error(format!("subagent task failed: {error}")),
            }
        })
    }
}

/// How a delegated run ended and what it spent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentRun {
    /// The text of the agent's last turn, or a message for the parent model
    /// when the provider failed (the session budget included), the registry
    /// could not be built, or the run exceeded [`MAX_TURNS`].
    pub result: Result<String, String>,
    /// Every completion the run made, including those of a run that failed.
    pub turns: Vec<ModelTurn>,
}

/// Runs one subagent to completion.
///
/// # Arguments
///
/// * `agent` - The definition.
/// * `task` - What to do.
/// * `env` - Providers, tools and paths.
///
/// # Returns
///
/// The outcome and the completions it took; see [`SubagentRun`].
pub async fn run_subagent(agent: &SubagentDef, task: &str, env: &dyn SubagentEnv) -> SubagentRun {
    let mut turns = Vec::new();
    let result = run_turns(agent, task, env, &mut turns).await;
    SubagentRun { result, turns }
}

/// The turn loop of [`run_subagent`], recording each completion in `turns`.
async fn run_turns(
    agent: &SubagentDef,
    task: &str,
    env: &dyn SubagentEnv,
    turns: &mut Vec<ModelTurn>,
) -> Result<String, String> {
    let provider = env.provider(agent.model.as_deref())?;
    let registry = env.registry(agent.inherit_permissions)?;
    let ctx = env.tool_context();
    let tools: Vec<ToolSpec> = registry
        .specs()
        .into_iter()
        .filter(|spec| agent.allows(&spec.name))
        .collect();
    let allowed: BTreeSet<&str> = tools.iter().map(|spec| spec.name.as_str()).collect();
    let system = system_prompt(agent, &tools);
    let events = EventSink::discard();
    let mut conversation = Conversation::new();
    conversation.push(Message::user(task));
    let session_dir = ctx.session_dir.clone().unwrap_or_else(std::env::temp_dir);

    for _ in 0..MAX_TURNS {
        if conversation.needs_compaction(&system, env.context_window()) {
            match conversation
                .compact(provider.as_ref(), task, &session_dir, 200_000)
                .await
            {
                Ok(Some(compaction)) => turns.push(ModelTurn {
                    stop_reason: StopReason::EndTurn,
                    usage: compaction.usage,
                    latency_ms: 0,
                    retries: 0,
                }),
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(agent = agent.name, %error, "subagent compaction failed");
                }
            }
        }
        let request_len = conversation.messages().len();
        let request = CompletionRequest {
            system: system.clone(),
            messages: conversation.messages().to_vec(),
            tools: tools.clone(),
            max_output_tokens: None,
        };
        let turn = complete_turn(provider.as_ref(), request, &events)
            .await
            .map_err(|error| format!("subagent `{}` provider error: {error}", agent.name))?;
        turns.push(ModelTurn {
            stop_reason: turn.stop_reason.clone(),
            usage: turn.usage,
            latency_ms: turn.latency_ms,
            retries: turn.retries,
        });
        if let Some(usage) = turn.usage {
            conversation.record_usage(request_len, usage);
        }
        conversation.push(turn.message());
        if turn.tool_calls.is_empty() {
            return Ok(turn.text);
        }
        let mut results = Vec::with_capacity(turn.tool_calls.len());
        for call in &turn.tool_calls {
            tracing::info!(agent = agent.name, tool = %call.name, "subagent tool call");
            if allowed.contains(call.name.as_str()) {
                results.push(registry.execute(call, &ctx).await.result);
            } else {
                results.push(ToolResult {
                    call_id: call.id.clone(),
                    content: format!(
                        "tool `{}` is not available to the `{}` subagent",
                        call.name, agent.name
                    ),
                    is_error: true,
                });
            }
        }
        conversation.push(Message::ToolResults { results });
    }
    Err(format!(
        "subagent `{}` did not finish within {MAX_TURNS} turns",
        agent.name
    ))
}

/// The agent's body followed by its tool list.
fn system_prompt(agent: &SubagentDef, tools: &[ToolSpec]) -> String {
    let mut text = agent.body.trim_end().to_owned();
    text.push_str("\n\n# Tools\n\n");
    if tools.is_empty() {
        text.push_str("No tools are available; answer from the task text alone.");
    } else {
        text.push_str("You may call these tools:\n");
        for tool in tools {
            let first_line = tool.description.lines().next().unwrap_or("");
            text.push_str(&format!("\n- {}: {first_line}", tool.name));
        }
    }
    text.push_str(
        "\n\nWhen you are done, reply with your final report as plain text. Only that last \
         message is returned to the caller.",
    );
    text
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use anvil_core::provider::{FakeProvider, ToolCall};
    use anvil_core::tools::{Permissions, PromptAnswer, StaticPrompt};
    use serde_json::{Map, json};

    use super::*;
    use crate::paths::Origin;

    struct FakeEnv {
        provider: Arc<FakeProvider>,
        dir: PathBuf,
    }

    impl SubagentEnv for FakeEnv {
        fn provider(&self, _model: Option<&str>) -> Result<Arc<dyn Provider>, String> {
            Ok(Arc::clone(&self.provider) as Arc<dyn Provider>)
        }

        fn registry(&self, _ask_user: bool) -> Result<ToolRegistry, String> {
            Ok(ToolRegistry::with_builtins(
                Permissions::empty(),
                Box::new(StaticPrompt(PromptAnswer::No)),
            ))
        }

        fn tool_context(&self) -> ToolContext {
            ToolContext::new(&self.dir)
        }

        fn context_window(&self) -> u32 {
            100_000
        }
    }

    fn agent(tools: Option<Vec<&str>>) -> SubagentDef {
        SubagentDef {
            name: "scout".to_owned(),
            description: "Looks around.".to_owned(),
            tools: tools.map(|tools| tools.into_iter().map(str::to_owned).collect()),
            model: None,
            inherit_permissions: false,
            body: "You are a scout.".to_owned(),
            path: PathBuf::from("scout.md"),
            origin: Origin::ProjectClaude,
            extra: Map::new(),
        }
    }

    fn list_dir_call() -> ToolCall {
        ToolCall {
            id: "c1".to_owned(),
            name: "list_dir".to_owned(),
            arguments_json: json!({}).to_string(),
        }
    }

    #[tokio::test]
    async fn returns_only_the_final_message_after_tool_calls() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("hello.txt"), "hi").expect("write");
        let provider = Arc::new(FakeProvider::new(vec![
            FakeProvider::tool_turn(vec![list_dir_call()]),
            FakeProvider::text_turn("Found hello.txt."),
        ]));
        let env = FakeEnv {
            provider: Arc::clone(&provider),
            dir: dir.path().to_path_buf(),
        };
        let run = run_subagent(&agent(Some(vec!["list_dir", "read_file"])), "look", &env).await;
        assert_eq!(run.result.as_deref(), Ok("Found hello.txt."));
        assert_eq!(run.turns.len(), 2, "every completion is reported");

        let requests = provider.requests();
        assert_eq!(requests.len(), 2);
        assert!(
            requests[0]
                .system
                .starts_with("You are a scout.\n\n# Tools")
        );
        let names: Vec<&str> = requests[0].tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["list_dir", "read_file"]);
        let Message::ToolResults { results } = &requests[1].messages[2] else {
            panic!("expected tool results, got {:?}", requests[1].messages[2]);
        };
        assert!(!results[0].is_error);
        assert!(results[0].content.contains("hello.txt"));
    }

    #[tokio::test]
    async fn calls_outside_the_allow_list_are_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let provider = Arc::new(FakeProvider::new(vec![
            FakeProvider::tool_turn(vec![list_dir_call()]),
            FakeProvider::text_turn("ok"),
        ]));
        let env = FakeEnv {
            provider: Arc::clone(&provider),
            dir: dir.path().to_path_buf(),
        };
        run_subagent(&agent(Some(vec!["glob"])), "look", &env)
            .await
            .result
            .expect("runs");
        let requests = provider.requests();
        let Message::ToolResults { results } = &requests[1].messages[2] else {
            panic!("expected tool results");
        };
        assert!(results[0].is_error);
        assert!(
            results[0]
                .content
                .contains("not available to the `scout` subagent")
        );
    }

    #[tokio::test]
    async fn the_tool_rejects_unknown_agents_and_runs_known_ones() {
        let dir = tempfile::tempdir().expect("tempdir");
        let provider = Arc::new(FakeProvider::new(vec![FakeProvider::text_turn("done")]));
        let env = Arc::new(FakeEnv {
            provider,
            dir: dir.path().to_path_buf(),
        });
        let tool = DelegateTool::new(Arc::new(AgentSet::from_agents(vec![agent(None)])), env);
        let ctx = ToolContext::new(dir.path());
        let unknown = tool
            .run(json!({"agent": "nobody", "task": "x"}), &ctx)
            .await;
        assert!(unknown.is_error);
        assert!(unknown.content.contains("available: scout"));
        let known = tool.run(json!({"agent": "scout", "task": "x"}), &ctx).await;
        assert!(!known.is_error);
        assert_eq!(known.content, "done");
    }

    #[test]
    fn only_agents_that_cannot_write_may_overlap() {
        let dir = tempfile::tempdir().expect("tempdir");
        let env = Arc::new(FakeEnv {
            provider: Arc::new(FakeProvider::new(Vec::new())),
            dir: dir.path().to_path_buf(),
        });
        let tool = DelegateTool::new(Arc::new(AgentSet::discover(dir.path(), None)), env);
        let parallel = |agent: &str| tool.parallel_safe(&json!({"agent": agent, "task": "x"}));
        assert!(parallel("explore"), "the built-in explorer only reads");
        assert!(!parallel("implement"), "the built-in implementer writes");
        assert!(!parallel("nobody"), "an unknown agent is not run at all");
        assert!(!tool.parallel_safe(&json!({"task": "x"})));

        let open = DelegateTool::new(
            Arc::new(AgentSet::from_agents(vec![agent(None)])),
            Arc::new(FakeEnv {
                provider: Arc::new(FakeProvider::new(Vec::new())),
                dir: dir.path().to_path_buf(),
            }),
        );
        assert!(
            !open.parallel_safe(&json!({"agent": "scout"})),
            "an agent without a tool list may write"
        );
    }

    #[tokio::test]
    async fn the_tool_output_carries_the_turns_of_the_run() {
        let dir = tempfile::tempdir().expect("tempdir");
        let provider = Arc::new(FakeProvider::new(vec![FakeProvider::text_turn("done")]));
        let env = Arc::new(FakeEnv {
            provider,
            dir: dir.path().to_path_buf(),
        });
        let tool = DelegateTool::new(Arc::new(AgentSet::from_agents(vec![agent(None)])), env);
        let ctx = ToolContext::new(dir.path());
        let output = tool.run(json!({"agent": "scout", "task": "x"}), &ctx).await;
        assert_eq!(output.model_turns.len(), 1);
        assert_eq!(
            output.model_turns[0].usage.map(|usage| usage.total()),
            Some(15)
        );
    }

    #[test]
    fn schema_snapshot() {
        let env: Arc<dyn SubagentEnv> = Arc::new(FakeEnv {
            provider: Arc::new(FakeProvider::new(Vec::new())),
            dir: PathBuf::new(),
        });
        insta::assert_json_snapshot!(DelegateTool::new(Arc::default(), env).schema());
    }
}
