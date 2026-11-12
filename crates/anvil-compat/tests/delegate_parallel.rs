//! Delegations in one model turn through a real agent loop: read-only agents
//! run at the same time, agents that may write run one after another, the
//! results keep call order and the delegated turns count as worker spend.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anvil_compat::{AgentSet, DelegateTool, SubagentEnv};
use anvil_core::agent::{
    AutoApprove, CheckCommand, EventSink, LoopConfig, LoopEvent, LoopProviders, Role, State, Status,
};
use anvil_core::provider::{
    CompletionRequest, EventStream, FakeProvider, Message, Provider, ToolCall,
};
use anvil_core::tools::{Permissions, PromptAnswer, StaticPrompt, ToolContext, ToolRegistry};
use anvil_types::LimitsConfig;
use futures_util::{StreamExt, stream};
use tokio::sync::{Barrier, mpsc};

/// A subagent provider that answers `answer to <task>` after yielding, and
/// optionally waits for another call to arrive first.
struct Gate {
    /// When set, a completion waits here until every party has arrived.
    rendezvous: Option<Arc<Barrier>>,
    /// Completions in flight now.
    inside: Arc<AtomicUsize>,
    /// The most completions that were in flight at once.
    most_inside: Arc<AtomicUsize>,
}

impl Provider for Gate {
    fn complete(&self, request: CompletionRequest) -> EventStream {
        let rendezvous = self.rendezvous.clone();
        let (inside, most_inside) = (Arc::clone(&self.inside), Arc::clone(&self.most_inside));
        let task = match request.messages.first() {
            Some(Message::User { text }) => text.clone(),
            _ => String::new(),
        };
        Box::pin(
            stream::once(async move {
                let now = inside.fetch_add(1, Ordering::SeqCst) + 1;
                most_inside.fetch_max(now, Ordering::SeqCst);
                if let Some(barrier) = rendezvous {
                    // A failure guard only: the barrier opens once both calls are inside.
                    let _ = tokio::time::timeout(Duration::from_secs(30), barrier.wait()).await;
                }
                // The first task finishes last, so completion order differs from call order.
                let yields = if task.contains("first") { 40 } else { 5 };
                for _ in 0..yields {
                    tokio::task::yield_now().await;
                }
                inside.fetch_sub(1, Ordering::SeqCst);
                stream::iter(FakeProvider::text_turn(&format!("answer to {task}")))
            })
            .flatten(),
        )
    }
}

/// Gives subagents the [`Gate`] and a registry with no permissions.
struct Env {
    gate: Arc<Gate>,
    dir: std::path::PathBuf,
}

