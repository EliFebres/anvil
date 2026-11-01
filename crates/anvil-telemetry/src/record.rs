//! One telemetry record and the rules that keep content out of it.
//!
//! The CLI builds a [`RecordKind`] from each loop event. Everything that
//! could carry project or model text goes through a constructor here that
//! reduces it to a size, a digest, a class or a scrubbed line; the raw
//! strings never reach the file.

use anvil_types::ValidationError;
use anvil_types::boundary;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A stable 64-bit FNV-1a digest. Identical to anvil-core's, so digests of
/// tool arguments made here match digests of tool results made there.
///
/// # Arguments
///
/// * `bytes` - What to digest.
///
/// # Returns
///
/// The digest.
pub fn digest(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    bytes.iter().fold(OFFSET, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(PRIME)
    })
}

/// Formats a digest as 16 lowercase hex digits.
///
/// # Arguments
///
/// * `digest` - The digest.
///
/// # Returns
///
/// The hex text.
pub fn hex(digest: u64) -> String {
    format!("{digest:016x}")
}

/// Tokens one completion used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Tokens {
    /// Uncached prompt tokens.
    pub input: u64,
    /// Output tokens.
    pub output: u64,
    /// Cached prompt tokens.
    pub cached: u64,
}

/// How a session was started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMode {
    /// `anvil run`.
    Run,
    /// `anvil chat`.
    Chat,
    /// `anvil resume`.
    Resume,
}

/// Why a completion stopped, with provider-specific reasons collapsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopKind {
    /// The model finished its reply.
    EndTurn,
    /// The model asked for tools.
    ToolUse,
    /// The output cap was hit.
    MaxTokens,
    /// Anything else.
    Other,
}

/// How a tool call ended. Mirrors anvil-core's outcome names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOutcomeKind {
    /// Ran and succeeded.
    Completed,
    /// Ran and reported an error.
    Failed,
    /// A permission rule denied it.
    DeniedByRule,
    /// The user said no.
    DeclinedByUser,
    /// A hook or a built-in block list stopped it.
    Blocked,
    /// No tool has that name.
    UnknownTool,
    /// The arguments were not valid JSON.
    InvalidArguments,
}

impl ToolOutcomeKind {
    /// The snake_case name, as it appears in the file.
    ///
    /// # Returns
    ///
    /// The name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::DeniedByRule => "denied_by_rule",
            Self::DeclinedByUser => "declined_by_user",
            Self::Blocked => "blocked",
            Self::UnknownTool => "unknown_tool",
            Self::InvalidArguments => "invalid_arguments",
        }
    }
}

/// What the user answered to a permission prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionAnswer {
    /// Allowed once.
    Yes,
    /// Declined.
    No,
    /// Allowed and saved as a rule.
    Always,
}

/// A warning's class; the message itself is never recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WarningClass {
    /// Compaction failed and the loop continued uncompacted.
    CompactionFailed,
    /// A check command could not run or failed.
    CheckFailed,
    /// The reviewer gave no usable verdict.
    ReviewUnparsed,
    /// A hook misbehaved.
    Hook,
    /// Anything else.
    Other,
}

impl WarningClass {
    /// Classifies a warning by its leading words.
    ///
    /// # Arguments
    ///
    /// * `message` - The warning text.
    ///
    /// # Returns
    ///
    /// The class.
    pub fn of(message: &str) -> Self {
        let lower = message.to_ascii_lowercase();
        if lower.starts_with("compaction") {
            Self::CompactionFailed
        } else if lower.starts_with("could not run") || lower.contains("check") {
            Self::CheckFailed
        } else if lower.contains("reviewer") || lower.contains("verdict") {
            Self::ReviewUnparsed
        } else if lower.contains("hook") {
            Self::Hook
        } else {
            Self::Other
        }
    }
}

/// How a session ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum SessionOutcome {
    /// The loop reached Done.
    Done,
    /// The loop stopped early.
    Incomplete {
        /// The reason's tag, such as `review_rounds_exhausted`.
        reason: String,
    },
    /// The loop stopped early after a tool call was denied.
    Denied {
        /// The reason's tag.
        reason: String,
    },
    /// The provider failed.
    ProviderError {
        /// The error's tag, such as `http` or `timeout`.
        kind: String,
    },
    /// A chat session ended; chats have no completion status.
    Chat,
}

impl SessionOutcome {
    /// The failure class used by `anvil stats`, such as
    /// `incomplete:review_rounds_exhausted`.
    ///
    /// # Returns
    ///
    /// The class.
    pub fn class(&self) -> String {
        match self {
            Self::Done => "done".to_owned(),
            Self::Incomplete { reason } => format!("incomplete:{reason}"),
            Self::Denied { reason } => format!("denied:{reason}"),
            Self::ProviderError { kind } => format!("provider_error:{kind}"),
            Self::Chat => "chat".to_owned(),
        }
    }
}

