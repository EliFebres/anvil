//! `forge chat`: a streaming REPL with tools, outside the state machine.
//!
//! The prompt shows the current mode (`[plan] >`, `[build] >`,
//! `[review] >`), which picks the system prompt and the tool set for the
//! next turns. Slash commands switch modes, compact, swap the model and
//! report cost.

use std::collections::BTreeSet;
use std::sync::Arc;

use anyhow::Context;
use forge_core::agent::prompts::{self, PromptInputs};
use forge_core::agent::turn::complete_turn;
use forge_core::agent::{EventSink, LoopEvent, State};
use forge_core::context::{Conversation, discover_instructions};
use forge_core::provider::{CompletionRequest, Message, Provider, ToolSpec};
use forge_core::tools::{Access, ToolContext, ToolRegistry};
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;
use tokio::sync::mpsc;

use crate::render;
use crate::wiring::Wiring;

/// Lines of help printed by `/help`.
const HELP: &str = "\
/plan            read-only tools, planning prompt
/build           all tools, build prompt
/review          no tools, reviewer prompt
/compact         summarize older turns with the summarizer
/model <name>    switch the coder to a provider from [providers]
/cost            tokens used so far
/quit            leave";

/// Runs the REPL until the user quits.
///
/// # Arguments
///
/// * `wiring` - Providers and paths.
///
/// # Returns
///
/// Zero.
///
/// # Errors
///
/// When providers, permissions or the line editor cannot be set up.
pub async fn chat(wiring: &Wiring) -> anyhow::Result<i32> {
    let (session_dir, id) = wiring.new_session_dir();
    std::fs::create_dir_all(&session_dir).context("creating the session folder")?;
    eprintln!("chat session {id}; /help for commands");
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(render::render(rx, false));
    let mut session = ChatSession {
        wiring,
        mode: State::Build,
        coder_name: wiring.config.roles.coder.clone(),
        coder: wiring.provider(&wiring.config.roles.coder)?,
        registry: wiring.registry(Wiring::prompt())?,
        ctx: ToolContext {
            session_dir: Some(session_dir.clone()),
            ..wiring.tool_context()
        },
        session_dir,
        conversation: Conversation::new(),
        touched: BTreeSet::new(),
        tokens_used: 0,
        events: EventSink::new(tx),
    };
    let mut editor = DefaultEditor::new().context("starting the line editor")?;
    loop {
        let line = match editor.readline(&format!("[{}] > ", session.mode)) {
            Ok(line) => line,
            Err(ReadlineError::Interrupted | ReadlineError::Eof) => break,
            Err(error) => return Err(error).context("reading input"),
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let _ = editor.add_history_entry(line);
        if let Some(command) = line.strip_prefix('/') {
            if !session.command(command).await {
                break;
            }
            continue;
        }
        session.turn(line).await;
    }
    tokio::task::yield_now().await;
    Ok(0)
}

/// Everything one chat holds.
struct ChatSession<'a> {
    wiring: &'a Wiring,
    mode: State,
    coder_name: String,
    coder: Arc<dyn Provider>,
    registry: ToolRegistry,
    ctx: ToolContext,
    session_dir: std::path::PathBuf,
    conversation: Conversation,
    touched: BTreeSet<std::path::PathBuf>,
    tokens_used: u64,
    events: EventSink,
}

