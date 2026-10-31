//! Signals: what the classifier found in a session, persisted until drafted.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use chrono::{DateTime, Utc};
use forge_types::{Priority, TicketKind};
use serde::{Deserialize, Serialize};
use ulid::Ulid;

use crate::error::TelemetryError;

/// Maximum signal title length, matching the ticket title limit.
const MAX_TITLE_CHARS: usize = 80;

/// One finding about a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signal {
    /// Unique id.
    pub id: Ulid,
    /// When it was found.
    pub at: DateTime<Utc>,
    /// The session it came from; the ticket's evidence ref.
    pub session: String,
    /// What was found.
    #[serde(flatten)]
    pub kind: SignalKind,
    /// The ticket it was drafted into or merged into, once handled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket: Option<Ulid>,
}

impl Signal {
    /// A fresh signal for a session.
    ///
    /// # Arguments
    ///
    /// * `session` - The session id.
    /// * `kind` - What was found.
    ///
    /// # Returns
    ///
    /// The signal, unhandled.
    pub fn new(session: &str, kind: SignalKind) -> Self {
        Self {
            id: Ulid::generate(),
            at: Utc::now(),
            session: session.to_owned(),
            kind,
            ticket: None,
        }
    }

    /// The dedup key: ticket kind plus normalized title.
    ///
    /// # Returns
    ///
    /// The key.
    pub fn key(&self) -> (TicketKind, String) {
        (
            self.kind.ticket_kind(),
            normalized_title(&self.kind.title()),
        )
    }
}

/// What the classifier can find.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "signal", rename_all = "snake_case")]
pub enum SignalKind {
    /// The model asked for a tool that does not exist, or a skill requires
    /// one that is not registered.
    ToolMissing {
        /// The tool name asked for.
        tool: String,
        /// The skill that requires it, when that is how it was found.
        skill: Option<String>,
    },
    /// One tool failed the same way three or more times in a session.
    ToolFailedRepeatedly {
        /// The tool name.
        tool: String,
        /// The error class the failures share.
        error_class: String,
        /// How many times.
        count: usize,
    },
    /// The Build/Review loop hit its cap.
    ReviewLoopExhausted {
        /// How many change requests the reviewer's final verdict made.
        items: usize,
    },
    /// Verify failed twice with the same check.
    VerifyLoopExhausted {
        /// The check's program name.
        check: String,
    },
    /// Two compactions within ten turns.
    CompactionThrash {
        /// Turns between the two compactions.
        turns_apart: usize,
    },
    /// The same permission prompt was answered yes many times, never always.
    PermissionFriction {
        /// The tool asked about.
        tool: String,
        /// How many times it was answered yes.
        count: usize,
    },
    /// A provider's median latency crossed the configured line.
    ModelSlow {
        /// The provider name.
        provider: String,
        /// Median latency in milliseconds.
        median_ms: u64,
        /// The line it crossed.
        limit_ms: u64,
    },
    /// A provider's error rate crossed the configured line.
    ModelErrors {
        /// The provider name.
        provider: String,
        /// Errors as a percentage of completions.
        rate_percent: u8,
        /// The line it crossed.
        limit_percent: u8,
    },
    /// The user typed `/ticket <text>`.
    UserFlag {
        /// The text.
        text: String,
    },
}

impl SignalKind {
    /// The kind of ticket this signal becomes.
    ///
    /// # Returns
    ///
    /// The kind.
    pub fn ticket_kind(&self) -> TicketKind {
        match self {
            Self::ToolMissing { skill: Some(_), .. } => TicketKind::Skill,
            Self::ToolMissing { skill: None, .. } => TicketKind::Tool,
            Self::ReviewLoopExhausted { .. } => TicketKind::Loop,
            Self::VerifyLoopExhausted { .. } => TicketKind::Prompt,
            Self::CompactionThrash { .. } => TicketKind::Memory,
            Self::ModelSlow { .. } | Self::ModelErrors { .. } => TicketKind::Model,
            Self::ToolFailedRepeatedly { .. }
            | Self::PermissionFriction { .. }
            | Self::UserFlag { .. } => TicketKind::Bug,
        }
    }

    /// How urgent the ticket is.
    ///
    /// # Returns
    ///
    /// P1 for a missing tool or a failing model, P2 otherwise.
    pub fn priority(&self) -> Priority {
        match self {
            Self::ToolMissing { .. } | Self::ModelErrors { .. } => Priority::P1,
            _ => Priority::P2,
        }
    }

    /// The ticket title, built from the signal's own fields so that the same
    /// finding in two sessions gets the same title.
    ///
    /// # Returns
    ///
    /// The title, at most 80 characters.
    pub fn title(&self) -> String {
        let title = match self {
            Self::ToolMissing { tool, skill: None } => format!("Missing tool `{tool}`"),
            Self::ToolMissing {
                tool,
                skill: Some(skill),
            } => format!("Skill `{skill}` requires missing tool `{tool}`"),
            Self::ToolFailedRepeatedly {
                tool, error_class, ..
            } => format!("Tool `{tool}` fails repeatedly ({error_class})"),
            Self::ReviewLoopExhausted { .. } => "Review loop exhausted".to_owned(),
            Self::VerifyLoopExhausted { check } => {
                format!("Verify loop exhausted on `{check}`")
            }
            Self::CompactionThrash { .. } => "Compaction thrash".to_owned(),
            Self::PermissionFriction { tool, .. } => {
                format!("Permission friction on `{tool}`")
            }
            Self::ModelSlow { provider, .. } => format!("Provider `{provider}` is slow"),
            Self::ModelErrors { provider, .. } => {
                format!("Provider `{provider}` errors too often")
            }
            Self::UserFlag { text } => format!("User flag: {}", text.lines().next().unwrap_or("")),
        };
        title.chars().take(MAX_TITLE_CHARS).collect()
    }
}

