//! Which front end a session talks to: the line renderer with stdin
//! prompts, or the full-screen UI. The session driver asks the front end
//! for input, prompts and a console, and never knows which it got.

use std::io::{self, IsTerminal};
use std::sync::Arc;
use std::time::Instant;

use anvil_core::agent::{AutoApprove, LoopEvent, PlanGate, State, Status};
use anvil_core::tools::{PermissionPrompt, PromptAnswer, StaticPrompt, ToolHook, ToolRegistry};
use anvil_memory::{Decision, Proposal};
use anyhow::Context;
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::console::Console;
use crate::memory::{self, terminal_decider, tui_decider};
use crate::render;
use crate::terminal::{TerminalGate, TerminalPrompt};
use crate::tui::runner::{UiChannels, run_ui};
use crate::tui::{
    App, Input, PermissionMode, RecentSession, ResultTee, SessionInfo, SessionKind, Theme, TuiGate,
    TuiPrompt, UiExit, UiHandle, UiMsg, animate_logo,
};
use crate::wiring::Wiring;

/// Which renderer a session uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiKind {
    /// Lines on stdout and stderr, prompts on stdin.
    Plain,
    /// The full-screen UI.
    Tui,
}

impl UiKind {
    /// Picks the renderer: plain when asked for, when the run is
    /// unattended, when the command is not interactive, or when any
    /// standard stream is not a terminal.
    ///
    /// # Arguments
    ///
    /// * `plain` - Whether `--plain` was given.
    /// * `yes` - Whether `--yes` was given.
    /// * `interactive` - Whether the command talks to a user at all.
    ///
    /// # Returns
    ///
    /// The kind.
    pub fn choose(plain: bool, yes: bool, interactive: bool) -> Self {
        if plain || yes || !interactive {
            return Self::Plain;
        }
        if std::env::var("TERM").is_ok_and(|term| term == "dumb") {
            return Self::Plain;
        }
        let on_terminal =
            io::stdin().is_terminal() && io::stdout().is_terminal() && io::stderr().is_terminal();
        if on_terminal { Self::Tui } else { Self::Plain }
    }
}

/// The running front end of one session.
pub enum Frontend {
    /// The line renderer, boxed because the line editor is large.
    Plain(Box<PlainFrontend>),
    /// The full-screen UI.
    Tui(TuiFrontend),
}

/// The line renderer: a render task and a lazily created line editor.
pub struct PlainFrontend {
    editor: Option<DefaultEditor>,
    render: JoinHandle<()>,
}

/// The full-screen UI task and the channels into and out of it.
pub struct TuiFrontend {
    ui: UiHandle,
    input: mpsc::UnboundedReceiver<Input>,
    cancel: watch::Receiver<u64>,
    task: JoinHandle<io::Result<()>>,
}

impl Frontend {
    /// Starts a front end.
    ///
    /// When the full-screen UI closes before the session is over, the
    /// events it did not show continue on the line renderer, so a run never
    /// goes silent.
    ///
    /// # Arguments
    ///
    /// * `kind` - Which one.
    /// * `info` - What the UI shows about the session.
    ///
    /// # Returns
    ///
    /// The front end and the sender its loop events go to.
    pub fn start(kind: UiKind, info: SessionInfo) -> (Self, mpsc::UnboundedSender<LoopEvent>) {
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let frontend = match kind {
            UiKind::Plain => Frontend::Plain(Box::new(PlainFrontend {
                editor: None,
                render: tokio::spawn(render::render(events_rx, false)),
            })),
            UiKind::Tui => {
                let (msg_tx, msg_rx) = mpsc::unbounded_channel();
                let (input_tx, input_rx) = mpsc::unbounded_channel();
                let (cancel_tx, cancel_rx) = watch::channel(0u64);
                let cwd = info.cwd.clone();
                let app = App::new(info, Theme::detect(), Instant::now());
                let channels = UiChannels {
                    events: events_rx,
                    msgs: msg_rx,
                    input: input_tx,
                    cancel: cancel_tx,
                    cwd,
                };
                let task = tokio::spawn(async move {
                    let UiExit::Quit { events } = run_ui(app, channels).await?;
                    render::render(events, false).await;
                    Ok(())
                });
                Frontend::Tui(TuiFrontend {
                    ui: UiHandle::new(msg_tx),
                    input: input_rx,
                    cancel: cancel_rx,
                    task,
                })
            }
        };
        (frontend, events_tx)
    }

    /// Where the session's status lines go.
    ///
    /// # Returns
    ///
    /// The console.
    pub fn console(&self) -> Console {
        match self {
            Frontend::Plain(_) => Console::Stderr,
            Frontend::Tui(tui) => Console::Ui(tui.ui.clone()),
        }
    }