impl SubagentEnv for Env {
    fn provider(&self, _model: Option<&str>) -> Result<Arc<dyn Provider>, String> {
        Ok(Arc::clone(&self.gate) as Arc<dyn Provider>)
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

fn delegate_call(id: &str, agent: &str, task: &str) -> ToolCall {
    ToolCall {
        id: id.to_owned(),
        name: "delegate".to_owned(),
        arguments_json: serde_json::json!({"agent": agent, "task": task}).to_string(),
    }
}

/// What a run of the loop left behind.
struct Outcome {
    status: Status,
    worker_turns: Option<u32>,
    events: Vec<LoopEvent>,
    /// The tool results the coder saw after the delegations, as `(call id, text)`.
    results: Vec<(String, String)>,
    most_inside: usize,
}

/// Runs a loop whose build turn makes `calls`, with `rendezvous` in the subagents' provider.
async fn run_loop(calls: Vec<ToolCall>, rendezvous: Option<Arc<Barrier>>) -> Outcome {
    let dir = tempfile::tempdir().expect("tempdir");
    let gate = Arc::new(Gate {
        rendezvous,
        inside: Arc::default(),
        most_inside: Arc::default(),
    });
    let env = Arc::new(Env {
        gate: Arc::clone(&gate),
        dir: dir.path().to_path_buf(),
    });
    let agents = Arc::new(AgentSet::discover(dir.path(), None));
    let mut registry = ToolRegistry::with_builtins(
        Permissions::empty(),
        Box::new(StaticPrompt(PromptAnswer::No)),
    );
    registry.register(Box::new(DelegateTool::new(agents, env)));

    let coder = Arc::new(FakeProvider::new(vec![
        FakeProvider::text_turn("Looked.\n\n# Plan\n\n1. [ ] Ask the workers\n"),
        FakeProvider::tool_turn(calls),
        FakeProvider::text_turn("built"),
        FakeProvider::text_turn("```json\n{\"verdict\": \"pass\"}\n```"),
    ]));
    let reviewer = Arc::new(FakeProvider::new(vec![FakeProvider::text_turn(
        "```json\n{\"verdict\": \"approve\"}\n```",
    )]));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let config = LoopConfig {
        task: "ask the workers".to_owned(),
        cwd: dir.path().to_path_buf(),
        session_dir: dir.path().join(".anvil").join("sessions").join("t1"),
        limits: LimitsConfig::default(),
        check: CheckCommand::None,
        context_window: 100_000,
        home_dir: None,
        extra_instructions: Vec::new(),
        sections: Vec::new(),
        review_sections: Vec::new(),
        delegate_guidance: true,
    };
    let agent = anvil_core::agent::AgentLoop::new(
        config,
        LoopProviders {
            coder: Arc::clone(&coder) as Arc<dyn Provider>,
            planner: None,
            reviewer: reviewer as Arc<dyn Provider>,
            summarizer: Arc::new(FakeProvider::new(Vec::new())) as Arc<dyn Provider>,
        },
        registry,
        ToolContext::new(dir.path()),
        Box::new(AutoApprove),
        EventSink::new(tx),
    );
    let result = agent.run().await;
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    let results = coder
        .requests()
        .iter()
        .flat_map(|request| request.messages.iter())
        .find_map(|message| match message {
            Message::ToolResults { results } => Some(
                results
                    .iter()
                    .map(|result| (result.call_id.clone(), result.content.clone()))
                    .collect(),
            ),
            _ => None,
        })
        .unwrap_or_default();
    Outcome {
        status: result.status,
        worker_turns: result
            .role_spend
            .get(&Role::Worker)
            .map(|spend| spend.turns),
        events,
        results,
        most_inside: gate.most_inside.load(Ordering::SeqCst),
    }
}

fn tool_events(events: &[LoopEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            LoopEvent::ToolStarted { call } => Some(format!("started {}", call.id)),
            LoopEvent::ToolFinished { call_id, .. } => Some(format!("finished {call_id}")),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn two_explore_calls_in_one_turn_overlap_and_return_in_call_order() {
    let calls = vec![
        delegate_call("d1", "explore", "first question"),
        delegate_call("d2", "explore", "second question"),
    ];
    let outcome = run_loop(calls, Some(Arc::new(Barrier::new(2)))).await;
    assert_eq!(outcome.status, Status::Done);
    assert_eq!(outcome.most_inside, 2, "both subagents were inside at once");
    assert_eq!(
        outcome.results,
        [
            ("d1".to_owned(), "answer to first question".to_owned()),
            ("d2".to_owned(), "answer to second question".to_owned()),
        ]
    );
    assert_eq!(
        tool_events(&outcome.events),
        ["started d1", "started d2", "finished d1", "finished d2"]
    );
    assert_eq!(outcome.worker_turns, Some(2));
    let worker_events = outcome
        .events
        .iter()
        .filter(|event| {
            matches!(
                event,
                LoopEvent::TurnFinished {
                    role: Role::Worker,
                    state: State::Build,
                    ..
                }
            )
        })
        .count();
    assert_eq!(worker_events, 2);
}

#[tokio::test]
async fn two_implement_calls_run_one_after_another_in_call_order() {
    let calls = vec![
        delegate_call("d1", "implement", "first step"),
        delegate_call("d2", "implement", "second step"),
    ];
    let outcome = run_loop(calls, None).await;
    assert_eq!(outcome.status, Status::Done);
    assert_eq!(outcome.most_inside, 1, "the subagents never overlapped");
    assert_eq!(
        outcome.results,
        [
            ("d1".to_owned(), "answer to first step".to_owned()),
            ("d2".to_owned(), "answer to second step".to_owned()),
        ]
    );
    assert_eq!(
        tool_events(&outcome.events),
        ["started d1", "finished d1", "started d2", "finished d2"]
    );
    assert_eq!(outcome.worker_turns, Some(2));
}

#[tokio::test]
async fn explore_and_implement_in_one_turn_do_not_overlap() {
    let calls = vec![
        delegate_call("d1", "explore", "first question"),
        delegate_call("d2", "implement", "second step"),
    ];
    let outcome = run_loop(calls, None).await;
    assert_eq!(outcome.most_inside, 1);
    assert_eq!(outcome.results.len(), 2);
}