/// What happened, without the content it happened to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum RecordKind {
    /// A session began.
    SessionStarted {
        /// How.
        mode: SessionMode,
    },
    /// The loop entered a state.
    StateEntered {
        /// The state name.
        state: String,
        /// How many times this state has been entered, starting at 1.
        visit: u32,
    },
    /// One model completion ended.
    Turn {
        /// The state it ran in, or `chat`.
        state: String,
        /// The provider name from config.
        provider: String,
        /// The model name from config.
        model: String,
        /// Tokens used.
        tokens: Tokens,
        /// Wall-clock milliseconds.
        latency_ms: u64,
        /// Retries before the request got through.
        retries: u32,
        /// Why the model stopped.
        stop_reason: StopKind,
    },
    /// The model asked for a tool.
    ToolCall {
        /// The tool name.
        name: String,
        /// Byte length of the arguments JSON.
        args_bytes: usize,
        /// Digest of the arguments JSON.
        args_digest: String,
    },
    /// A tool call finished.
    ToolResult {
        /// The tool name.
        name: String,
        /// How it ended.
        outcome: ToolOutcomeKind,
        /// A class for grouping failures; `None` when it completed.
        error_class: Option<String>,
        /// Byte length of the full result.
        bytes: usize,
        /// Whether the model saw a truncated copy.
        truncated: bool,
        /// Digest of the full result.
        digest: String,
    },
    /// The user answered a permission prompt.
    Permission {
        /// The tool name.
        tool: String,
        /// Digest of the rule `always` would have written.
        rule_digest: String,
        /// The answer.
        answer: PermissionAnswer,
    },
    /// The reviewer answered. The change requests themselves are not
    /// recorded: the reviewer reads the diff and quotes it, so its text is
    /// project content.
    Review {
        /// Whether it approved.
        approved: bool,
        /// How many change requests it made.
        items: usize,
        /// Digest of each change request, so repeats can be told apart.
        digests: Vec<String>,
    },
    /// A check command finished.
    Check {
        /// The program name only.
        program: String,
        /// Digest of the whole command line.
        command_digest: String,
        /// Whether it passed.
        passed: bool,
    },
    /// Older turns were replaced by a summary.
    Compacted {
        /// Messages the summary replaced.
        dropped: usize,
        /// The summarizer's provider name.
        provider: String,
        /// The summarizer's model name.
        model: String,
        /// Tokens the summarizer used, when reported.
        tokens: Option<Tokens>,
    },
    /// The provider failed.
    ProviderError {
        /// The provider name from config.
        provider: String,
        /// The error's tag.
        kind: String,
        /// The HTTP status, when there was one.
        http_status: Option<u16>,
    },
    /// The loop warned.
    Warning {
        /// The class.
        class: WarningClass,
    },
    /// A skill requires a tool that is not registered.
    SkillRequires {
        /// The skill name.
        skill: String,
        /// The missing tool.
        tool: String,
    },
    /// The user typed `/ticket <text>`.
    UserFlag {
        /// The text, which passed the boundary check.
        text: String,
    },
    /// The session ended.
    SessionEnded {
        /// How.
        #[serde(flatten)]
        outcome: SessionOutcome,
    },
}

impl RecordKind {
    /// A tool call record from the arguments JSON.
    ///
    /// # Arguments
    ///
    /// * `name` - The tool name.
    /// * `arguments_json` - The arguments; digested, not stored.
    ///
    /// # Returns
    ///
    /// The record.
    pub fn tool_call(name: &str, arguments_json: &str) -> Self {
        Self::ToolCall {
            name: name.to_owned(),
            args_bytes: arguments_json.len(),
            args_digest: hex(digest(arguments_json.as_bytes())),
        }
    }

    /// A tool result record.
    ///
    /// The error class lets the classifier group repeats of one failure. For
    /// a tool that ran and failed it digests the first line of the result
    /// with case and digits removed; for shell commands that line is usually
    /// program output, so shell failures rarely share a class. Every other
    /// outcome is its own class.
    ///
    /// # Arguments
    ///
    /// * `name` - The tool name.
    /// * `outcome` - How it ended.
    /// * `preview` - The first line of the result; digested, not stored.
    /// * `bytes` - Byte length of the full result.
    /// * `truncated` - Whether the model saw a truncated copy.
    /// * `digest` - Digest of the full result.
    ///
    /// # Returns
    ///
    /// The record.
    pub fn tool_result(
        name: &str,
        outcome: ToolOutcomeKind,
        preview: &str,
        bytes: usize,
        truncated: bool,
        digest: u64,
    ) -> Self {
        let error_class = match outcome {
            ToolOutcomeKind::Completed => None,
            ToolOutcomeKind::Failed => {
                let normalized: String = preview
                    .to_lowercase()
                    .chars()
                    .filter(|c| !c.is_ascii_digit())
                    .collect::<String>()
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                let class = hex(self::digest(normalized.as_bytes()));
                Some(format!("failed:{}", &class[..8]))
            }
            other => Some(other.name().to_owned()),
        };
        Self::ToolResult {
            name: name.to_owned(),
            outcome,
            error_class,
            bytes,
            truncated,
            digest: hex(digest),
        }
    }

