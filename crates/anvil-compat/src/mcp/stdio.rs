//! The stdio transport: a child process speaking newline-delimited JSON-RPC.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::oneshot;

use super::client::{RpcFuture, RpcTransport};
use super::jsonrpc::{Incoming, McpError, Notification, Request, Response, parse_incoming};

/// How many stderr lines are kept for the "server exited" message.
const STDERR_TAIL_LINES: usize = 20;

/// Responses waiting for the reader task, by request id.
type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Response>>>>;

/// A running stdio server.
pub struct StdioTransport {
    /// The server name, for errors.
    name: String,
    /// The child's stdin, shared with the reader task for replies.
    stdin: Arc<tokio::sync::Mutex<ChildStdin>>,
    /// Requests waiting for a response.
    pending: Pending,
    /// The next request id.
    next_id: AtomicU64,
    /// Cleared by the reader task when stdout closes.
    alive: Arc<AtomicBool>,
    /// The last stderr lines.
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
    /// The process; killed when the transport drops.
    _child: Child,
}

impl StdioTransport {
    /// Spawns the server and starts reading its output.
    ///
    /// # Arguments
    ///
    /// * `name` - The server name.
    /// * `command` - The program to run.
    /// * `args` - Its arguments.
    /// * `env` - Extra environment variables.
    /// * `cwd` - The working directory.
    ///
    /// # Returns
    ///
    /// The transport.
    ///
    /// # Errors
    ///
    /// [`McpError::Spawn`] when the process cannot start.
    pub fn spawn(
        name: &str,
        command: &str,
        args: &[String],
        env: &BTreeMap<String, String>,
        cwd: &Path,
    ) -> Result<Self, McpError> {
        let program = resolve_program(command);
        let mut child = Command::new(&program)
            .args(args)
            .envs(env)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| McpError::Spawn {
                server: name.to_owned(),
                command: command.to_owned(),
                detail: error.to_string(),
            })?;
        let stdin = child.stdin.take().ok_or_else(|| McpError::Spawn {
            server: name.to_owned(),
            command: command.to_owned(),
            detail: "no stdin pipe".to_owned(),
        })?;
        let stdout = child.stdout.take().ok_or_else(|| McpError::Spawn {
            server: name.to_owned(),
            command: command.to_owned(),
            detail: "no stdout pipe".to_owned(),
        })?;
        let stdin = Arc::new(tokio::sync::Mutex::new(stdin));
        let pending: Pending = Arc::default();
        let alive = Arc::new(AtomicBool::new(true));
        let stderr_tail = Arc::new(Mutex::new(VecDeque::new()));

        if let Some(stderr) = child.stderr.take() {
            let tail = Arc::clone(&stderr_tail);
            let server = name.to_owned();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(server, "{line}");
                    if let Ok(mut tail) = tail.lock() {
                        if tail.len() == STDERR_TAIL_LINES {
                            tail.pop_front();
                        }
                        tail.push_back(line);
                    }
                }
            });
        }
        tokio::spawn(read_loop(
            name.to_owned(),
            stdout,
            Arc::clone(&stdin),
            Arc::clone(&pending),
            Arc::clone(&alive),
        ));

        Ok(Self {
            name: name.to_owned(),
            stdin,
            pending,
            next_id: AtomicU64::new(1),
            alive,
            stderr_tail,
            _child: child,
        })
    }

    /// The error for a server that is gone, with its last stderr lines.
    fn exited(&self) -> McpError {
        let tail = self
            .stderr_tail
            .lock()
            .ok()
            .filter(|tail| !tail.is_empty())
            .map(|tail| tail.iter().cloned().collect::<Vec<_>>().join("\n"));
        McpError::ServerExited {
            server: self.name.clone(),
            stderr_tail: tail,
        }
    }

    /// Writes one JSON document and a newline.
    async fn send(&self, text: String) -> Result<(), McpError> {
        if !self.alive.load(Ordering::SeqCst) {
            return Err(self.exited());
        }
        let mut stdin = self.stdin.lock().await;
        let written = async {
            stdin.write_all(text.as_bytes()).await?;
            stdin.write_all(b"\n").await?;
            stdin.flush().await
        }
        .await;
        written.map_err(|error| {
            if self.alive.load(Ordering::SeqCst) {
                McpError::Transport {
                    server: self.name.clone(),
                    detail: error.to_string(),
                }
            } else {
                self.exited()
            }
        })
    }
}

