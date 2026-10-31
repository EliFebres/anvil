//! Turning signals into ticket drafts through the summarizer.
//!
//! The model sees the signal's fields and the content-free records behind
//! it, nothing else. It fills the prose fields; the title, kind, priority
//! and evidence come from the signal, so the same finding in two sessions
//! lands in one ticket.

use std::future::Future;
use std::pin::Pin;

use chrono::{DateTime, Utc};
use forge_types::boundary;
use forge_types::{TelemetryRef, Ticket, TicketOrigin, TicketStatus};
use serde::Deserialize;
use ulid::Ulid;

use crate::error::TelemetryError;
use crate::record::{Record, RecordKind};
use crate::signal::{Signal, SignalKind};
use crate::tickets::TicketStore;

/// The system prompt for drafting.
pub const SYSTEM_PROMPT: &str = include_str!("../prompts/draft.md");

/// Most records shown to the drafter for one signal.
const MAX_EVIDENCE_RECORDS: usize = 40;

/// A boxed future from a [`Drafter`].
pub type DraftFuture<'a> = Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>>;

/// Something that answers a drafting prompt: the summarizer in the CLI, a
/// canned reply in tests.
pub trait Drafter: Send + Sync {
    /// Completes one prompt.
    ///
    /// # Arguments
    ///
    /// * `system` - The system prompt.
    /// * `user` - The user message.
    ///
    /// # Returns
    ///
    /// The model's reply, or the provider's error message.
    fn complete<'a>(&'a self, system: &'a str, user: &'a str) -> DraftFuture<'a>;
}

/// The fields the drafter fills.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DraftFields {
    /// What went wrong or was missing.
    pub symptom: String,
    /// The interface or behavior wanted.
    pub spec: String,
    /// Checks that prove the change works.
    #[serde(default)]
    pub acceptance: Vec<String>,
    /// A synthetic reproduction.
    #[serde(default)]
    pub repro: Option<String>,
}

/// What one `forge ticket draft` run did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DraftReport {
    /// New drafts: ticket id and title.
    pub created: Vec<(Ulid, String)>,
    /// Signals folded into an existing ticket: ticket id and title.
    pub merged: Vec<(Ulid, String)>,
    /// Signals that could not be drafted: signal id and why.
    pub failed: Vec<(Ulid, String)>,
}

/// The user message for one signal.
///
/// # Arguments
///
/// * `signal` - The signal.
/// * `records` - The records behind it, already limited to its session.
///
/// # Returns
///
/// The message: the signal as JSON, then the records as JSON lines.
pub fn user_prompt(signal: &Signal, records: &[&Record]) -> String {
    let signal_json =
        serde_json::to_string_pretty(&signal.kind).unwrap_or_else(|_| "{}".to_owned());
    let lines: Vec<String> = records
        .iter()
        .take(MAX_EVIDENCE_RECORDS)
        .filter_map(|record| serde_json::to_string(&record.kind).ok())
        .collect();
    format!(
        "Signal (ticket kind `{}`):\n{signal_json}\n\nEvent records from the session, oldest first ({} of {}):\n{}\n",
        serde_json::to_value(signal.kind.ticket_kind())
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default(),
        lines.len(),
        records.len(),
        lines.join("\n")
    )
}

/// The records that back a signal: the session's records of the kinds the
/// signal is about.
///
/// # Arguments
///
/// * `signal` - The signal.
/// * `records` - Records of any sessions.
///
/// # Returns
///
/// The matching records, in order.
pub fn relevant_records<'a>(signal: &Signal, records: &'a [Record]) -> Vec<&'a Record> {
    records
        .iter()
        .filter(|record| record.session == signal.session)
        .filter(|record| is_relevant(&signal.kind, &record.kind))
        .collect()
}

