//! The `status.json` a session keeps in its folder so other tools can tell
//! whether it is working, stuck or waiting for the owner.
//!
//! This module is the format and its validator. Writing the file and
//! deciding the state live in `anvil-cli`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::ValidationError;

/// The file name inside a session folder.
pub const STATUS_FILE: &str = "status.json";

/// The lock file the running session holds, so a reader can tell a live
/// session from one that died.
pub const LOCK_FILE: &str = "status.lock";

/// The only schema version this crate reads and writes.
pub const SCHEMA_VERSION: u32 = 1;

/// What a session is doing, as the owner needs to see it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    /// The agent is running.
    Working,
    /// A permission prompt, a plan approval or a question waits for the owner.
    NeedsInput,
    /// A budget ran out, the provider cannot be reached, or the loop stopped unfinished.
    Blocked,
    /// The task was finished.
    Done,
    /// An error aborted the command.
    Failed,
}

impl SessionState {
    /// The snake_case name, as written in the file.
    ///
    /// # Returns
    ///
    /// The name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::NeedsInput => "needs_input",
            Self::Blocked => "blocked",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }

    /// Whether a live process is expected to keep this state current.
    ///
    /// # Returns
    ///
    /// True for `working` and `needs_input`.
    pub fn is_live(self) -> bool {
        matches!(self, Self::Working | Self::NeedsInput)
    }
}

/// Which step of the agent loop a session is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopState {
    /// Explore and write the plan.
    Plan,
    /// Implement the plan.
    Build,
    /// A second model reviews the diff.
    Review,
    /// Run the project's checks.
    Verify,
}

impl LoopState {
    /// The lowercase name, as written in the file.
    ///
    /// # Returns
    ///
    /// The name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Build => "build",
            Self::Review => "review",
            Self::Verify => "verify",
        }
    }
}

/// What the session has used so far.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionSpend {
    /// Prompt tokens that were not served from cache.
    pub input_tokens: u64,
    /// Generated tokens.
    pub output_tokens: u64,
    /// Prompt tokens served from cache.
    pub cached_tokens: u64,
    /// Estimated cost in USD; `None` when the provider has no price configured.
    pub cost_usd: Option<f64>,
}

impl SessionSpend {
    /// Every token counted.
    ///
    /// # Returns
    ///
    /// Input, output and cached tokens, saturating.
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.cached_tokens)
    }
}

/// The content of one session's `status.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionStatus {
    /// The format version, [`SCHEMA_VERSION`].
    pub schema: u32,
    /// The session id.
    pub session_id: String,
    /// The project directory.
    pub project: String,
    /// The first line of the task.
    pub task: String,
    /// What the session is doing.
    pub state: SessionState,
    /// The loop step, or `None` outside a run (chat) and before it starts.
    pub loop_state: Option<LoopState>,
    /// One line saying more, such as why a session is blocked.
    pub detail: String,
    /// Tokens and cost so far.
    pub spend: SessionSpend,
    /// When the session started.
    pub started_at: DateTime<Utc>,
    /// When this file was last written.
    pub updated_at: DateTime<Utc>,
}

impl SessionStatus {
    /// Checks the rules the file format promises.
    ///
    /// # Returns
    ///
    /// `Ok(())` when the schema is current, the id is set and the task and
    /// detail are single lines.
    ///
    /// # Errors
    ///
    /// [`ValidationError::Status`] naming the first broken field.
    pub fn validate(&self) -> Result<(), ValidationError> {
        let fail = |reason: String| Err(ValidationError::Status { reason });
        if self.schema != SCHEMA_VERSION {
            return fail(format!(
                "`schema` is {}; this build reads {SCHEMA_VERSION}",
                self.schema
            ));
        }
        if self.session_id.trim().is_empty() {
            return fail("`session_id` must not be empty".to_owned());
        }
        if self.task.contains('\n') {
            return fail("`task` must be a single line".to_owned());
        }
        if self.detail.contains('\n') {
            return fail("`detail` must be a single line".to_owned());
        }
        Ok(())
    }

    /// Reads and validates a `status.json` text.
    ///
    /// # Arguments
    ///
    /// * `text` - The file content.
    ///
    /// # Returns
    ///
    /// The status.
    ///
    /// # Errors
    ///
    /// [`ValidationError::Status`] when the text is not JSON of this shape
    /// or breaks a rule of [`SessionStatus::validate`].
    pub fn parse(text: &str) -> Result<Self, ValidationError> {
        let status: Self = serde_json::from_str(text).map_err(|error| ValidationError::Status {
            reason: error.to_string(),
        })?;
        status.validate()?;
        Ok(status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> SessionStatus {
        let at = DateTime::parse_from_rfc3339("2026-11-01T10:00:00Z")
            .expect("time")
            .with_timezone(&Utc);
        SessionStatus {
            schema: SCHEMA_VERSION,
            session_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".to_owned(),
            project: "/work/app".to_owned(),
            task: "fix the bug".to_owned(),
            state: SessionState::NeedsInput,
            loop_state: Some(LoopState::Plan),
            detail: "waiting for plan approval".to_owned(),
            spend: SessionSpend::default(),
            started_at: at,
            updated_at: at,
        }
    }

    #[test]
    fn round_trips_with_snake_case_names() {
        let text = serde_json::to_string(&sample()).expect("serialize");
        assert!(text.contains("\"state\":\"needs_input\""), "{text}");
        assert!(text.contains("\"loop_state\":\"plan\""), "{text}");
        assert!(
            text.contains("\"started_at\":\"2026-11-01T10:00:00Z\""),
            "{text}"
        );
        assert_eq!(SessionStatus::parse(&text).expect("parse"), sample());
    }

    #[test]
    fn rejects_a_wrong_schema_and_multi_line_text() {
        let mut status = sample();
        status.schema = 2;
        assert!(status.validate().is_err());
        let mut status = sample();
        status.task = "a\nb".to_owned();
        assert!(status.validate().is_err());
        let mut status = sample();
        status.detail = "a\nb".to_owned();
        assert!(status.validate().is_err());
        assert!(SessionStatus::parse("{").is_err());
    }

    #[test]
    fn only_working_and_needs_input_are_live() {
        assert!(SessionState::Working.is_live());
        assert!(SessionState::NeedsInput.is_live());
        assert!(!SessionState::Blocked.is_live());
        assert!(!SessionState::Done.is_live());
        assert!(!SessionState::Failed.is_live());
    }
}
