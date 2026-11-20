//! Tracing setup: a JSON file under `.anvil/logs/` and a human console sink.
//!
//! Both sinks write through [`RedactingWriter`], so every log line passes
//! the redactor before it is written: hook output, MCP server stderr,
//! provider error bodies and git output reach the log through many
//! different call sites, and this is the one place they all pass.

use std::io::Write;
use std::path::Path;
use std::sync::{Arc, LazyLock, RwLock};

use anvil_types::redact::Redactor;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, fmt};

/// Folder for log files, relative to the project root.
pub const LOG_DIR: &str = ".anvil/logs";

/// Environment variable that sets the file filter, such as `anvil_core=debug`.
pub const FILTER_ENV: &str = "ANVIL_LOG";

/// What log lines are redacted with: the built-in shapes and the values of
/// secret-looking variables (the `.env` files are loaded before logging
/// starts), until a session's wiring installs its own.
static LOG_REDACTOR: LazyLock<RwLock<Arc<Redactor>>> = LazyLock::new(|| {
    let redactor = Redactor::new(&[], std::env::vars(), &[]).unwrap_or_default();
    RwLock::new(Arc::new(redactor.with_json_escaped_values()))
});

/// Makes the session's redactor the one log lines pass through.
///
/// A redactor a policy turned off is ignored: the log keeps the default
/// one, because turning redaction off is about what the model sees, and
/// the log is never sent anywhere that needs the raw values.
///
/// # Arguments
///
/// * `redactor` - The session's redactor.
pub fn use_redactor(redactor: &Redactor) {
    if !redactor.is_enabled() {
        return;
    }
    let redactor = Arc::new(redactor.clone().with_json_escaped_values());
    if let Ok(mut current) = LOG_REDACTOR.write() {
        *current = redactor;
    }
}

/// The redactor log lines currently pass through.
fn log_redactor() -> Arc<Redactor> {
    match LOG_REDACTOR.read() {
        Ok(current) => Arc::clone(&current),
        Err(_) => Arc::new(Redactor::builtin()),
    }
}

/// Wraps a sink so every line it writes is redacted first.
struct Redacting<M> {
    /// The sink.
    inner: M,
    /// A fixed redactor for tests; `None` uses [`LOG_REDACTOR`].
    redactor: Option<Arc<Redactor>>,
}

impl<M> Redacting<M> {
    /// Wraps `inner`, redacting with the process's log redactor.
    fn new(inner: M) -> Self {
        Self {
            inner,
            redactor: None,
        }
    }
}

impl<'a, M: MakeWriter<'a>> MakeWriter<'a> for Redacting<M> {
    type Writer = RedactingWriter<M::Writer>;

    fn make_writer(&'a self) -> Self::Writer {
        RedactingWriter {
            inner: self.inner.make_writer(),
            buffer: Vec::new(),
            redactor: self.redactor.clone().unwrap_or_else(log_redactor),
        }
    }
}

/// Collects one formatted event and writes it redacted when dropped. The
/// formatter makes one writer per event, so a secret is never split
/// between two writes.
struct RedactingWriter<W: Write> {
    /// Where the redacted line goes.
    inner: W,
    /// The event's bytes so far.
    buffer: Vec<u8>,
    /// What replaces secrets.
    redactor: Arc<Redactor>,
}

impl<W: Write> Write for RedactingWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.buffer.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<W: Write> Drop for RedactingWriter<W> {
    fn drop(&mut self) {
        if self.buffer.is_empty() {
            return;
        }
        let text = self
            .redactor
            .redact(&String::from_utf8_lossy(&self.buffer))
            .text;
        // A log line that cannot be written must not fail the command.
        let _ = self.inner.write_all(text.as_bytes());
        let _ = self.inner.flush();
    }
}