impl ChatSession<'_> {
    /// Handles a slash command; returns `false` to leave the REPL.
    async fn command(&mut self, command: &str) -> bool {
        let mut words = command.split_whitespace();
        match (words.next().unwrap_or(""), words.next()) {
            ("plan", _) => self.mode = State::Plan,
            ("build", _) => self.mode = State::Build,
            ("review", _) => self.mode = State::Review,
            ("compact", _) => self.compact().await,
            ("model", Some(name)) => match self.wiring.provider(name) {
                Ok(provider) => {
                    self.coder = provider;
                    self.coder_name = name.to_owned();
                    eprintln!("coder is now `{name}`");
                }
                Err(error) => eprintln!("error: {error:#}"),
            },
            ("model", None) => eprintln!("coder is `{}`; usage: /model <name>", self.coder_name),
            ("cost", _) => {
                let budget = self
                    .wiring
                    .budget
                    .lock()
                    .map(|b| b.remaining())
                    .unwrap_or(0);
                eprintln!(
                    "{} tokens used this chat; {budget} left in the session budget",
                    self.tokens_used
                );
            }
            ("help", _) => {
                eprintln!("{HELP}");
                let skills = self.wiring.skills.help_lines();
                if !skills.is_empty() {
                    eprintln!("\nskills (each loads its instructions into the chat):");
                    for line in skills {
                        eprintln!("{line}");
                    }
                }
                let commands = self.wiring.commands.help_lines();
                if !commands.is_empty() {
                    eprintln!("\ncommands (from .forge/commands and .claude/commands):");
                    for line in commands {
                        eprintln!("{line}");
                    }
                }
            }
            ("quit" | "exit" | "q", _) => return false,
            (other, _) => match self.wiring.expand_slash(&format!("/{command}")) {
                Some(message) => self.turn(&message).await,
                None => eprintln!("unknown command `/{other}`; /help lists them"),
            },
        }
        true
    }

    /// Sends one user line and runs the model until it stops calling tools.
    async fn turn(&mut self, line: &str) {
        self.conversation.push(Message::user(line));
        let tools = self.tools();
        loop {
            let system = self.system_prompt(&tools);
            let request_len = self.conversation.messages().len();
            let request = CompletionRequest {
                system,
                messages: self.conversation.messages().to_vec(),
                tools: tools.clone(),
                max_output_tokens: None,
            };
            let turn = match complete_turn(&*self.coder, request, &self.events).await {
                Ok(turn) => turn,
                Err(error) => {
                    eprintln!("\nerror: {error}");
                    return;
                }
            };
            if let Some(usage) = turn.usage {
                self.tokens_used += usage.total();
                self.conversation.record_usage(request_len, usage);
            }
            self.conversation.push(turn.message());
            if turn.tool_calls.is_empty() {
                println!();
                return;
            }
            let mut results = Vec::new();
            for call in &turn.tool_calls {
                self.events
                    .emit(LoopEvent::ToolStarted { call: call.clone() });
                let execution = self.registry.execute(call, &self.ctx).await;
                if let Some(path) = serde_json::from_str::<serde_json::Value>(&call.arguments_json)
                    .ok()
                    .and_then(|args| args.get("path").and_then(|p| p.as_str()).map(str::to_owned))
                {
                    self.touched.insert(self.ctx.resolve(&path));
                }
                self.events.emit(LoopEvent::ToolFinished {
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                    outcome: execution.outcome,
                    preview: execution
                        .result
                        .content
                        .lines()
                        .next()
                        .unwrap_or("")
                        .to_owned(),
                });
                results.push(execution.result);
            }
            self.conversation.push(Message::ToolResults { results });
            if self
                .conversation
                .needs_compaction("", self.wiring.context_window())
            {
                self.compact().await;
            }
        }
    }

    /// Compacts the conversation with the summarizer.
    async fn compact(&mut self) {
        let summarizer = match self.wiring.provider(&self.wiring.config.roles.summarizer) {
            Ok(provider) => provider,
            Err(error) => {
                eprintln!("error: {error:#}");
                return;
            }
        };
        let max_chars = usize::try_from(self.wiring.context_window())
            .unwrap_or(usize::MAX)
            .saturating_mul(2);
        match self
            .conversation
            .compact(
                &*summarizer,
                "interactive chat",
                &self.session_dir,
                max_chars,
            )
            .await
        {
            Ok(Some(compaction)) => eprintln!(
                "compacted {} messages; summary at {}",
                compaction.dropped_messages,
                compaction.summary_path.display()
            ),
            Ok(None) => eprintln!("nothing to compact yet (fewer than five turns)"),
            Err(error) => eprintln!("compaction failed: {error}"),
        }
    }

    /// The tools for the current mode.
    fn tools(&self) -> Vec<ToolSpec> {
        match self.mode {
            State::Review => Vec::new(),
            State::Plan => self
                .registry
                .specs()
                .into_iter()
                .filter(|spec| {
                    self.registry
                        .get(&spec.name)
                        .is_some_and(|tool| tool.access() == Access::Read)
                })
                .collect(),
            _ => self.registry.specs(),
        }
    }

    /// The system prompt for the current mode.
    fn system_prompt(&self, tools: &[ToolSpec]) -> String {
        if self.mode == State::Review {
            return prompts::REVIEW.to_owned();
        }
        let mut instructions =
            discover_instructions(self.wiring.home.as_deref(), &self.wiring.cwd, &self.touched);
        instructions.extend(self.wiring.extra_instructions(&self.registry.names()));
        prompts::system_prompt(&PromptInputs {
            state: self.mode,
            instructions: &instructions,
            tools,
            task: "Interactive chat: do what the user asks in each message.",
        })
    }
}