    /// Who answers `ask` permission decisions: stdin in plain mode, the
    /// permission prompt under the UI.
    ///
    /// # Arguments
    ///
    /// * `yes` - Whether the run is unattended, which approves everything.
    ///
    /// # Returns
    ///
    /// The prompt, not yet recorded by telemetry.
    pub fn prompt(&self, yes: bool) -> Box<dyn PermissionPrompt> {
        if yes {
            return Box::new(StaticPrompt(PromptAnswer::Yes));
        }
        match self {
            Frontend::Plain(_) => Box::new(TerminalPrompt),
            Frontend::Tui(tui) => Box::new(TuiPrompt::new(tui.ui.clone())),
        }
    }

    /// Who approves the plan: stdin in plain mode, the plan prompt under
    /// the UI.
    ///
    /// # Arguments
    ///
    /// * `yes` - Whether the run is unattended, which approves every plan.
    ///
    /// # Returns
    ///
    /// The gate.
    pub fn gate(&self, yes: bool) -> Box<dyn PlanGate> {
        if yes {
            return Box::new(AutoApprove);
        }
        match self {
            Frontend::Plain(_) => Box::new(TerminalGate),
            Frontend::Tui(tui) => Box::new(TuiGate::new(tui.ui.clone())),
        }
    }

    /// Who decides which proposed memory facts to keep: stdin in plain
    /// mode, the keep/drop prompt under the UI.
    ///
    /// # Arguments
    ///
    /// * `yes` - Whether the run is unattended, which keeps everything.
    ///
    /// # Returns
    ///
    /// The decider, called once per proposal.
    pub fn decider(&self, yes: bool) -> Box<dyn FnMut(&Proposal) -> Decision> {
        if yes {
            return Box::new(|_| Decision::Keep);
        }
        match self {
            Frontend::Plain(_) => terminal_decider(),
            Frontend::Tui(tui) => tui_decider(tui.ui.clone()),
        }
    }

    /// The signal the UI bumps when the user cancels a turn.
    ///
    /// # Returns
    ///
    /// A watch on it, or `None` in plain mode, which never cancels.
    pub fn cancel_watch(&self) -> Option<watch::Receiver<u64>> {
        match self {
            Frontend::Plain(_) => None,
            Frontend::Tui(tui) => Some(tui.cancel.clone()),
        }
    }

    /// Makes every tool result reach the UI, keeping the hook that was
    /// already installed running behind it.
    ///
    /// # Arguments
    ///
    /// * `registry` - The session's tools.
    /// * `inner` - The hook the registry had, if any.
    pub fn install_tool_hook(&self, registry: &mut ToolRegistry, inner: Option<Arc<dyn ToolHook>>) {
        if let Frontend::Tui(tui) = self {
            registry.set_hook(Arc::new(ResultTee::new(inner, tui.ui.clone())));
        }
    }

    /// Waits for the next line from the user.
    ///
    /// # Arguments
    ///
    /// * `prompt` - The prompt the line editor shows in plain mode.
    ///
    /// # Returns
    ///
    /// The line, or why there is none.
    ///
    /// # Errors
    ///
    /// When the line editor cannot be started or read in plain mode.
    pub async fn next_input(&mut self, prompt: &str) -> anyhow::Result<Input> {
        match self {
            Frontend::Plain(plain) => {
                let editor = match &mut plain.editor {
                    Some(editor) => editor,
                    None => plain
                        .editor
                        .insert(DefaultEditor::new().context("starting the line editor")?),
                };
                match editor.readline(prompt) {
                    Ok(line) => {
                        if !line.trim().is_empty() {
                            let _ = editor.add_history_entry(line.trim());
                        }
                        Ok(Input::Line(line))
                    }
                    Err(ReadlineError::Interrupted | ReadlineError::Eof) => Ok(Input::Quit),
                    Err(error) => Err(error).context("reading input"),
                }
            }
            Frontend::Tui(tui) => Ok(tui.input.recv().await.unwrap_or(Input::Quit)),
        }
    }

    /// Ends the front end: waits for the last events to be rendered and,
    /// for the UI, restores the terminal.
    ///
    /// The loop event sender must be dropped before this is awaited, or
    /// the renderer never ends.
    ///
    /// # Arguments
    ///
    /// * `wait_for_key` - Whether the UI should wait for a key before closing.
    ///
    /// # Errors
    ///
    /// When the UI task failed or panicked.
    pub async fn shutdown(self, wait_for_key: bool) -> anyhow::Result<()> {
        match self {
            Frontend::Plain(plain) => {
                let _ = plain.render.await;
                Ok(())
            }
            Frontend::Tui(tui) => {
                tui.ui.send(UiMsg::Shutdown { wait_for_key });
                match tui.task.await {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(error)) => Err(error).context("the full-screen UI failed"),
                    Err(join) => {
                        crate::tui::runner::TerminalGuard::restore();
                        Err(anyhow::anyhow!("the full-screen UI stopped: {join}"))
                    }
                }
            }
        }
    }
}

