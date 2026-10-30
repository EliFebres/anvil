//! The ticket: the unit of work handed from forge to the factory.

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ulid::Ulid;

use crate::boundary;
use crate::error::ValidationError;

/// Maximum length of a ticket title, in characters.
pub const MAX_TITLE_CHARS: usize = 80;

/// What kind of improvement a ticket asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TicketKind {
    /// A new or changed tool.
    Tool,
    /// A new or changed skill.
    Skill,
    /// A prompt change.
    Prompt,
    /// A change to the agent loop itself.
    Loop,
    /// A change to cross-project memory.
    Memory,
    /// A change to a model or provider.
    Model,
    /// Something that is simply broken.
    Bug,
}

/// How urgently a ticket should be worked.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    /// Blocks normal use; work it first.
    P0,
    /// Important, not blocking.
    P1,
    /// Nice to have.
    P2,
}

/// Where a ticket is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TicketStatus {
    /// Waiting in `tickets/open/`.
    Open,
    /// A factory worker has taken it.
    Claimed,
    /// A pull request exists and is under review.
    InReview,
    /// Merged to trunk.
    Merged,
    /// Closed without merging.
    Rejected,
}

/// Who created the ticket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TicketOrigin {
    /// Drafted by forge's own telemetry.
    Cli,
    /// Drafted by the factory, for example from a failed review.
    Factory,
    /// Written by a person.
    Human,
}

/// Reference to one event in forge's own telemetry store.
///
/// Carries the id only, never the event content, so a ticket can point at
/// evidence without leaking what the user was working on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct TelemetryRef(#[schemars(with = "String")] pub Ulid);

/// A request for the factory to improve forge.
///
/// Tickets are stored as `tickets/open/<id>.toml`. Call [`Ticket::validate`]
/// after deserializing; serde checks the shape, the validator checks the
/// data boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Ticket {
    /// Unique, time-ordered id. Also the file stem.
    #[schemars(with = "String")]
    pub id: Ulid,
    /// When the ticket was created, RFC 3339.
    pub created_at: DateTime<Utc>,
    /// What kind of improvement is wanted.
    pub kind: TicketKind,
    /// One line, at most [`MAX_TITLE_CHARS`] characters.
    pub title: String,
    /// What went wrong or what was missing, in plain words.
    pub symptom: String,
    /// A synthetic reproduction. Never project code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repro: Option<String>,
    /// The interface wanted: CLI flags, tool JSON schema, skill name and trigger.
    pub spec: String,
    /// Checks the factory must make pass.
    pub acceptance: Vec<String>,
    /// Ids into forge's telemetry, not content.
    #[serde(default)]
    pub evidence: Vec<TelemetryRef>,
    /// How urgent the ticket is.
    pub priority: Priority,
    /// Where the ticket is in its lifecycle.
    pub status: TicketStatus,
    /// Who created it.
    pub origin: TicketOrigin,
}

