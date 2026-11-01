//! Which front end a session talks to: the line renderer with stdin
//! prompts, or the full-screen UI. The session driver asks the front end
//! for input, prompts and a console, and never knows which it got.

use std::io::{self, IsTerminal};
use std::time::Instant;

use anvil_core::agent::{LoopEvent, State};
use anvil_core::tools::{PermissionPrompt, PromptAnswer, StaticPrompt};
use anyhow::Context;
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::console::Console;
use crate::render;
use crate::terminal::TerminalPrompt;
use crate::tui::runner::{UiChannels, run_ui};
use crate::tui::{App, Input, SessionInfo, SessionKind, Theme, UiExit, UiHandle, UiMsg};
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
    task: JoinHandle<io::Result<UiExit>>,
}

impl Frontend {
    /// Starts a front end.
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
                let app = App::new(info, Theme::detect(), Instant::now());
                let task = tokio::spawn(run_ui(
                    app,
                    UiChannels {
                        events: events_rx,
                        msgs: msg_rx,
                        input: input_tx,
                    },
                ));
                Frontend::Tui(TuiFrontend {
                    ui: UiHandle::new(msg_tx),
                    input: input_rx,
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

    /// Who answers `ask` permission decisions.
    ///
    /// Under the full-screen UI every asked call is declined until the
    /// permission modal lands; the status line says so.
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
            Frontend::Tui(_) => Box::new(StaticPrompt(PromptAnswer::No)),
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
    /// the plain renderer never ends.
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
                    Ok(Ok(_)) => Ok(()),
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
    SessionInfo {
        kind,
        model: model_label(wiring, coder),
        context_window: wiring.context_window(),
        price: provider.and_then(|provider| provider.price_per_mtoken),
        permission_mode: if yes { "bypass" } else { "ask" }.to_owned(),
        initial_state,
        slash_commands,
    }
}

/// A provider's name with its model, as the status line shows it.
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
