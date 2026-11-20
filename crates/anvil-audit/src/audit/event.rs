//! What one audit record says happened.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::Digest as _;

pub use anvil_types::policy::RecordMode;

/// A prompt or a tool call's arguments, as the record mode keeps it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Content {
    /// Byte length of the text.
    pub bytes: u64,
    /// SHA-256 of the text, lowercase hex.
    pub sha256: String,
    /// The text, under [`RecordMode::Full`] only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

impl Content {
    /// Keeps `text` as `mode` says.
    ///
    /// # Arguments
    ///
    /// * `text` - The text, already redacted.
    /// * `mode` - Whether the text itself is kept.
    ///
    /// # Returns
    ///
    /// The content.
    pub fn of(text: &str, mode: RecordMode) -> Self {
        Self {
            bytes: u64::try_from(text.len()).unwrap_or(u64::MAX),
            sha256: sha256_hex(text.as_bytes()),
            text: (mode == RecordMode::Full).then(|| text.to_owned()),
        }
    }
}

/// One configured provider, by where its requests go.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderEntry {
    /// The provider's name in the configuration.
    pub name: String,
    /// The `host:port` its requests go to.
    pub host: String,
    /// The model it asks for.
    pub model: String,
}

/// What happened, with the fields that event needs. Serialized with a
/// `type` field naming the variant in snake case.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuditEvent {
    /// A session started or resumed.
    SessionStart {
        /// `run`, `chat`, `resume` or `build`.
        mode: String,
        /// Every configured provider.
        providers: Vec<ProviderEntry>,
        /// Role name to provider name.
        roles: BTreeMap<String, String>,
        /// The switches in effect, by name.
        flags: BTreeMap<String, String>,
    },
    /// The user gave the session a task or a chat message.
    PromptSubmitted {
        /// The prompt, as the record mode keeps it.
        content: Content,
    },
    /// One model completion finished.
    ModelRequest {
        /// The role it served.
        role: String,
        /// The provider's name.
        provider: String,
        /// The `host:port` the request went to.
        host: String,
        /// The model.
        model: String,
        /// Uncached prompt tokens.
        input_tokens: u64,
        /// Output tokens.
        output_tokens: u64,
        /// Cached prompt tokens.
        cached_tokens: u64,
        /// Why the model stopped.
        stop_reason: String,
        /// Secrets redacted from what entered the conversation since the
        /// previous model request.
        redactions: u64,
    },
    /// The model called a tool.
    ToolCall {
        /// The call id.
        call_id: String,
        /// The tool.
        tool: String,
        /// The arguments, as the record mode keeps them.
        arguments: Content,
    },
    /// The permission model decided a call.
    PermissionDecision {
        /// The tool.
        tool: String,
        /// `allow` or `deny`.
        decision: String,
        /// `once`, `session`, `rule`, `deny` or `blocked`.
        scope: String,
        /// Where the decision came from, such as `policy_rule`, `user_rule`,
        /// `project_rule`, `prompt` or `yes_flag`.
        source: String,
    },
    /// A tool call finished.
    ToolResult {
        /// The call id.
        call_id: String,
        /// The tool.
        tool: String,
        /// How it ended, such as `completed`, `failed` or `denied_by_rule`.
        outcome: String,
        /// Whether the model was told it failed.
        error: bool,
        /// Byte length of the full result, after redaction.
        bytes: u64,
        /// SHA-256 of the full result, after redaction.
        sha256: String,
        /// Whether the model saw a truncated copy.
        truncated: bool,
        /// The exit code of the program, for `shell`.
        exit_code: Option<i32>,
    },
    /// A tool wrote a file.
    FileChange {
        /// The call id.
        call_id: String,
        /// The file, relative to the workspace when inside it.
        path: String,
        /// SHA-256 before, or `null` when the file was new.
        before: Option<String>,
        /// SHA-256 after.
        after: String,
    },
    /// A plan was approved, sent back or rejected.
    PlanApproval {
        /// `approved`, `revise` or `rejected`.
        decision: String,
        /// `user`, `yes_flag` or `build` (the build's item list was approved).
        answered_by: String,
    },
    /// The managed policy refused something.
    PolicyRefusal {
        /// What was refused.
        what: String,
        /// The policy key that refused it.
        key: String,
    },
    /// A session ended.
    SessionEnd {
        /// How, such as `done`, `incomplete`, `chat` or `audit_failed`.
        outcome: String,
    },
}

impl AuditEvent {
    /// The value of the event's `type` field.
    ///
    /// # Returns
    ///
    /// Such as `tool_call`.
    pub fn tag(&self) -> &'static str {
        match self {
            Self::SessionStart { .. } => "session_start",
            Self::PromptSubmitted { .. } => "prompt_submitted",
            Self::ModelRequest { .. } => "model_request",
            Self::ToolCall { .. } => "tool_call",
            Self::PermissionDecision { .. } => "permission_decision",
            Self::ToolResult { .. } => "tool_result",
            Self::FileChange { .. } => "file_change",
            Self::PlanApproval { .. } => "plan_approval",
            Self::PolicyRefusal { .. } => "policy_refusal",
            Self::SessionEnd { .. } => "session_end",
        }
    }
}

/// The SHA-256 of `bytes` as lowercase hex.
///
/// # Arguments
///
/// * `bytes` - What to digest.
///
/// # Returns
///
/// 64 hex digits.
pub fn sha256_hex(bytes: &[u8]) -> String {
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_mode_keeps_size_and_hash_but_not_the_text() {
        let digest = Content::of("hello", RecordMode::Digest);
        assert_eq!(digest.bytes, 5);
        assert_eq!(
            digest.sha256,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        assert_eq!(digest.text, None);
        assert!(
            !serde_json::to_string(&digest)
                .expect("json")
                .contains("hello")
        );
        assert_eq!(
            Content::of("hello", RecordMode::Full).text.as_deref(),
            Some("hello")
        );
    }

    #[test]
    fn events_serialize_with_their_type_tag() {
        let event = AuditEvent::PolicyRefusal {
            what: "shell".to_owned(),
            key: "permissions.deny".to_owned(),
        };
        let json = serde_json::to_string(&event).expect("json");
        assert!(json.starts_with(r#"{"type":"policy_refusal""#), "{json}");
        assert_eq!(event.tag(), "policy_refusal");
    }
}