/// Installs the global subscriber.
///
/// The console shows warnings and above; the file gets everything the
/// filter allows (default `info`). Logging never fails the command: when
/// the log folder cannot be created, only the console sink is installed.
///
/// # Arguments
///
/// * `cwd` - The project root.
/// * `console` - Whether to print warnings to stderr. The full-screen UI
///   turns this off, since stderr is the screen; the file still gets them.
///
/// # Returns
///
/// A guard that flushes the file on drop; keep it alive for the process.
pub fn init(cwd: &Path, console: bool) -> Option<WorkerGuard> {
    let filter = EnvFilter::try_from_env(FILTER_ENV).unwrap_or_else(|_| EnvFilter::new("info"));
    let log_dir = cwd.join(LOG_DIR);
    if anvil_types::private_fs::create_dir_all(&log_dir).is_err() {
        let console = console.then(|| {
            fmt::layer()
                .with_writer(Redacting::new(std::io::stderr))
                .with_target(false)
                .without_time()
                .with_filter(LevelFilter::WARN)
        });
        let _ = tracing_subscriber::registry()
            .with(filter)
            .with(console)
            .try_init();
        return None;
    }
    // The appender opens the file with the default mode; creating it first
    // makes it owner-only on Unix.
    let _ = anvil_types::private_fs::open_append(log_dir.join("anvil.jsonl"));
    let file = tracing_appender::rolling::never(&log_dir, "anvil.jsonl");
    let (writer, guard) = tracing_appender::non_blocking(file);
    let json = fmt::layer().json().with_writer(Redacting::new(writer));
    let console = console.then(|| {
        fmt::layer()
            .with_writer(Redacting::new(std::io::stderr))
            .with_target(false)
            .without_time()
            .with_filter(LevelFilter::WARN)
    });
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(json)
        .with(console)
        .try_init();
    Some(guard)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use anvil_compat::hooks::{HookConfig, HookRunner};
    use anvil_core::provider::types::ToolCall;
    use anvil_core::tools::{
        Permissions, PromptAnswer, Shell, StaticPrompt, ToolContext, ToolRegistry,
    };
    use serde_json::json;

    use super::*;

    /// A sink that keeps what it is given.
    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl Write for Captured {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("lock").extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for Captured {
        type Writer = Captured;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    #[tokio::test]
    async fn a_failing_hook_that_prints_a_dotenv_value_leaves_no_value_in_the_log() {
        let dir = tempfile::tempdir().expect("tempdir");
        let secret = "dotenv-deploy-value-4821";
        std::fs::write(dir.path().join(".env"), format!("DEPLOY_TOKEN={secret}\n")).expect("write");
        let values =
            anvil_types::dotenv::read_values(&dir.path().join(".env")).expect("reads .env");
        let redactor = Redactor::new(&[], values, &[])
            .expect("builds")
            .with_json_escaped_values();
        let captured = Captured::default();
        let sink = Redacting {
            inner: captured.clone(),
            redactor: Some(Arc::new(redactor)),
        };
        let subscriber = tracing_subscriber::registry().with(fmt::layer().json().with_writer(sink));
        let _default = tracing::subscriber::set_default(subscriber);

        let command = if cfg!(windows) {
            "[Console]::Error.WriteLine((Get-Content .env -Raw)); exit 1"
        } else {
            "cat .env >&2; exit 1"
        };
        let mut config = HookConfig::default();
        config.add(
            &json!({"PostToolUse": [{"hooks": [{"command": command}]}]}),
            Path::new("settings.json"),
        );
        let runner = HookRunner::new(
            Arc::new(config),
            dir.path().to_path_buf(),
            Shell::for_platform(),
            "sess-1".to_owned(),
            None,
        );
        let mut registry = ToolRegistry::with_builtins(
            Permissions::empty(),
            Box::new(StaticPrompt(PromptAnswer::Yes)),
        );
        registry.set_hook(Arc::new(runner));
        let call = ToolCall {
            id: "c1".to_owned(),
            name: "list_dir".to_owned(),
            arguments_json: "{}".to_owned(),
        };
        let _ = registry.execute(&call, &ToolContext::new(dir.path())).await;

        let log = String::from_utf8(captured.0.lock().expect("lock").clone()).expect("utf-8");
        assert!(log.contains("hook failed"), "{log}");
        assert!(log.contains("[REDACTED:env:DEPLOY_TOKEN]"), "{log}");
        assert!(!log.contains(secret), "{log}");
    }

    #[test]
    fn a_value_with_characters_json_escapes_is_still_redacted() {
        let secret = "quote\"and\\slash-value";
        let redactor = Redactor::new(&[], [("API_TOKEN".to_owned(), secret.to_owned())], &[])
            .expect("builds")
            .with_json_escaped_values();
        let captured = Captured::default();
        let sink = Redacting {
            inner: captured.clone(),
            redactor: Some(Arc::new(redactor)),
        };
        let subscriber = tracing_subscriber::registry().with(fmt::layer().json().with_writer(sink));
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(stderr = secret, "hook failed; continuing");
        });
        let log = String::from_utf8(captured.0.lock().expect("lock").clone()).expect("utf-8");
        assert!(log.contains("[REDACTED:env:API_TOKEN]"), "{log}");
        assert!(!log.contains("slash-value"), "{log}");
    }
}