/// Whether a record is evidence for a signal kind.
fn is_relevant(signal: &SignalKind, record: &RecordKind) -> bool {
    match signal {
        SignalKind::ToolMissing { tool, skill: None } => match record {
            RecordKind::ToolCall { name, .. } | RecordKind::ToolResult { name, .. } => name == tool,
            _ => false,
        },
        SignalKind::ToolMissing { skill: Some(_), .. } => {
            matches!(record, RecordKind::SkillRequires { .. })
        }
        SignalKind::ToolFailedRepeatedly { tool, .. } => match record {
            RecordKind::ToolCall { name, .. } | RecordKind::ToolResult { name, .. } => name == tool,
            _ => false,
        },
        SignalKind::ReviewLoopExhausted { .. } => matches!(
            record,
            RecordKind::Review { .. }
                | RecordKind::StateEntered { .. }
                | RecordKind::SessionEnded { .. }
        ),
        SignalKind::VerifyLoopExhausted { .. } => {
            matches!(
                record,
                RecordKind::Check { .. } | RecordKind::SessionEnded { .. }
            )
        }
        SignalKind::CompactionThrash { .. } => {
            matches!(
                record,
                RecordKind::Compacted { .. } | RecordKind::Turn { .. }
            )
        }
        SignalKind::PermissionFriction { tool, .. } => {
            matches!(record, RecordKind::Permission { tool: t, .. } if t == tool)
        }
        SignalKind::ModelSlow { provider, .. } | SignalKind::ModelErrors { provider, .. } => {
            matches!(
                record,
                RecordKind::Turn { provider: p, .. } | RecordKind::ProviderError { provider: p, .. }
                    if p == provider
            )
        }
        SignalKind::UserFlag { .. } => matches!(record, RecordKind::UserFlag { .. }),
    }
}

/// Extracts the draft from the model's reply.
///
/// A fenced `json` block is preferred; otherwise the last `{...}` span is
/// tried.
///
/// # Arguments
///
/// * `text` - The reply.
///
/// # Returns
///
/// The fields.
///
/// # Errors
///
/// [`TelemetryError::BadDraft`] when no JSON object with the expected
/// fields can be found.
pub fn parse_draft(text: &str) -> Result<DraftFields, TelemetryError> {
    let fenced: Vec<&str> = text
        .split("```")
        .enumerate()
        .filter(|(index, _)| index % 2 == 1)
        .map(|(_, block)| {
            block
                .trim_start()
                .strip_prefix("json")
                .unwrap_or(block)
                .trim()
        })
        .collect();
    let mut last_error = None;
    for block in fenced.iter().rev() {
        match serde_json::from_str::<DraftFields>(block) {
            Ok(fields) => return Ok(fields),
            Err(error) => last_error = Some(error.to_string()),
        }
    }
    if let (Some(start), Some(end)) = (text.find('{'), text.rfind('}'))
        && start < end
    {
        match serde_json::from_str::<DraftFields>(&text[start..=end]) {
            Ok(fields) => return Ok(fields),
            Err(error) => last_error = Some(error.to_string()),
        }
    }
    Err(TelemetryError::BadDraft {
        detail: last_error.unwrap_or_else(|| "no JSON object in the reply".to_owned()),
    })
}

/// Builds the ticket and runs it through the validator.
///
/// Besides [`Ticket::validate`], the symptom and every acceptance check are
/// held to the same path rule as the spec, because the drafter wrote them
/// from the same material.
///
/// # Arguments
///
/// * `signal` - The signal; gives the title, kind, priority and evidence.
/// * `fields` - What the drafter wrote.
/// * `now` - The creation time.
///
/// # Returns
///
/// A draft that passes the validator.
///
/// # Errors
///
/// [`TelemetryError::Validation`] when the draft fails the boundary, or
/// [`TelemetryError::BadSessionId`] when the session id is not a ULID.
pub fn assemble(
    signal: &Signal,
    fields: DraftFields,
    now: DateTime<Utc>,
) -> Result<Ticket, TelemetryError> {
    let session = Ulid::from_string(&signal.session).map_err(|_| TelemetryError::BadSessionId {
        id: signal.session.clone(),
    })?;
    let acceptance: Vec<String> = fields
        .acceptance
        .into_iter()
        .map(|item| item.trim().to_owned())
        .filter(|item| !item.is_empty())
        .collect();
    let ticket = Ticket {
        id: Ulid::generate(),
        created_at: now,
        kind: signal.kind.ticket_kind(),
        title: signal.kind.title(),
        symptom: fields.symptom.trim().to_owned(),
        repro: fields
            .repro
            .map(|repro| repro.trim().to_owned())
            .filter(|repro| !repro.is_empty()),
        spec: fields.spec.trim().to_owned(),
        acceptance,
        evidence: vec![TelemetryRef(session)],
        priority: signal.kind.priority(),
        status: TicketStatus::Draft,
        origin: TicketOrigin::Cli,
    };
    ticket.validate()?;
    boundary::check_text("symptom", &ticket.symptom)?;
    for item in &ticket.acceptance {
        boundary::check_text("acceptance", item)?;
    }
    Ok(ticket)
}