/// Lowercases a title, drops punctuation and collapses whitespace, so titles
/// that differ only in quoting or case compare equal.
///
/// # Arguments
///
/// * `title` - The title.
///
/// # Returns
///
/// The normalized form.
pub fn normalized_title(title: &str) -> String {
    title
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Reads a signals file.
///
/// # Arguments
///
/// * `path` - The file; missing means no signals.
///
/// # Returns
///
/// The signals in file order.
///
/// # Errors
///
/// When the file cannot be read or a line does not parse.
pub fn load_signals(path: &Path) -> Result<Vec<Signal>, TelemetryError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(TelemetryError::io(path, &error)),
    };
    text.lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(index, line)| {
            serde_json::from_str(line).map_err(|error| TelemetryError::Parse {
                path: path.to_path_buf(),
                line: index + 1,
                detail: error.to_string(),
            })
        })
        .collect()
}

/// Appends signals to a signals file.
///
/// # Arguments
///
/// * `path` - The file, created as needed.
/// * `signals` - What to append.
///
/// # Errors
///
/// When the file cannot be written.
pub fn append_signals(path: &Path, signals: &[Signal]) -> Result<(), TelemetryError> {
    if signals.is_empty() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| TelemetryError::io(parent, &e))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| TelemetryError::io(path, &e))?;
    file.write_all(lines(signals).as_bytes())
        .map_err(|e| TelemetryError::io(path, &e))
}

/// Rewrites a signals file, for example after signals were handled.
///
/// # Arguments
///
/// * `path` - The file.
/// * `signals` - The whole new content.
///
/// # Errors
///
/// When the file cannot be written.
pub fn save_signals(path: &Path, signals: &[Signal]) -> Result<(), TelemetryError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| TelemetryError::io(parent, &e))?;
    }
    std::fs::write(path, lines(signals)).map_err(|e| TelemetryError::io(path, &e))
}

/// Serializes signals as JSONL.
fn lines(signals: &[Signal]) -> String {
    signals
        .iter()
        .filter_map(|signal| serde_json::to_string(signal).ok())
        .map(|line| line + "\n")
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_map_to_ticket_kinds_and_priorities() {
        let missing = SignalKind::ToolMissing {
            tool: "browser".to_owned(),
            skill: None,
        };
        assert_eq!(missing.ticket_kind(), TicketKind::Tool);
        assert_eq!(missing.priority(), Priority::P1);
        let via_skill = SignalKind::ToolMissing {
            tool: "browser".to_owned(),
            skill: Some("web".to_owned()),
        };
        assert_eq!(via_skill.ticket_kind(), TicketKind::Skill);
        assert_eq!(
            SignalKind::ReviewLoopExhausted { items: 0 }.ticket_kind(),
            TicketKind::Loop
        );
        assert_eq!(
            SignalKind::VerifyLoopExhausted {
                check: "cargo".to_owned()
            }
            .ticket_kind(),
            TicketKind::Prompt
        );
        assert_eq!(
            SignalKind::CompactionThrash { turns_apart: 3 }.ticket_kind(),
            TicketKind::Memory
        );
        assert_eq!(
            SignalKind::ModelSlow {
                provider: "p".to_owned(),
                median_ms: 1,
                limit_ms: 1
            }
            .ticket_kind(),
            TicketKind::Model
        );
        assert_eq!(
            SignalKind::UserFlag {
                text: "x".to_owned()
            }
            .ticket_kind(),
            TicketKind::Bug
        );
        assert_eq!(
            SignalKind::UserFlag {
                text: "x".to_owned()
            }
            .priority(),
            Priority::P2
        );
    }

    #[test]
    fn titles_are_deterministic_and_capped() {
        let a = SignalKind::ToolMissing {
            tool: "browser".to_owned(),
            skill: None,
        };
        assert_eq!(a.title(), "Missing tool `browser`");
        assert_eq!(a.title(), a.clone().title());
        let long = SignalKind::UserFlag {
            text: "x".repeat(200),
        };
        assert_eq!(long.title().chars().count(), MAX_TITLE_CHARS);
        assert_eq!(
            normalized_title("Missing tool `browser`"),
            normalized_title("missing   tool BROWSER!")
        );
        assert_eq!(normalized_title("A, b: c"), "a b c");
    }

    #[test]
    fn signals_round_trip_through_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("t").join("signals.jsonl");
        assert!(load_signals(&path).expect("missing is empty").is_empty());
        let mut first = Signal::new(
            "s1",
            SignalKind::ToolMissing {
                tool: "x".to_owned(),
                skill: None,
            },
        );
        let second = Signal::new("s2", SignalKind::CompactionThrash { turns_apart: 2 });
        append_signals(&path, std::slice::from_ref(&first)).expect("append");
        append_signals(&path, std::slice::from_ref(&second)).expect("append");
        assert_eq!(
            load_signals(&path).expect("load"),
            vec![first.clone(), second.clone()]
        );
        first.ticket = Some(Ulid::generate());
        save_signals(&path, &[first.clone(), second.clone()]).expect("save");
        let back = load_signals(&path).expect("load");
        assert_eq!(back[0].ticket, first.ticket);
        assert_eq!(back[1], second);
        std::fs::write(&path, "nope\n").expect("write");
        assert!(matches!(
            load_signals(&path),
            Err(TelemetryError::Parse { line: 1, .. })
        ));
    }
}
