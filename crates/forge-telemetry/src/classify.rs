//! Turning one session's records into signals.
//!
//! Each signal has its own small function over synthetic-friendly inputs.
//! Session-local signals read only the session; the cross-session ones
//! (permission friction, slow or failing models) read the history the CLI
//! loads from the home telemetry folder, which includes the session itself.

use std::collections::{BTreeMap, BTreeSet};

use forge_types::TelemetryConfig;

use crate::record::{PermissionAnswer, Record, RecordKind, SessionOutcome, ToolOutcomeKind};
use crate::recorder::last_sessions;
use crate::signal::{Signal, SignalKind};

/// Failures of one tool with one error class that make a signal.
const REPEAT_FAILURES: usize = 3;

/// Compactions this close together, in turns, are thrash.
const THRASH_TURNS: usize = 10;

/// Yes answers to one prompt, without an always, that make a signal.
const FRICTION_ANSWERS: usize = 5;

/// Sessions the provider statistics look back over.
const MODEL_SESSIONS: usize = 20;

/// Completions and errors a provider needs before its rate or latency counts.
const MODEL_MIN_SAMPLES: usize = 5;

/// The reason tag the loop records when review rounds run out.
const REVIEW_EXHAUSTED: &str = "review_rounds_exhausted";

/// The reason tag the loop records when verify rounds run out.
const VERIFY_EXHAUSTED: &str = "verify_rounds_exhausted";

/// Classifies a finished session.
///
/// # Arguments
///
/// * `session` - The session's records, in order.
/// * `history` - Records of every session available for cross-session
///   signals, this one included.
/// * `config` - The lines for slow and failing providers.
///
/// # Returns
///
/// Zero or more signals, all attributed to the session.
pub fn classify(session: &[Record], history: &[Record], config: &TelemetryConfig) -> Vec<Signal> {
    let Some(id) = session.first().map(|record| record.session.as_str()) else {
        return Vec::new();
    };
    let mut kinds = tool_missing(session);
    kinds.extend(tool_failed_repeatedly(session));
    kinds.extend(review_loop_exhausted(session));
    kinds.extend(verify_loop_exhausted(session));
    kinds.extend(compaction_thrash(session));
    kinds.extend(user_flags(session));
    kinds.extend(permission_friction(history));
    kinds.extend(model_slow(history, config.slow_median_ms));
    kinds.extend(model_errors(history, config.error_rate_percent));
    kinds
        .into_iter()
        .map(|kind| Signal::new(id, kind))
        .collect()
}

/// Tools the model asked for that do not exist, and tools skills require
/// that are not registered. One signal per distinct name.
///
/// # Arguments
///
/// * `session` - The session's records.
///
/// # Returns
///
/// The signals.
pub fn tool_missing(session: &[Record]) -> Vec<SignalKind> {
    let mut seen = BTreeSet::new();
    let mut found = Vec::new();
    for record in session {
        let kind = match &record.kind {
            RecordKind::ToolResult {
                name,
                outcome: ToolOutcomeKind::UnknownTool,
                ..
            } => SignalKind::ToolMissing {
                tool: name.clone(),
                skill: None,
            },
            RecordKind::SkillRequires { skill, tool } => SignalKind::ToolMissing {
                tool: tool.clone(),
                skill: Some(skill.clone()),
            },
            _ => continue,
        };
        if seen.insert(kind.title()) {
            found.push(kind);
        }
    }
    found
}

/// Tools that failed three or more times with the same error class.
///
/// # Arguments
///
/// * `session` - The session's records.
///
/// # Returns
///
/// The signals, one per tool and class.
pub fn tool_failed_repeatedly(session: &[Record]) -> Vec<SignalKind> {
    let mut counts: BTreeMap<(&str, &str), usize> = BTreeMap::new();
    for record in session {
        if let RecordKind::ToolResult {
            name,
            outcome: ToolOutcomeKind::Failed,
            error_class: Some(class),
            ..
        } = &record.kind
        {
            *counts.entry((name, class)).or_default() += 1;
        }
    }
    counts
        .into_iter()
        .filter(|(_, count)| *count >= REPEAT_FAILURES)
        .map(|((tool, class), count)| SignalKind::ToolFailedRepeatedly {
            tool: tool.to_owned(),
            error_class: class.to_owned(),
            count,
        })
        .collect()
}

