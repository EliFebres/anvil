//! How the session reaches the UI: a cloneable handle over the message
//! channel, the prompt and gate that ask through it, the tool hook that
//! forwards every result to it, and the cancel signal that comes back.

use std::sync::Arc;

use anvil_core::agent::{PlanDecision, PlanGate};
use anvil_core::provider::{ToolCall, ToolResult};
use anvil_core::tools::{
    HookDecision, HookFuture, PermissionPrompt, PermissionRequest, PromptAnswer, ToolContext,
    ToolHook,
};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot, watch};

use super::{Level, UiMsg, UiRequest};

/// A sender into the UI. Dropping every handle closes the UI.
#[derive(Debug, Clone)]
pub struct UiHandle {
    tx: mpsc::UnboundedSender<UiMsg>,
}

impl UiHandle {
    /// A handle over a channel.
    ///
    /// # Arguments
    ///
    /// * `tx` - The sender the UI task reads from.
    ///
    /// # Returns
    ///
    /// The handle.
    pub fn new(tx: mpsc::UnboundedSender<UiMsg>) -> Self {
        Self { tx }
    }

    /// Sends a message; a closed UI drops it silently.
    ///
    /// # Arguments
    ///
    /// * `msg` - The message.
    pub fn send(&self, msg: UiMsg) {
        let _ = self.tx.send(msg);
    }

    /// Shows a line at a level.
    ///
    /// # Arguments
    ///
    /// * `text` - The text.
    /// * `level` - How loud.
    pub fn notice(&self, text: impl Into<String>, level: Level) {
        self.send(UiMsg::Notice {
            text: text.into(),
            level,
        });
    }
}

/// Waits for the UI's answer from the session's thread.
///
/// The prompt hooks are synchronous and run on the thread that drives the
/// session, inside the runtime, so this steps out of the runtime for the
/// wait. It needs the multi-thread runtime; a current-thread runtime
/// panics here.
///
/// # Arguments
///
/// * `reply` - The channel the UI answers on.
///
/// # Returns
///
/// The answer, or `None` when the UI went away without one.
pub fn block_on_reply<T>(reply: oneshot::Receiver<T>) -> Option<T> {
    tokio::task::block_in_place(|| reply.blocking_recv().ok())
}

/// Resolves when the user cancels the current turn, and never when there
/// is no UI or the UI has gone.
///
/// # Arguments
///
/// * `cancel` - The cancel signal, or `None` in plain mode.
pub async fn wait_cancel(cancel: Option<&mut watch::Receiver<u64>>) {
    if let Some(cancel) = cancel
        && cancel.changed().await.is_ok()
    {
        return;
    }
    std::future::pending::<()>().await;
}

/// Asks each `ask` permission through the UI's modal. A closed UI
/// declines.
#[derive(Debug, Clone)]
pub struct TuiPrompt {
    ui: UiHandle,
}

impl TuiPrompt {
    /// A prompt over a UI handle.
    ///
    /// # Arguments
    ///
    /// * `ui` - The UI.
    ///
    /// # Returns
    ///
    /// The prompt.
    pub fn new(ui: UiHandle) -> Self {
        Self { ui }
    }
}

impl PermissionPrompt for TuiPrompt {
    fn ask(&self, request: &PermissionRequest) -> PromptAnswer {
        let (reply, answer) = oneshot::channel();
        self.ui.send(UiMsg::Request(UiRequest::Permission {
            request: request.clone(),
            reply,
        }));
        block_on_reply(answer).unwrap_or(PromptAnswer::No)
    }
}

/// Reviews each plan through the UI's modal. A closed UI aborts.
#[derive(Debug, Clone)]
pub struct TuiGate {
    ui: UiHandle,
}

impl TuiGate {
    /// A gate over a UI handle.
    ///
    /// # Arguments
    ///
    /// * `ui` - The UI.
    ///
    /// # Returns
    ///
    /// The gate.
    pub fn new(ui: UiHandle) -> Self {
        Self { ui }
    }
}

impl PlanGate for TuiGate {
    fn review(&self, plan: &str) -> PlanDecision {
        let (reply, decision) = oneshot::channel();
        self.ui.send(UiMsg::Request(UiRequest::Plan {
            plan: plan.to_owned(),
            reply,
        }));
        block_on_reply(decision).unwrap_or(PlanDecision::Abort)
    }
}

/// A tool hook that sends every result to the UI, then defers to the hook
/// that was there before, if any. The result is the capped text the model
/// sees, so the expanded tool entry shows exactly that.
pub struct ResultTee {
    inner: Option<Arc<dyn ToolHook>>,
    ui: UiHandle,
}

impl ResultTee {
    /// A tee in front of an optional inner hook.
    ///
    /// # Arguments
    ///
    /// * `inner` - The hook to keep running, if any.
    /// * `ui` - Where results go.
    ///
    /// # Returns
    ///
    /// The hook.
    pub fn new(inner: Option<Arc<dyn ToolHook>>, ui: UiHandle) -> Self {
        Self { inner, ui }
    }
}