impl Ticket {
    /// Checks the ticket's invariants and the forge/factory data boundary.
    ///
    /// # Returns
    ///
    /// `Ok(())` when the title is 1 to [`MAX_TITLE_CHARS`] characters, the
    /// symptom and spec are non-empty, there is at least one acceptance check,
    /// and neither `spec` nor `repro` names a path outside the monorepo or
    /// holds more than [`boundary::MAX_CODE_LINES`] lines of code.
    ///
    /// # Errors
    ///
    /// The first [`ValidationError`] found, in field order.
    pub fn validate(&self) -> Result<(), ValidationError> {
        let title_len = self.title.chars().count();
        if title_len == 0 {
            return Err(ValidationError::EmptyField { field: "title" });
        }
        if title_len > MAX_TITLE_CHARS {
            return Err(ValidationError::TitleTooLong {
                len: title_len,
                max: MAX_TITLE_CHARS,
            });
        }
        if self.symptom.trim().is_empty() {
            return Err(ValidationError::EmptyField { field: "symptom" });
        }
        if self.spec.trim().is_empty() {
            return Err(ValidationError::EmptyField { field: "spec" });
        }
        if self.acceptance.is_empty() {
            return Err(ValidationError::EmptyAcceptance);
        }
        boundary::check_text("spec", &self.spec)?;
        if let Some(repro) = &self.repro {
            boundary::check_text("repro", repro)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Ticket {
        Ticket {
            id: Ulid::from_string("01J9Z6Q4K8XW2R3T5V7Y9B1D3F").expect("valid ulid literal"),
            created_at: "2026-10-30T10:00:00Z".parse().expect("valid rfc3339"),
            kind: TicketKind::Tool,
            title: "Add a `grep` tool with a max-results flag".to_owned(),
            symptom: "The agent reads whole files to find one symbol.".to_owned(),
            repro: Some("```\nforge run \"find callers of foo\"\n```".to_owned()),
            spec: "Tool `grep` with `pattern`, `path`, `max_results` (default 50).".to_owned(),
            acceptance: vec!["cargo test -p forge-core passes".to_owned()],
            evidence: vec![TelemetryRef(
                Ulid::from_string("01J9Z6Q4K8XW2R3T5V7Y9B1D3G").expect("valid ulid literal"),
            )],
            priority: Priority::P1,
            status: TicketStatus::Open,
            origin: TicketOrigin::Cli,
        }
    }

    #[test]
    fn sample_is_valid() {
        sample().validate().expect("sample ticket validates");
    }

    #[test]
    fn toml_round_trip() {
        let ticket = sample();
        let text = toml::to_string_pretty(&ticket).expect("serializes to toml");
        let back: Ticket = toml::from_str(&text).expect("parses back");
        assert_eq!(back, ticket);
    }

    #[test]
    fn json_round_trip() {
        let ticket = sample();
        let text = serde_json::to_string(&ticket).expect("serializes to json");
        let back: Ticket = serde_json::from_str(&text).expect("parses back");
        assert_eq!(back, ticket);
    }

    #[test]
    fn id_and_timestamp_serialize_as_strings() {
        let text = toml::to_string(&sample()).expect("serializes");
        assert!(
            text.contains("id = \"01J9Z6Q4K8XW2R3T5V7Y9B1D3F\""),
            "{text}"
        );
        assert!(
            text.contains("created_at = \"2026-10-30T10:00:00Z\""),
            "{text}"
        );
        assert!(
            text.contains("evidence = [\"01J9Z6Q4K8XW2R3T5V7Y9B1D3G\"]"),
            "{text}"
        );
    }

    #[test]
    fn enums_use_snake_case_spellings() {
        let json = |v: &dyn erased::Ser| v.json();
        assert_eq!(json(&TicketStatus::InReview), "\"in_review\"");
        assert_eq!(json(&Priority::P0), "\"p0\"");
        assert_eq!(json(&TicketKind::Loop), "\"loop\"");
        assert_eq!(json(&TicketOrigin::Factory), "\"factory\"");
    }

    /// Tiny object-safe shim so one closure can serialize several enum types.
    mod erased {
        pub trait Ser {
            fn json(&self) -> String;
        }
        impl<T: serde::Serialize> Ser for T {
            fn json(&self) -> String {
                serde_json::to_string(self).expect("enum serializes")
            }
        }
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let mut text = toml::to_string(&sample()).expect("serializes");
        text.push_str("\nassignee = \"someone\"\n");
        assert!(toml::from_str::<Ticket>(&text).is_err());
    }

    #[test]
    fn title_limit_is_eighty_characters() {
        let mut ticket = sample();
        ticket.title = "x".repeat(MAX_TITLE_CHARS);
        ticket.validate().expect("80 characters is allowed");
        ticket.title.push('x');
        assert_eq!(
            ticket.validate(),
            Err(ValidationError::TitleTooLong { len: 81, max: 80 })
        );
    }

    #[test]
    fn empty_required_fields_are_rejected() {
        let mut ticket = sample();
        ticket.symptom = "   ".to_owned();
        assert_eq!(
            ticket.validate(),
            Err(ValidationError::EmptyField { field: "symptom" })
        );
        let mut ticket = sample();
        ticket.acceptance.clear();
        assert_eq!(ticket.validate(), Err(ValidationError::EmptyAcceptance));
    }

    #[test]
    fn boundary_applies_to_spec_and_repro() {
        let mut ticket = sample();
        ticket.spec = "read C:\\Users\\someone\\project\\main.rs".to_owned();
        assert!(matches!(
            ticket.validate(),
            Err(ValidationError::OutsidePath { field: "spec", .. })
        ));
        let mut ticket = sample();
        ticket.repro = Some("see /home/someone/project".to_owned());
        assert!(matches!(
            ticket.validate(),
            Err(ValidationError::OutsidePath { field: "repro", .. })
        ));
    }
}