/// A session that ended because review rounds ran out, carrying how many
/// change requests the reviewer's last verdict made.
///
/// # Arguments
///
/// * `session` - The session's records.
///
/// # Returns
///
/// At most one signal.
pub fn review_loop_exhausted(session: &[Record]) -> Vec<SignalKind> {
    if !ended_with(session, REVIEW_EXHAUSTED) {
        return Vec::new();
    }
    let items = session
        .iter()
        .rev()
        .find_map(|record| match &record.kind {
            RecordKind::Review {
                approved: false,
                items,
                ..
            } => Some(*items),
            _ => None,
        })
        .unwrap_or_default();
    vec![SignalKind::ReviewLoopExhausted { items }]
}

/// A session where the same check failed twice, or that ended because
/// verify rounds ran out.
///
/// # Arguments
///
/// * `session` - The session's records.
///
/// # Returns
///
/// At most one signal.
pub fn verify_loop_exhausted(session: &[Record]) -> Vec<SignalKind> {
    let mut failures: BTreeMap<&str, (usize, &str)> = BTreeMap::new();
    for record in session {
        if let RecordKind::Check {
            program,
            command_digest,
            passed: false,
        } = &record.kind
        {
            let entry = failures.entry(command_digest).or_insert((0, program));
            entry.0 += 1;
        }
    }
    let repeated = failures
        .values()
        .find(|(count, _)| *count >= 2)
        .map(|(_, program)| (*program).to_owned());
    let check = match repeated {
        Some(program) => program,
        None if ended_with(session, VERIFY_EXHAUSTED) => failures
            .values()
            .last()
            .map(|(_, program)| (*program).to_owned())
            .unwrap_or_else(|| "verify".to_owned()),
        None => return Vec::new(),
    };
    vec![SignalKind::VerifyLoopExhausted { check }]
}

/// Two compactions fewer than ten turns apart.
///
/// # Arguments
///
/// * `session` - The session's records.
///
/// # Returns
///
/// At most one signal, for the first such pair.
pub fn compaction_thrash(session: &[Record]) -> Vec<SignalKind> {
    let mut turns_since: Option<usize> = None;
    for record in session {
        match &record.kind {
            RecordKind::Turn { .. } => {
                if let Some(turns) = turns_since.as_mut() {
                    *turns += 1;
                }
            }
            RecordKind::Compacted { .. } => {
                if let Some(turns) = turns_since
                    && turns < THRASH_TURNS
                {
                    return vec![SignalKind::CompactionThrash { turns_apart: turns }];
                }
                turns_since = Some(0);
            }
            _ => {}
        }
    }
    Vec::new()
}

/// Every `/ticket` the user typed.
///
/// # Arguments
///
/// * `session` - The session's records.
///
/// # Returns
///
/// One signal per flag.
pub fn user_flags(session: &[Record]) -> Vec<SignalKind> {
    session
        .iter()
        .filter_map(|record| match &record.kind {
            RecordKind::UserFlag { text } => Some(SignalKind::UserFlag { text: text.clone() }),
            _ => None,
        })
        .collect()
}

/// Prompts answered yes five or more times across sessions and never always.
///
/// # Arguments
///
/// * `history` - Records of every session.
///
/// # Returns
///
/// One signal per prompt.
pub fn permission_friction(history: &[Record]) -> Vec<SignalKind> {
    let mut prompts: BTreeMap<&str, (&str, usize, bool)> = BTreeMap::new();
    for record in history {
        if let RecordKind::Permission {
            tool,
            rule_digest,
            answer,
        } = &record.kind
        {
            let entry = prompts.entry(rule_digest).or_insert((tool, 0, false));
            match answer {
                PermissionAnswer::Yes => entry.1 += 1,
                PermissionAnswer::Always => entry.2 = true,
                PermissionAnswer::No => {}
            }
        }
    }
    prompts
        .into_values()
        .filter(|(_, yes, always)| *yes >= FRICTION_ANSWERS && !always)
        .map(|(tool, count, _)| SignalKind::PermissionFriction {
            tool: tool.to_owned(),
            count,
        })
        .collect()
}