impl ToolHook for ResultTee {
    fn before<'a>(
        &'a self,
        call: &'a ToolCall,
        args: &'a Value,
        ctx: &'a ToolContext,
    ) -> HookFuture<'a, HookDecision> {
        Box::pin(async move {
            match &self.inner {
                Some(inner) => inner.before(call, args, ctx).await,
                None => HookDecision::Allow,
            }
        })
    }

    fn after<'a>(
        &'a self,
        call: &'a ToolCall,
        args: &'a Value,
        result: &'a ToolResult,
        ctx: &'a ToolContext,
    ) -> HookFuture<'a, Option<String>> {
        Box::pin(async move {
            self.ui.send(UiMsg::ToolResult {
                call_id: result.call_id.clone(),
                text: result.content.clone(),
                is_error: result.is_error,
            });
            match &self.inner {
                Some(inner) => inner.after(call, args, result, ctx).await,
                None => None,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    struct Recording(Mutex<Vec<&'static str>>);

    impl ToolHook for Recording {
        fn before<'a>(
            &'a self,
            _call: &'a ToolCall,
            _args: &'a Value,
            _ctx: &'a ToolContext,
        ) -> HookFuture<'a, HookDecision> {
            Box::pin(async move {
                self.0.lock().expect("lock").push("before");
                HookDecision::Allow
            })
        }

        fn after<'a>(
            &'a self,
            _call: &'a ToolCall,
            _args: &'a Value,
            _result: &'a ToolResult,
            _ctx: &'a ToolContext,
        ) -> HookFuture<'a, Option<String>> {
            Box::pin(async move {
                self.0.lock().expect("lock").push("after");
                Some("note".to_owned())
            })
        }
    }

    #[tokio::test]
    async fn result_tee_forwards_then_delegates() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let inner = Arc::new(Recording(Mutex::new(Vec::new())));
        let tee = ResultTee::new(Some(inner.clone()), UiHandle::new(tx));
        let call = ToolCall {
            id: "c1".to_owned(),
            name: "shell".to_owned(),
            arguments_json: "{}".to_owned(),
        };
        let ctx = ToolContext::new(std::env::temp_dir());
        assert_eq!(
            tee.before(&call, &Value::Null, &ctx).await,
            HookDecision::Allow
        );
        let result = ToolResult {
            call_id: "c1".to_owned(),
            content: "out".to_owned(),
            is_error: true,
        };
        assert_eq!(
            tee.after(&call, &Value::Null, &result, &ctx).await,
            Some("note".to_owned())
        );
        assert_eq!(*inner.0.lock().expect("lock"), vec!["before", "after"]);
        match rx.try_recv() {
            Ok(UiMsg::ToolResult {
                call_id,
                text,
                is_error,
            }) => {
                assert_eq!(call_id, "c1");
                assert_eq!(text, "out");
                assert!(is_error);
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prompt_and_gate_round_trip_through_the_ui() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let ui = UiHandle::new(tx);
        let answering = tokio::spawn(async move {
            let mut seen = Vec::new();
            while let Some(msg) = rx.recv().await {
                match msg {
                    UiMsg::Request(UiRequest::Permission { request, reply }) => {
                        seen.push(request.tool);
                        let _ = reply.send(PromptAnswer::Always);
                    }
                    UiMsg::Request(UiRequest::Plan { plan, reply }) => {
                        seen.push(plan);
                        let _ = reply.send(PlanDecision::Revise("shorter".to_owned()));
                    }
                    other => panic!("unexpected message: {other:?}"),
                }
            }
            seen
        });
        let request = PermissionRequest {
            tool: "shell".to_owned(),
            target: Some("ls".to_owned()),
            arguments: Value::Null,
        };
        assert_eq!(
            TuiPrompt::new(ui.clone()).ask(&request),
            PromptAnswer::Always
        );
        assert_eq!(
            TuiGate::new(ui.clone()).review("# Plan"),
            PlanDecision::Revise("shorter".to_owned())
        );
        drop(ui);
        assert_eq!(
            answering.await.expect("answering task"),
            vec!["shell".to_owned(), "# Plan".to_owned()]
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_closed_ui_gives_the_safe_answers() {
        let (tx, rx) = mpsc::unbounded_channel();
        drop(rx);
        let ui = UiHandle::new(tx);
        let request = PermissionRequest {
            tool: "shell".to_owned(),
            target: None,
            arguments: Value::Null,
        };
        assert_eq!(TuiPrompt::new(ui.clone()).ask(&request), PromptAnswer::No);
        assert_eq!(TuiGate::new(ui).review("# Plan"), PlanDecision::Abort);
    }

    #[tokio::test]
    async fn wait_cancel_resolves_on_a_bump_and_never_without_a_ui() {
        let (tx, mut rx) = watch::channel(0u64);
        let waited =
            tokio::time::timeout(std::time::Duration::from_millis(50), wait_cancel(None)).await;
        assert!(waited.is_err(), "plain mode never cancels");
        tx.send_modify(|n| *n += 1);
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            wait_cancel(Some(&mut rx)),
        )
        .await
        .expect("a bump cancels");
        drop(tx);
        let gone = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            wait_cancel(Some(&mut rx)),
        )
        .await;
        assert!(gone.is_err(), "a gone UI never cancels");
    }
}