/// The slash commands the chat itself handles, for Tab completion.
pub const BUILTIN_COMMANDS: &[&str] = &[
    "/plan", "/build", "/review", "/compact", "/model", "/cost", "/mode", "/expand", "/plain",
    "/help", "/ticket", "/quit", "/exit",
];

/// What the UI shows about a session.
///
/// # Arguments
///
/// * `wiring` - Providers and paths.
/// * `kind` - Chat or run.
/// * `yes` - Whether the run is unattended, which shows the bypass mode.
/// * `initial_state` - The state before the first event.
///
/// # Returns
///
/// The description.
pub fn session_info(
    wiring: &Wiring,
    kind: SessionKind,
    yes: bool,
    initial_state: State,
) -> SessionInfo {
    let coder = &wiring.config.roles.coder;
    let provider = wiring.config.providers.get(coder);
    let mut slash_commands: Vec<String> = BUILTIN_COMMANDS
        .iter()
        .map(|command| (*command).to_owned())
        .collect();
    slash_commands.extend(wiring.skills.names().iter().map(|name| format!("/{name}")));
    slash_commands.extend(
        wiring
            .commands
            .commands()
            .iter()
            .map(|command| format!("/{}", command.name)),
    );
    let rules = wiring
        .home
        .as_deref()
        .and_then(|home| memory::open_rules(home).ok())
        .map(|set| {
            set.rules
                .into_iter()
                .map(|rule| (rule.id, rule.rule))
                .collect()
        })
        .unwrap_or_default();
    let recent_sessions = anvil_core::context::list_sessions(&wiring.cwd)
        .unwrap_or_default()
        .into_iter()
        .take(RECENT_SESSIONS)
        .map(recent_session)
        .collect();
    SessionInfo {
        kind,
        cwd: wiring.cwd.clone(),
        model: model_id(wiring, coder),
        context_window: wiring.context_window(),
        price: provider.and_then(|provider| provider.price_per_mtoken),
        permission_mode: if yes {
            PermissionMode::Bypass
        } else {
            PermissionMode::Ask
        },
        initial_state,
        slash_commands,
        version: env!("CARGO_PKG_VERSION").to_owned(),
        coder: model_label(wiring, coder),
        reviewer: wiring
            .config
            .roles
            .reviewer
            .as_deref()
            .map(|name| model_label(wiring, name)),
        rules,
        skills: wiring.skills.skills().len(),
        mcp_servers: wiring.mcp_servers.servers().len(),
        recent_sessions,
        animate: animate_logo(
            io::stdout().is_terminal(),
            std::env::var_os("ANVIL_NO_ANIMATION").is_some(),
        ),
    }
}

/// How many saved sessions the startup screen lists.
const RECENT_SESSIONS: usize = 5;

/// A saved session as the startup screen shows it.
fn recent_session(session: anvil_core::context::SessionSummary) -> RecentSession {
    let (status, ok) = match (&session.status, session.last_state) {
        (Some(Status::Done), _) => ("done", Some(true)),
        (Some(Status::Incomplete { .. }), _) => ("incomplete", Some(false)),
        (Some(Status::Denied { .. }), _) => ("denied", Some(false)),
        (Some(Status::ProviderError { .. }), _) => ("error", Some(false)),
        (None, Some(state)) => {
            return RecentSession {
                status: format!("stopped in {state}"),
                ..recent_session_with(session, "", None)
            };
        }
        (None, None) => ("empty", None),
    };
    recent_session_with(session, status, ok)
}

/// A saved session with its status already decided.
fn recent_session_with(
    session: anvil_core::context::SessionSummary,
    status: &str,
    ok: Option<bool>,
) -> RecentSession {
    let status = status.to_owned();
    RecentSession {
        id: session.meta.id,
        status,
        ok,
        task: session.meta.task,
        when: session
            .meta
            .created_at
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M")
            .to_string(),
    }
}

/// A provider's name with its model, as the startup screen shows it.
///
/// # Arguments
///
/// * `wiring` - Providers and paths.
/// * `name` - The provider name from `[providers]`.
///
/// # Returns
///
/// `name (model)`, or just the name when it is not configured.
pub fn model_label(wiring: &Wiring, name: &str) -> String {
    match wiring.config.providers.get(name) {
        Some(provider) => format!("{name} ({})", provider.model),
        None => name.to_owned(),
    }
}

/// A provider's model id, as the status line shows it.
///
/// # Arguments
///
/// * `wiring` - Providers and paths.
/// * `name` - The provider name from `[providers]`.
///
/// # Returns
///
/// The model id, or the name when the provider is not configured.
pub fn model_id(wiring: &Wiring, name: &str) -> String {
    match wiring.config.providers.get(name) {
        Some(provider) => provider.model.clone(),
        None => name.to_owned(),
    }
}
