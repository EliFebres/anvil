//! Context management: instruction files, the conversation with automatic
//! compaction, and session persistence.

pub mod conversation;
pub mod instructions;
pub mod session;

pub use conversation::{
    Compaction, Conversation, KEEP_LAST_TURNS, collect_text, estimate_tokens, render_transcript,
};
pub use instructions::discover_instructions;
pub use session::{
    SessionError, SessionLog, SessionMeta, SessionRecord, SessionSnapshot, SessionSummary,
    list_sessions, load_session, new_session_id, session_dir,
};
