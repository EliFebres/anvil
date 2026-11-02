//! Where a session's status lines go: stderr in plain mode, the transcript
//! under the full-screen UI, where a stray `eprintln!` would tear the screen.

use crate::tui::{Level, UiHandle};

/// The sink for a session's status lines.
#[derive(Debug, Clone)]
pub enum Console {
    /// Print to stderr, as the line renderer does.
    Stderr,
    /// Send to the full-screen UI's transcript.
    Ui(UiHandle),
}

impl Console {
    /// Shows a routine line.
    ///
    /// # Arguments
    ///
    /// * `text` - The line, printed as given.
    pub fn notice(&self, text: impl AsRef<str>) {
        self.emit(text.as_ref(), Level::Info);
    }

    /// Shows a line worth noticing.
    ///
    /// # Arguments
    ///
    /// * `text` - The line, printed as given.
    pub fn warn(&self, text: impl AsRef<str>) {
        self.emit(text.as_ref(), Level::Warn);
    }

    /// Shows a failure.
    ///
    /// # Arguments
    ///
    /// * `text` - The line, printed as given.
    pub fn error(&self, text: impl AsRef<str>) {
        self.emit(text.as_ref(), Level::Error);
    }

    /// Ends the model's streamed text: a newline on stdout in plain mode,
    /// nothing under the UI, where the transcript already separates entries.
    pub fn end_text(&self) {
        if matches!(self, Console::Stderr) {
            println!();
        }
    }

    /// The UI handle, when the session has a full-screen UI.
    ///
    /// # Returns
    ///
    /// The handle.
    pub fn ui(&self) -> Option<&UiHandle> {
        match self {
            Console::Stderr => None,
            Console::Ui(ui) => Some(ui),
        }
    }

    /// Sends one line.
    fn emit(&self, text: &str, level: Level) {
        match self {
            Console::Stderr => eprintln!("{text}"),
            Console::Ui(ui) => ui.notice(text, level),
        }
    }
}