impl RpcTransport for StdioTransport {
    fn request<'a>(&'a self, method: &'a str, params: Option<Value>) -> RpcFuture<'a, Response> {
        Box::pin(async move {
            let id = self.next_id.fetch_add(1, Ordering::SeqCst);
            let (tx, rx) = oneshot::channel();
            if let Ok(mut pending) = self.pending.lock() {
                pending.insert(id, tx);
            }
            let text =
                serde_json::to_string(&Request::new(id, method, params)).map_err(|error| {
                    McpError::Protocol {
                        server: self.name.clone(),
                        detail: error.to_string(),
                    }
                })?;
            if let Err(error) = self.send(text).await {
                if let Ok(mut pending) = self.pending.lock() {
                    pending.remove(&id);
                }
                return Err(error);
            }
            rx.await.map_err(|_| self.exited())
        })
    }

    fn notify<'a>(&'a self, method: &'a str, params: Option<Value>) -> RpcFuture<'a, ()> {
        Box::pin(async move {
            let text =
                serde_json::to_string(&Notification::new(method, params)).map_err(|error| {
                    McpError::Protocol {
                        server: self.name.clone(),
                        detail: error.to_string(),
                    }
                })?;
            self.send(text).await
        })
    }
}

/// Reads stdout until it closes, routing responses and answering requests.
async fn read_loop(
    server: String,
    stdout: tokio::process::ChildStdout,
    stdin: Arc<tokio::sync::Mutex<ChildStdin>>,
    pending: Pending,
    alive: Arc<AtomicBool>,
) {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        match parse_incoming(&line) {
            Ok(Incoming::Response(response)) => {
                let waiter = response
                    .numeric_id()
                    .and_then(|id| pending.lock().ok().and_then(|mut map| map.remove(&id)));
                match waiter {
                    Some(tx) => {
                        let _ = tx.send(response);
                    }
                    None => tracing::debug!(server, id = ?response.id, "response without a waiter"),
                }
            }
            Ok(Incoming::ServerRequest { id, method }) => {
                tracing::debug!(server, method, "declining a server request");
                let reply = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": super::jsonrpc::METHOD_NOT_FOUND, "message": format!("anvil does not support `{method}`")}
                });
                let mut stdin = stdin.lock().await;
                let _ = stdin.write_all(reply.to_string().as_bytes()).await;
                let _ = stdin.write_all(b"\n").await;
                let _ = stdin.flush().await;
            }
            Ok(Incoming::Notification { method }) => {
                tracing::debug!(server, method, "server notification");
            }
            Err(detail) => tracing::warn!(server, detail, "ignoring a line that is not JSON-RPC"),
        }
    }
    alive.store(false, Ordering::SeqCst);
    // Dropping the senders wakes every waiter with a RecvError, which the
    // request future turns into ServerExited.
    if let Ok(mut map) = pending.lock() {
        map.clear();
    }
    tracing::info!(server, "mcp server closed its output");
}

/// Finds the executable for a bare command name.
///
/// On Windows a bare `npx` is `npx.cmd` somewhere on `PATH`; the standard
/// library only looks for `.exe`, so the search here also tries every
/// `PATHEXT` extension. Names with a path separator or an extension are
/// returned unchanged.
fn resolve_program(command: &str) -> String {
    if !cfg!(windows) || command.contains(['/', '\\']) || Path::new(command).extension().is_some() {
        return command.to_owned();
    }
    let extensions: Vec<String> = std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_owned())
        .split(';')
        .filter(|ext| !ext.is_empty())
        .map(str::to_owned)
        .collect();
    let dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    for dir in dirs {
        for ext in &extensions {
            let candidate = dir.join(format!("{command}{ext}"));
            if candidate.is_file() {
                return candidate.to_string_lossy().into_owned();
            }
        }
    }
    command.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_and_extensions_are_left_alone() {
        assert_eq!(resolve_program("node.exe"), "node.exe");
        assert_eq!(resolve_program("./bin/server"), "./bin/server");
        assert_eq!(resolve_program("C:\\tools\\x"), "C:\\tools\\x");
    }
}