/// Providers whose median completion latency over the last twenty sessions
/// is above the line.
///
/// # Arguments
///
/// * `history` - Records of every session.
/// * `limit_ms` - The line.
///
/// # Returns
///
/// One signal per provider.
pub fn model_slow(history: &[Record], limit_ms: u64) -> Vec<SignalKind> {
    let recent = last_sessions(history, MODEL_SESSIONS);
    let mut latencies: BTreeMap<&str, Vec<u64>> = BTreeMap::new();
    for record in &recent {
        if let RecordKind::Turn {
            provider,
            latency_ms,
            ..
        } = &record.kind
        {
            latencies.entry(provider).or_default().push(*latency_ms);
        }
    }
    latencies
        .into_iter()
        .filter(|(_, samples)| samples.len() >= MODEL_MIN_SAMPLES)
        .filter_map(|(provider, mut samples)| {
            samples.sort_unstable();
            let median = samples[samples.len() / 2];
            (median > limit_ms).then(|| SignalKind::ModelSlow {
                provider: provider.to_owned(),
                median_ms: median,
                limit_ms,
            })
        })
        .collect()
}

/// Providers whose error rate over the last twenty sessions is above the
/// line.
///
/// # Arguments
///
/// * `history` - Records of every session.
/// * `limit_percent` - The line.
///
/// # Returns
///
/// One signal per provider.
pub fn model_errors(history: &[Record], limit_percent: u8) -> Vec<SignalKind> {
    let recent = last_sessions(history, MODEL_SESSIONS);
    let mut counts: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for record in &recent {
        match &record.kind {
            RecordKind::Turn { provider, .. } => counts.entry(provider).or_default().0 += 1,
            RecordKind::ProviderError { provider, .. } => {
                counts.entry(provider).or_default().1 += 1;
            }
            _ => {}
        }
    }
    counts
        .into_iter()
        .filter(|(_, (turns, errors))| turns + errors >= MODEL_MIN_SAMPLES)
        .filter_map(|(provider, (turns, errors))| {
            let rate = errors * 100 / (turns + errors);
            let rate_percent = u8::try_from(rate).unwrap_or(u8::MAX);
            (rate_percent > limit_percent).then(|| SignalKind::ModelErrors {
                provider: provider.to_owned(),
                rate_percent,
                limit_percent,
            })
        })
        .collect()
}