    /// A permission record keyed by the rule `always` would write.
    ///
    /// # Arguments
    ///
    /// * `tool` - The tool name.
    /// * `rule` - The rule text, such as `shell:cargo *`; digested, not stored.
    /// * `answer` - The answer.
    ///
    /// # Returns
    ///
    /// The record.
    pub fn permission(tool: &str, rule: &str, answer: PermissionAnswer) -> Self {
        Self::Permission {
            tool: tool.to_owned(),
            rule_digest: hex(digest(rule.as_bytes())),
            answer,
        }
    }

    /// A review record that keeps a count and digests of the change
    /// requests, never their text.
    ///
    /// # Arguments
    ///
    /// * `approved` - Whether the reviewer approved.
    /// * `changes` - The reviewer's change requests; digested, not stored.
    ///
    /// # Returns
    ///
    /// The record.
    pub fn review(approved: bool, changes: &[String]) -> Self {
        Self::Review {
            approved,
            items: changes.len(),
            digests: changes
                .iter()
                .map(|item| hex(digest(item.trim().as_bytes())))
                .collect(),
        }
    }

    /// A check record that keeps the program name only.
    ///
    /// # Arguments
    ///
    /// * `command` - The command line; digested, not stored.
    /// * `passed` - Whether it passed.
    ///
    /// # Returns
    ///
    /// The record.
    pub fn check(command: &str, passed: bool) -> Self {
        Self::Check {
            program: program_name(command),
            command_digest: hex(digest(command.as_bytes())),
            passed,
        }
    }

    /// A user flag, refused when the text fails the boundary check.
    ///
    /// # Arguments
    ///
    /// * `text` - What the user typed after `/ticket`.
    ///
    /// # Returns
    ///
    /// The record.
    ///
    /// # Errors
    ///
    /// When the text names a path outside the repository or holds too much
    /// code.
    pub fn user_flag(text: &str) -> Result<Self, ValidationError> {
        boundary::check_text("ticket", text)?;
        Ok(Self::UserFlag {
            text: text.trim().to_owned(),
        })
    }

    /// The tag written to the file, such as `tool_result`.
    ///
    /// # Returns
    ///
    /// The tag.
    pub fn tag(&self) -> &'static str {
        match self {
            Self::SessionStarted { .. } => "session_started",
            Self::StateEntered { .. } => "state_entered",
            Self::Turn { .. } => "turn",
            Self::ToolCall { .. } => "tool_call",
            Self::ToolResult { .. } => "tool_result",
            Self::Permission { .. } => "permission",
            Self::Review { .. } => "review",
            Self::Check { .. } => "check",
            Self::Compacted { .. } => "compacted",
            Self::ProviderError { .. } => "provider_error",
            Self::Warning { .. } => "warning",
            Self::SkillRequires { .. } => "skill_requires",
            Self::UserFlag { .. } => "user_flag",
            Self::SessionEnded { .. } => "session_ended",
        }
    }
}

/// The program a command line runs, without its directory.
fn program_name(command: &str) -> String {
    let first = command.split_whitespace().next().unwrap_or("");
    first.rsplit(['/', '\\']).next().unwrap_or(first).to_owned()
}

/// One line of a telemetry file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// When it happened.
    pub ts: DateTime<Utc>,
    /// The session id, a ULID.
    pub session: String,
    /// Digest of the project path, so the home copy can tell projects apart
    /// without naming them.
    pub project: String,
    /// What happened.
    #[serde(flatten)]
    pub kind: RecordKind,
}

