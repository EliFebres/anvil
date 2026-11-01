//! How the session reaches the UI: a cloneable handle over the message
//! channel, and the tool hook that forwards every result to it.

use std::sync::Arc;

use anvil_core::provider::{ToolCall, ToolResult};
use anvil_core::tools::{HookDecision, HookFuture, ToolContext, ToolHook};
use serde_json::Value;
use tokio::sync::mpsc;

use super::{Level, UiMsg};

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
}
