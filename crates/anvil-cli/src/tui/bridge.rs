//! How the session reaches the UI: a cloneable handle over the message
//! channel.

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