impl Record {
    /// A record stamped with the current time.
    ///
    /// # Arguments
    ///
    /// * `session` - The session id.
    /// * `project` - The project digest.
    /// * `kind` - What happened.
    ///
    /// # Returns
    ///
    /// The record.
    pub fn now(session: &str, project: &str, kind: RecordKind) -> Self {
        Self {
            ts: Utc::now(),
            session: session.to_owned(),
            project: project.to_owned(),
            kind,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_matches_anvil_core_vectors() {
        assert_eq!(digest(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(digest(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(hex(0xaf63_dc4c_8601_ec8c), "af63dc4c8601ec8c");
    }

    #[test]
    fn tool_call_keeps_size_and_digest_only() {
        let record = RecordKind::tool_call("write_file", r#"{"path":"secret.rs"}"#);
        let json = serde_json::to_string(&record).expect("json");
        assert!(!json.contains("secret"));
        assert!(json.contains(r#""event":"tool_call""#));
        assert!(json.contains(r#""args_bytes":20"#));
    }

    #[test]
    fn error_class_groups_failures_without_digits() {
        let a = RecordKind::tool_result("shell", ToolOutcomeKind::Failed, "error 12", 5, false, 1);
        let b = RecordKind::tool_result("shell", ToolOutcomeKind::Failed, "ERROR 99", 5, false, 2);
        let c = RecordKind::tool_result("shell", ToolOutcomeKind::Failed, "other", 5, false, 3);
        let class = |record: &RecordKind| match record {
            RecordKind::ToolResult { error_class, .. } => error_class.clone(),
            _ => None,
        };
        assert_eq!(class(&a), class(&b));
        assert_ne!(class(&a), class(&c));
        assert!(class(&a).expect("class").starts_with("failed:"));
        assert!(
            !serde_json::to_string(&a)
                .expect("json")
                .contains("error 12")
        );
        let unknown =
            RecordKind::tool_result("nope", ToolOutcomeKind::UnknownTool, "x", 1, false, 0);
        assert_eq!(class(&unknown).as_deref(), Some("unknown_tool"));
        let ok = RecordKind::tool_result("glob", ToolOutcomeKind::Completed, "x", 1, false, 0);
        assert_eq!(class(&ok), None);
    }

    #[test]
    fn review_keeps_counts_and_digests_only() {
        let items = vec![
            "Rename the helper in src/lib.rs to parse_args".to_owned(),
            "Rename the helper in src/lib.rs to parse_args ".to_owned(),
            "Add a test for the parser".to_owned(),
        ];
        let record = RecordKind::review(false, &items);
        let RecordKind::Review {
            items: count,
            digests,
            approved,
        } = &record
        else {
            panic!("review record");
        };
        assert!(!approved);
        assert_eq!(*count, 3);
        assert_eq!(
            digests[0], digests[1],
            "whitespace does not change the digest"
        );
        assert_ne!(digests[0], digests[2]);
        let json = serde_json::to_string(&record).expect("json");
        assert!(!json.contains("parse_args"));
        assert!(!json.contains("lib.rs"));
    }

    #[test]
    fn check_keeps_the_program_name_only() {
        let RecordKind::Check {
            program,
            command_digest,
            passed,
        } = RecordKind::check("C:\\proj\\scripts\\check.ps1 --fast", true)
        else {
            panic!("check record");
        };
        assert_eq!(program, "check.ps1");
        assert_eq!(command_digest.len(), 16);
        assert!(passed);
        assert!(matches!(
            RecordKind::check("cargo test", false),
            RecordKind::Check { program, .. } if program == "cargo"
        ));
    }

    #[test]
    fn user_flag_is_boundary_checked() {
        assert!(RecordKind::user_flag("the glob tool ignores .gitignore").is_ok());
        assert!(matches!(
            RecordKind::user_flag("see /home/me/proj/x.rs"),
            Err(ValidationError::OutsidePath { .. })
        ));
    }

    #[test]
    fn warnings_are_classified_by_prefix() {
        assert_eq!(
            WarningClass::of("compaction failed; continuing uncompacted: boom"),
            WarningClass::CompactionFailed
        );
        assert_eq!(
            WarningClass::of("could not run `cargo test`"),
            WarningClass::CheckFailed
        );
        assert_eq!(
            WarningClass::of("the reviewer gave no JSON verdict; asking once more"),
            WarningClass::ReviewUnparsed
        );
        assert_eq!(WarningClass::of("something odd"), WarningClass::Other);
    }

    #[test]
    fn session_outcome_round_trips_and_classes() {
        let record = Record::now(
            "s",
            "p",
            RecordKind::SessionEnded {
                outcome: SessionOutcome::Incomplete {
                    reason: "review_rounds_exhausted".to_owned(),
                },
            },
        );
        let json = serde_json::to_string(&record).expect("json");
        assert!(json.contains(r#""event":"session_ended""#));
        assert!(json.contains(r#""outcome":"incomplete""#));
        let back: Record = serde_json::from_str(&json).expect("parses");
        assert_eq!(back, record);
        let RecordKind::SessionEnded { outcome } = back.kind else {
            panic!("ended");
        };
        assert_eq!(outcome.class(), "incomplete:review_rounds_exhausted");
        assert_eq!(SessionOutcome::Done.class(), "done");
    }
}