/// Drafts every unhandled signal, folding duplicates into existing tickets.
///
/// A signal whose kind and normalized title match a draft or open ticket
/// adds its session to that ticket's evidence instead of becoming a new
/// draft. Every handled signal gets its `ticket` set; the caller saves the
/// signals.
///
/// # Arguments
///
/// * `signals` - All signals; handled ones are skipped.
/// * `records` - The project's records, for the drafter's evidence.
/// * `store` - The ticket folders.
/// * `drafter` - Who writes the prose.
/// * `now` - The creation time for new drafts.
///
/// # Returns
///
/// What was created, merged and failed.
///
/// # Errors
///
/// When a ticket file cannot be written. A drafter or validator failure
/// is reported in the result and does not stop the run.
pub async fn draft_all(
    signals: &mut [Signal],
    records: &[Record],
    store: &mut TicketStore,
    drafter: &dyn Drafter,
    now: DateTime<Utc>,
) -> Result<DraftReport, TelemetryError> {
    let mut report = DraftReport::default();
    for signal in signals.iter_mut().filter(|signal| signal.ticket.is_none()) {
        let (kind, normalized) = signal.key();
        if let Some(existing) = store.find_duplicate(kind, &normalized) {
            match Ulid::from_string(&signal.session) {
                Ok(session) => {
                    let evidence = TelemetryRef(session);
                    if !existing.ticket.evidence.contains(&evidence) {
                        existing.ticket.evidence.push(evidence);
                        TicketStore::save(existing)?;
                    }
                    signal.ticket = Some(existing.ticket.id);
                    report
                        .merged
                        .push((existing.ticket.id, existing.ticket.title.clone()));
                }
                Err(_) => report.failed.push((
                    signal.id,
                    TelemetryError::BadSessionId {
                        id: signal.session.clone(),
                    }
                    .to_string(),
                )),
            }
            continue;
        }
        let evidence = relevant_records(signal, records);
        let user = user_prompt(signal, &evidence);
        let reply = match drafter.complete(SYSTEM_PROMPT, &user).await {
            Ok(reply) => reply,
            Err(detail) => {
                report
                    .failed
                    .push((signal.id, TelemetryError::Drafter { detail }.to_string()));
                continue;
            }
        };
        match parse_draft(&reply).and_then(|fields| assemble(signal, fields, now)) {
            Ok(ticket) => {
                let id = ticket.id;
                let title = ticket.title.clone();
                store.add_draft(ticket)?;
                signal.ticket = Some(id);
                report.created.push((id, title));
            }
            Err(error) => report.failed.push((signal.id, error.to_string())),
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::task::{Context, Poll, Waker};

    use super::*;
    use crate::record::ToolOutcomeKind;

    /// Runs a future that never actually waits, as every test drafter is
    /// ready at once.
    fn block_on<F: Future>(future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        let mut context = Context::from_waker(Waker::noop());
        loop {
            if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
                return value;
            }
        }
    }

    /// Replies with one canned text and remembers every prompt.
    struct FakeDrafter {
        reply: Result<String, String>,
        prompts: Mutex<Vec<String>>,
    }

    impl FakeDrafter {
        fn new(reply: &str) -> Self {
            Self {
                reply: Ok(reply.to_owned()),
                prompts: Mutex::new(Vec::new()),
            }
        }
    }

    impl Drafter for FakeDrafter {
        fn complete<'a>(&'a self, _system: &'a str, user: &'a str) -> DraftFuture<'a> {
            self.prompts.lock().expect("lock").push(user.to_owned());
            let reply = self.reply.clone();
            Box::pin(async move { reply })
        }
    }

    const GOOD_REPLY: &str = "Here you go.\n```json\n{\"symptom\": \"The model asked for a browser tool that forge does not have.\", \"spec\": \"Add a `browser` tool that fetches a URL and returns its text.\", \"acceptance\": [\"the tool is listed by `forge doctor`\", \"a fetch of a small page returns its text\"], \"repro\": \"a task that needs documentation from a web page\"}\n```\n";

    fn missing(session: &str, tool: &str) -> Signal {
        Signal::new(
            session,
            SignalKind::ToolMissing {
                tool: tool.to_owned(),
                skill: None,
            },
        )
    }

    fn fields(spec: &str) -> DraftFields {
        DraftFields {
            symptom: "the model asked for a tool forge lacks".to_owned(),
            spec: spec.to_owned(),
            acceptance: vec!["the tool exists".to_owned()],
            repro: None,
        }
    }

    #[test]
    fn parse_draft_prefers_the_last_fenced_block_and_falls_back_to_braces() {
        let parsed = parse_draft(GOOD_REPLY).expect("parses");
        assert_eq!(parsed.acceptance.len(), 2);
        assert!(parsed.repro.is_some());
        let bare = r#"sure: {"symptom": "s", "spec": "x"}"#;
        let parsed = parse_draft(bare).expect("parses");
        assert_eq!(parsed.spec, "x");
        assert!(parsed.acceptance.is_empty());
        assert!(matches!(
            parse_draft("no json here"),
            Err(TelemetryError::BadDraft { .. })
        ));
        assert!(matches!(
            parse_draft("```json\n{\"symptom\": \"only\"}\n```"),
            Err(TelemetryError::BadDraft { .. })
        ));
    }

    #[test]
    fn assemble_fills_the_ticket_from_the_signal() {
        let signal = missing(&Ulid::generate().to_string(), "browser");
        let ticket = assemble(&signal, fields("add it"), Utc::now()).expect("valid");
        assert_eq!(ticket.title, "Missing tool `browser`");
        assert_eq!(ticket.kind, forge_types::TicketKind::Tool);
        assert_eq!(ticket.status, TicketStatus::Draft);
        assert_eq!(ticket.origin, TicketOrigin::Cli);
        assert_eq!(ticket.evidence.len(), 1);
        assert_eq!(ticket.evidence[0].0.to_string(), signal.session);
        assert!(matches!(
            assemble(&missing("not-a-ulid", "x"), fields("add it"), Utc::now()),
            Err(TelemetryError::BadSessionId { .. })
        ));
    }

    #[test]
    fn validator_rejects_a_draft_that_smuggles_a_path() {
        let signal = missing(&Ulid::generate().to_string(), "browser");
        for bad in [
            "see C:\\Users\\me\\proj\\main.rs",
            "see /home/me/proj/src/lib.rs",
            "see ../secret/notes.md",
        ] {
            assert!(
                matches!(
                    assemble(&signal, fields(bad), Utc::now()),
                    Err(TelemetryError::Validation(
                        forge_types::ValidationError::OutsidePath { .. }
                    ))
                ),
                "{bad}"
            );
        }
        let mut sneaky = fields("fine");
        sneaky.symptom = "fails on /home/me/proj".to_owned();
        assert!(matches!(
            assemble(&signal, sneaky, Utc::now()),
            Err(TelemetryError::Validation(_))
        ));
        let mut sneaky = fields("fine");
        sneaky.acceptance = vec!["check ~/proj/out.txt".to_owned()];
        assert!(matches!(
            assemble(&signal, sneaky, Utc::now()),
            Err(TelemetryError::Validation(_))
        ));
        let mut empty = fields("fine");
        empty.acceptance = vec![" ".to_owned()];
        assert!(matches!(
            assemble(&signal, empty, Utc::now()),
            Err(TelemetryError::Validation(
                forge_types::ValidationError::EmptyAcceptance
            ))
        ));
    }

    #[test]
    fn two_sessions_with_the_same_signal_make_one_ticket_with_two_refs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = TicketStore::load(dir.path()).expect("store");
        let first = Ulid::generate().to_string();
        let second = Ulid::generate().to_string();
        let mut signals = vec![missing(&first, "browser"), missing(&second, "browser")];
        let records = vec![
            Record::now(
                &first,
                "p",
                RecordKind::tool_result("browser", ToolOutcomeKind::UnknownTool, "", 1, false, 0),
            ),
            Record::now(
                &second,
                "p",
                RecordKind::tool_result("glob", ToolOutcomeKind::Completed, "", 1, false, 0),
            ),
        ];
        let drafter = FakeDrafter::new(GOOD_REPLY);
        let report = block_on(draft_all(
            &mut signals,
            &records,
            &mut store,
            &drafter,
            Utc::now(),
        ))
        .expect("draft");
        assert_eq!(report.created.len(), 1);
        assert_eq!(report.merged.len(), 1);
        assert!(report.failed.is_empty());
        assert_eq!(
            drafter.prompts.lock().expect("lock").len(),
            1,
            "one model call"
        );
        assert!(drafter.prompts.lock().expect("lock")[0].contains("unknown_tool"));
        let reloaded = TicketStore::load(dir.path()).expect("store");
        assert_eq!(reloaded.drafts.len(), 1);
        let ticket = &reloaded.drafts[0].ticket;
        assert_eq!(ticket.evidence.len(), 2);
        assert_eq!(ticket.evidence[0].0.to_string(), first);
        assert_eq!(ticket.evidence[1].0.to_string(), second);
        assert_eq!(signals[0].ticket, Some(ticket.id));
        assert_eq!(signals[1].ticket, Some(ticket.id));

        let mut again = signals.clone();
        let report = block_on(draft_all(
            &mut again,
            &records,
            &mut store,
            &drafter,
            Utc::now(),
        ))
        .expect("draft");
        assert_eq!(
            report,
            DraftReport::default(),
            "handled signals are skipped"
        );
        let third = missing(&first, "browser");
        let mut dup = vec![third];
        let report = block_on(draft_all(
            &mut dup,
            &records,
            &mut store,
            &drafter,
            Utc::now(),
        ))
        .expect("draft");
        assert_eq!(report.merged.len(), 1);
        assert_eq!(
            TicketStore::load(dir.path()).expect("store").drafts[0]
                .ticket
                .evidence
                .len(),
            2,
            "a session is never listed twice"
        );
    }

    #[test]
    fn drafter_and_validator_failures_are_reported_not_fatal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut store = TicketStore::load(dir.path()).expect("store");
        let mut signals = vec![missing(&Ulid::generate().to_string(), "a")];
        let failing = FakeDrafter {
            reply: Err("provider down".to_owned()),
            prompts: Mutex::new(Vec::new()),
        };
        let report = block_on(draft_all(
            &mut signals,
            &[],
            &mut store,
            &failing,
            Utc::now(),
        ))
        .expect("draft");
        assert_eq!(report.failed.len(), 1);
        assert!(report.failed[0].1.contains("provider down"));
        assert_eq!(signals[0].ticket, None);
        let smuggler = FakeDrafter::new(
            "```json\n{\"symptom\": \"x\", \"spec\": \"edit /home/me/proj/a.rs\", \"acceptance\": [\"y\"]}\n```",
        );
        let report = block_on(draft_all(
            &mut signals,
            &[],
            &mut store,
            &smuggler,
            Utc::now(),
        ))
        .expect("draft");
        assert_eq!(report.failed.len(), 1);
        assert!(report.failed[0].1.contains("outside the monorepo"));
        assert!(store.drafts.is_empty());
        assert!(!dir.path().join("tickets").exists());
    }

    #[test]
    fn user_prompt_is_stable() {
        let signal = Signal {
            id: Ulid::nil(),
            at: DateTime::<Utc>::UNIX_EPOCH,
            session: "01ARZ3NDEKTSV4RRFFQ69G5FAV".to_owned(),
            kind: SignalKind::ToolFailedRepeatedly {
                tool: "edit_file".to_owned(),
                error_class: "failed:0a1b2c3d".to_owned(),
                count: 3,
            },
            ticket: None,
        };
        let records: Vec<Record> = (0..3)
            .map(|_| Record {
                ts: DateTime::<Utc>::UNIX_EPOCH,
                session: signal.session.clone(),
                project: "p".to_owned(),
                kind: RecordKind::tool_result(
                    "edit_file",
                    ToolOutcomeKind::Failed,
                    "no match for old text",
                    42,
                    false,
                    7,
                ),
            })
            .collect();
        let relevant = relevant_records(&signal, &records);
        insta::assert_snapshot!(user_prompt(&signal, &relevant));
        insta::assert_snapshot!(SYSTEM_PROMPT);
    }
}