/// Whether the session ended incomplete or denied with this reason tag.
fn ended_with(session: &[Record], reason_tag: &str) -> bool {
    session.iter().any(|record| {
        matches!(
            &record.kind,
            RecordKind::SessionEnded {
                outcome: SessionOutcome::Incomplete { reason } | SessionOutcome::Denied { reason },
            } if reason == reason_tag
        )
    })
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, Utc};

    use super::*;
    use crate::record::{StopKind, Tokens};

    fn record(session: &str, kind: RecordKind) -> Record {
        Record {
            ts: Utc::now(),
            session: session.to_owned(),
            project: "p".to_owned(),
            kind,
        }
    }

    fn turn(session: &str, provider: &str, latency_ms: u64) -> Record {
        record(
            session,
            RecordKind::Turn {
                state: "build".to_owned(),
                provider: provider.to_owned(),
                model: "m".to_owned(),
                tokens: Tokens::default(),
                latency_ms,
                retries: 0,
                stop_reason: StopKind::EndTurn,
            },
        )
    }

    fn failed(session: &str, tool: &str, preview: &str) -> Record {
        record(
            session,
            RecordKind::tool_result(tool, ToolOutcomeKind::Failed, preview, 1, false, 0),
        )
    }

    fn ended(session: &str, outcome: SessionOutcome) -> Record {
        record(session, RecordKind::SessionEnded { outcome })
    }

    fn incomplete(reason: &str) -> SessionOutcome {
        SessionOutcome::Incomplete {
            reason: reason.to_owned(),
        }
    }

    #[test]
    fn tool_missing_from_unknown_tools_and_skill_requires() {
        let session = vec![
            record(
                "s",
                RecordKind::tool_result("browser", ToolOutcomeKind::UnknownTool, "", 1, false, 0),
            ),
            record(
                "s",
                RecordKind::tool_result("browser", ToolOutcomeKind::UnknownTool, "", 1, false, 0),
            ),
            record(
                "s",
                RecordKind::SkillRequires {
                    skill: "web".to_owned(),
                    tool: "playwright".to_owned(),
                },
            ),
            record(
                "s",
                RecordKind::tool_result("glob", ToolOutcomeKind::Completed, "", 1, false, 0),
            ),
        ];
        let found = tool_missing(&session);
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(
            found[0],
            SignalKind::ToolMissing {
                tool: "browser".to_owned(),
                skill: None
            }
        );
        assert_eq!(
            found[1],
            SignalKind::ToolMissing {
                tool: "playwright".to_owned(),
                skill: Some("web".to_owned())
            }
        );
        assert!(tool_missing(&session[3..]).is_empty());
    }

    #[test]
    fn repeated_failures_need_three_of_the_same_class() {
        let session = vec![
            failed("s", "shell", "error 1"),
            failed("s", "shell", "error 2"),
            failed("s", "shell", "error 3"),
            failed("s", "edit_file", "no match"),
            failed("s", "edit_file", "no match"),
            failed("s", "edit_file", "something else"),
        ];
        let found = tool_failed_repeatedly(&session);
        assert_eq!(found.len(), 1);
        assert!(matches!(
            &found[0],
            SignalKind::ToolFailedRepeatedly { tool, count: 3, .. } if tool == "shell"
        ));
        assert!(tool_failed_repeatedly(&session[3..]).is_empty());
    }

    #[test]
    fn review_loop_exhausted_carries_the_last_item_count() {
        let session = vec![
            record("s", RecordKind::review(false, &["first".to_owned()])),
            record(
                "s",
                RecordKind::review(false, &["second".to_owned(), "third".to_owned()]),
            ),
            ended("s", incomplete(REVIEW_EXHAUSTED)),
        ];
        assert_eq!(
            review_loop_exhausted(&session),
            vec![SignalKind::ReviewLoopExhausted { items: 2 }]
        );
        let denied = vec![ended(
            "s",
            SessionOutcome::Denied {
                reason: REVIEW_EXHAUSTED.to_owned(),
            },
        )];
        assert_eq!(review_loop_exhausted(&denied).len(), 1);
        let done = vec![
            record("s", RecordKind::review(false, &["x".to_owned()])),
            ended("s", SessionOutcome::Done),
        ];
        assert!(review_loop_exhausted(&done).is_empty());
    }

    #[test]
    fn verify_loop_exhausted_from_repeated_checks_or_the_outcome() {
        let repeated = vec![
            record("s", RecordKind::check("cargo test", false)),
            record("s", RecordKind::check("cargo clippy", true)),
            record("s", RecordKind::check("cargo test", false)),
            ended("s", SessionOutcome::Done),
        ];
        assert_eq!(
            verify_loop_exhausted(&repeated),
            vec![SignalKind::VerifyLoopExhausted {
                check: "cargo".to_owned()
            }]
        );
        let outcome_only = vec![
            record("s", RecordKind::check("pytest", false)),
            ended("s", incomplete(VERIFY_EXHAUSTED)),
        ];
        assert_eq!(
            verify_loop_exhausted(&outcome_only),
            vec![SignalKind::VerifyLoopExhausted {
                check: "pytest".to_owned()
            }]
        );
        let different = vec![
            record("s", RecordKind::check("cargo test", false)),
            record("s", RecordKind::check("cargo clippy", false)),
            ended("s", SessionOutcome::Done),
        ];
        assert!(verify_loop_exhausted(&different).is_empty());
    }

    #[test]
    fn compaction_thrash_needs_two_close_compactions() {
        let compacted = || {
            record(
                "s",
                RecordKind::Compacted {
                    dropped: 4,
                    provider: "p".to_owned(),
                    model: "m".to_owned(),
                    tokens: None,
                },
            )
        };
        let mut thrash = vec![compacted()];
        thrash.extend((0..3).map(|_| turn("s", "p", 1)));
        thrash.push(compacted());
        assert_eq!(
            compaction_thrash(&thrash),
            vec![SignalKind::CompactionThrash { turns_apart: 3 }]
        );
        let mut spaced = vec![compacted()];
        spaced.extend((0..10).map(|_| turn("s", "p", 1)));
        spaced.push(compacted());
        assert!(compaction_thrash(&spaced).is_empty());
        assert!(compaction_thrash(&[compacted()]).is_empty());
    }

    #[test]
    fn user_flags_become_signals() {
        let session = vec![
            record(
                "s",
                RecordKind::user_flag("glob ignores hidden files").expect("ok"),
            ),
            turn("s", "p", 1),
        ];
        assert_eq!(
            user_flags(&session),
            vec![SignalKind::UserFlag {
                text: "glob ignores hidden files".to_owned()
            }]
        );
        assert!(user_flags(&session[1..]).is_empty());
    }

    #[test]
    fn permission_friction_counts_yes_without_always_across_sessions() {
        let answer = |session: &str, rule: &str, answer: PermissionAnswer| {
            record(session, RecordKind::permission("shell", rule, answer))
        };
        let mut history: Vec<Record> = (0..5)
            .map(|i| answer(&format!("s{i}"), "shell:cargo *", PermissionAnswer::Yes))
            .collect();
        history.push(answer("s9", "shell:cargo *", PermissionAnswer::No));
        history
            .extend((0..5).map(|i| answer(&format!("t{i}"), "shell:git *", PermissionAnswer::Yes)));
        history.push(answer("t9", "shell:git *", PermissionAnswer::Always));
        history.extend((0..4).map(|i| answer(&format!("u{i}"), "shell:ls", PermissionAnswer::Yes)));
        assert_eq!(
            permission_friction(&history),
            vec![SignalKind::PermissionFriction {
                tool: "shell".to_owned(),
                count: 5
            }]
        );
    }

    #[test]
    fn model_slow_uses_the_median_over_recent_sessions() {
        let mut history: Vec<Record> = (0..6)
            .map(|i| turn(&format!("s{i}"), "slow", 100))
            .collect();
        history.push(turn("s0", "slow", 1));
        history.extend((0..6).map(|i| turn(&format!("s{i}"), "fast", 1)));
        history.extend((0..3).map(|i| turn(&format!("s{i}"), "few", 1000)));
        assert_eq!(
            model_slow(&history, 50),
            vec![SignalKind::ModelSlow {
                provider: "slow".to_owned(),
                median_ms: 100,
                limit_ms: 50
            }]
        );
        assert!(model_slow(&history, 100).is_empty());
        let mut old: Vec<Record> = (0..6)
            .map(|i| {
                let mut record = turn(&format!("old{i}"), "slow", 10_000);
                record.ts -= Duration::days(30);
                record
            })
            .collect();
        old.extend((0..MODEL_SESSIONS).map(|i| turn(&format!("new{i}"), "slow", 1)));
        assert!(
            model_slow(&old, 50).is_empty(),
            "sessions older than the last twenty do not count"
        );
    }

    #[test]
    fn model_errors_compares_the_rate_to_the_line() {
        let error = |session: &str, provider: &str| {
            record(
                session,
                RecordKind::ProviderError {
                    provider: provider.to_owned(),
                    kind: "http".to_owned(),
                    http_status: Some(503),
                },
            )
        };
        let mut history: Vec<Record> = (0..6).map(|i| turn(&format!("s{i}"), "flaky", 1)).collect();
        history.extend((0..4).map(|i| error(&format!("s{i}"), "flaky")));
        history.extend((0..6).map(|i| turn(&format!("s{i}"), "solid", 1)));
        history.push(error("s0", "solid"));
        history.push(error("s0", "rare"));
        assert_eq!(
            model_errors(&history, 20),
            vec![SignalKind::ModelErrors {
                provider: "flaky".to_owned(),
                rate_percent: 40,
                limit_percent: 20
            }]
        );
        assert!(model_errors(&history, 40).is_empty());
    }

    #[test]
    fn classify_attributes_everything_to_the_session() {
        let session = vec![
            record(
                "sess",
                RecordKind::tool_result("nope", ToolOutcomeKind::UnknownTool, "", 1, false, 0),
            ),
            ended("sess", SessionOutcome::Done),
        ];
        let signals = classify(&session, &session, &TelemetryConfig::default());
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].session, "sess");
        assert_eq!(signals[0].ticket, None);
        assert!(classify(&[], &[], &TelemetryConfig::default()).is_empty());
    }
}
