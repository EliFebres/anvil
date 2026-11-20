//! The hash chain: one record per line, each sealed with its own SHA-256
//! and the SHA-256 of the record before it.
//!
//! # Canonical form
//!
//! A record's canonical bytes are the compact JSON (no spaces, UTF-8, no
//! escaping beyond what JSON requires) of [`RecordBody`] with its fields in
//! the order they are declared: `seq`, `ts`, `session`, `agent`, `user`,
//! `host`, `version`, `commit`, `binary_sha256`, `policy`, `event`, `prev`.
//! No field holds a floating-point number. The stored line is the canonical
//! bytes with `,"hash":"<64 hex>"` put before the closing `}`, where the
//! hash is the SHA-256 of the canonical bytes. `prev` of the first record
//! is [`GENESIS`], 64 zeros; `seq` counts from 0.
//!
//! [`verify`] reads the file as UTF-8 with LF line ends; every line, the
//! last included, ends with a newline. For each line it cuts at the last
//! `,"hash":"`, adds `}` to the part before, compares its SHA-256 with the
//! 64 lowercase digits after, parses it (a duplicate key is an error),
//! serializes the parsed record again and requires the same bytes, so an
//! extra, missing or reordered field, an escape JSON does not require, or a
//! space is a break even when the line was resealed. Then `seq` must count
//! up by one from 0, `prev` must be the hash of the line before, and every
//! record must name the first one's session.
//!
//! `verify_audit.py` next to this file checks a file by the same rules
//! without anvil (`python verify_audit.py audit.jsonl`), including the
//! fields and types of every event; `tests/python_check.rs` runs it and
//! [`verify`] over the same intact and tampered files and requires the
//! same verdict.
//!
//! The chain shows a record edited, removed or moved inside the file. It
//! cannot show that records were cut from the end, or that the whole file
//! was rewritten by someone who can write it; the mirror directory of the
//! policy exists for that.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::event::{AuditEvent, sha256_hex};

/// `prev` of the first record of a session.
pub const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// What separates a record's canonical bytes from its own hash.
const HASH_KEY: &str = ",\"hash\":\"";

/// Why an audit file could not be written, read or trusted.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AuditError {
    /// A file or directory could not be read or written.
    #[error("cannot write or read the audit log {}: {detail}", path.display())]
    Io {
        /// The path that failed.
        path: PathBuf,
        /// The operating system's message.
        detail: String,
    },
    /// A record could not be serialized.
    #[error("cannot serialize an audit record: {detail}")]
    Serialize {
        /// The serializer's message.
        detail: String,
    },
    /// The chain breaks at a line.
    #[error("line {line}: {reason}")]
    Broken {
        /// The line, starting at 1.
        line: usize,
        /// What is wrong with it.
        reason: String,
    },
    /// The mirror does not hold the same records as the session's own file,
    /// so one of them was cut, removed or rewritten.
    #[error(
        "the audit mirror {} does not hold the same records as {}",
        mirror.display(),
        primary.display()
    )]
    MirrorDiffers {
        /// The session's own file.
        primary: PathBuf,
        /// The mirror.
        mirror: PathBuf,
    },
    /// An earlier write failed under a policy that requires the audit log,
    /// so nothing more is written.
    #[error("the audit log stopped after a failed write: {detail}")]
    Stopped {
        /// The first failure.
        detail: String,
    },
}

impl AuditError {
    /// Wraps an I/O error with the path it concerned.
    ///
    /// # Arguments
    ///
    /// * `path` - The path that failed.
    /// * `error` - The error.
    ///
    /// # Returns
    ///
    /// The wrapped error.
    pub fn io(path: impl Into<PathBuf>, error: &std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            detail: error.to_string(),
        }
    }
}

/// Who and what produced the records: the same on every record of a
/// session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    /// The operating system user.
    pub user: String,
    /// The host name.
    pub host: String,
    /// The anvil version.
    pub version: String,
    /// The commit anvil was built from, or `unknown`.
    pub commit: String,
    /// SHA-256 of the running anvil binary, or `unknown`.
    pub binary_sha256: String,
    /// `sha256:<hex>` of the managed policy, or `unmanaged`.
    pub policy: String,
}

/// A record without its own hash: what the hash is computed over.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordBody {
    /// Position in the session's chain, from 0.
    pub seq: u64,
    /// UTC time, RFC 3339 with milliseconds and `Z`.
    pub ts: String,
    /// The session id.
    pub session: String,
    /// `main`, or the name of the subagent that acted.
    pub agent: String,
    /// The operating system user.
    pub user: String,
    /// The host name.
    pub host: String,
    /// The anvil version.
    pub version: String,
    /// The commit anvil was built from, or `unknown`.
    pub commit: String,
    /// SHA-256 of the running anvil binary, or `unknown`.
    pub binary_sha256: String,
    /// `sha256:<hex>` of the managed policy, or `unmanaged`.
    pub policy: String,
    /// What happened.
    pub event: AuditEvent,
    /// The hash of the record before, or [`GENESIS`].
    pub prev: String,
}

/// One record read back from a file, with its hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealedRecord {
    /// The record.
    pub body: RecordBody,
    /// Its own hash.
    pub hash: String,
}

/// Seals a record into the line that is stored.
///
/// # Arguments
///
/// * `body` - The record.
///
/// # Returns
///
/// The line without a newline, and the record's hash.
///
/// # Errors
///
/// [`AuditError::Serialize`] when the record cannot be serialized.
pub fn seal(body: &RecordBody) -> Result<(String, String), AuditError> {
    let canonical = serde_json::to_string(body).map_err(|error| AuditError::Serialize {
        detail: error.to_string(),
    })?;
    debug_assert!(
        canonical.ends_with('}'),
        "a record serializes as a JSON object"
    );
    let hash = sha256_hex(canonical.as_bytes());
    let open = &canonical[..canonical.len() - 1];
    Ok((format!("{open}{HASH_KEY}{hash}\"}}"), hash))
}

/// Opens one stored line: checks its hash and parses it.
///
/// # Arguments
///
/// * `line` - The line, without its newline.
///
/// # Returns
///
/// The record.
///
/// # Errors
///
/// A reason when the line has no hash, the hash does not match the
/// content, or the content is not a record.
pub fn open_line(line: &str) -> Result<SealedRecord, String> {
    let cut = line
        .rfind(HASH_KEY)
        .ok_or_else(|| "the record has no hash".to_owned())?;
    let rest = &line[cut + HASH_KEY.len()..];
    let hash = rest
        .strip_suffix("\"}")
        .filter(|hash| hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| "the record's hash is not 64 hex digits at the end".to_owned())?;
    let canonical = format!("{}}}", &line[..cut]);
    if sha256_hex(canonical.as_bytes()) != hash {
        return Err("the hash does not match the record: it was edited".to_owned());
    }
    let body: RecordBody = serde_json::from_str(&canonical)
        .map_err(|error| format!("the record does not parse: {error}"))?;
    // Byte equality with the re-serialization, so one record has exactly one
    // valid line and two verifiers cannot read the same line differently.
    let again = serde_json::to_string(&body)
        .map_err(|error| format!("the record does not serialize: {error}"))?;
    if again != canonical {
        return Err(
            "the record is not in canonical form (an extra or reordered field, an escape or a space)"
                .to_owned(),
        );
    }
    Ok(SealedRecord {
        body,
        hash: hash.to_owned(),
    })
}

/// What [`verify`] found in an intact file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    /// The records, in order.
    pub records: Vec<SealedRecord>,
}

impl Verified {
    /// The hash of the last record, or [`GENESIS`] for an empty file.
    ///
    /// # Returns
    ///
    /// The hash.
    pub fn last_hash(&self) -> &str {
        self.records
            .last()
            .map_or(GENESIS, |record| record.hash.as_str())
    }
}

/// Walks a chain and stops at the first break.
///
/// Every line must end with a newline; a blank line is a break.
///
/// # Arguments
///
/// * `text` - The file's contents.
///
/// # Returns
///
/// The records.
///
/// # Errors
///
/// [`AuditError::Broken`] naming the first line that does not follow: an
/// edited record (its hash does not match), a removed or moved record (its
/// `seq` or `prev` does not follow from the line before), a record of
/// another session, or a last line without a newline.
pub fn verify(text: &str) -> Result<Verified, AuditError> {
    let mut records: Vec<SealedRecord> = Vec::new();
    let mut lines: Vec<&str> = text.split('\n').collect();
    let complete = lines.pop().is_some_and(str::is_empty);
    for (index, line) in lines.iter().enumerate() {
        let broken = |reason: String| AuditError::Broken {
            line: index + 1,
            reason,
        };
        let record = open_line(line).map_err(broken)?;
        let expected_seq = u64::try_from(records.len()).unwrap_or(u64::MAX);
        let expected_prev = records.last().map_or(GENESIS, |last| last.hash.as_str());
        if record.body.seq != expected_seq {
            return Err(broken(format!(
                "seq is {} where {expected_seq} was expected: a record was removed or moved",
                record.body.seq
            )));
        }
        if record.body.prev != expected_prev {
            return Err(broken(
                "prev is not the hash of the record before: a record was removed or moved"
                    .to_owned(),
            ));
        }
        if let Some(first) = records.first()
            && first.body.session != record.body.session
        {
            return Err(broken(format!(
                "the record belongs to session {}, not {}",
                record.body.session, first.body.session
            )));
        }
        records.push(record);
    }
    if !complete {
        return Err(AuditError::Broken {
            line: lines.len() + 1,
            reason: "the last line has no newline: the file was cut or appended to by hand"
                .to_owned(),
        });
    }
    Ok(Verified { records })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(seq: u64, prev: &str) -> RecordBody {
        RecordBody {
            seq,
            ts: "2026-11-19T21:00:00.000Z".to_owned(),
            session: "s1".to_owned(),
            agent: "main".to_owned(),
            user: "u".to_owned(),
            host: "h".to_owned(),
            version: "0.0.0".to_owned(),
            commit: "unknown".to_owned(),
            binary_sha256: "unknown".to_owned(),
            policy: "unmanaged".to_owned(),
            event: AuditEvent::SessionEnd {
                outcome: format!("o{seq}"),
            },
            prev: prev.to_owned(),
        }
    }

    /// A valid chain of `count` records.
    fn chain(count: u64) -> Vec<String> {
        let mut prev = GENESIS.to_owned();
        (0..count)
            .map(|seq| {
                let (line, hash) = seal(&body(seq, &prev)).expect("seal");
                prev = hash;
                line
            })
            .collect()
    }

    fn file(lines: &[String]) -> String {
        lines.iter().map(|line| format!("{line}\n")).collect()
    }

    #[test]
    fn a_sealed_line_is_the_canonical_json_with_its_hash_last() {
        let (line, hash) = seal(&body(0, GENESIS)).expect("seal");
        assert!(line.starts_with(r#"{"seq":0,"ts":"#), "{line}");
        assert!(line.ends_with(&format!(r#","hash":"{hash}"}}"#)), "{line}");
        let canonical = serde_json::to_string(&body(0, GENESIS)).expect("json");
        assert_eq!(hash, sha256_hex(canonical.as_bytes()));
        let order = [
            "seq",
            "ts",
            "session",
            "agent",
            "user",
            "host",
            "version",
            "commit",
            "binary_sha256",
            "policy",
            "event",
            "prev",
            "hash",
        ];
        let positions: Vec<usize> = order
            .iter()
            .map(|key| line.find(&format!("\"{key}\":")).expect(key))
            .collect();
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]), "{line}");
    }

    #[test]
    fn an_intact_chain_verifies() {
        let verified = verify(&file(&chain(4))).expect("intact");
        assert_eq!(verified.records.len(), 4);
        assert_eq!(verified.records[3].body.seq, 3);
        assert!(verify("").expect("empty").records.is_empty());
    }

    #[test]
    fn the_audit_chain_detects_edit_removal_and_reorder() {
        let lines = chain(4);

        let mut edited = lines.clone();
        edited[1] = edited[1].replace("\"o1\"", "\"o9\"");
        assert!(
            matches!(verify(&file(&edited)), Err(AuditError::Broken { line: 2, ref reason }) if reason.contains("edited")),
            "{:?}",
            verify(&file(&edited))
        );

        let mut removed = lines.clone();
        removed.remove(2);
        assert!(
            matches!(
                verify(&file(&removed)),
                Err(AuditError::Broken { line: 3, .. })
            ),
            "{:?}",
            verify(&file(&removed))
        );

        let mut swapped = lines.clone();
        swapped.swap(1, 2);
        assert!(
            matches!(
                verify(&file(&swapped)),
                Err(AuditError::Broken { line: 2, .. })
            ),
            "{:?}",
            verify(&file(&swapped))
        );

        let mut first_removed = lines.clone();
        first_removed.remove(0);
        assert!(matches!(
            verify(&file(&first_removed)),
            Err(AuditError::Broken { line: 1, .. })
        ));
    }

    #[test]
    fn a_record_resealed_after_an_edit_still_breaks_the_next_link() {
        let lines = chain(3);
        let mut resealed = body(1, &open_line(&lines[0]).expect("open").hash);
        resealed.event = AuditEvent::SessionEnd {
            outcome: "replaced".to_owned(),
        };
        let (line, _) = seal(&resealed).expect("seal");
        let tampered = vec![lines[0].clone(), line, lines[2].clone()];
        assert!(matches!(
            verify(&file(&tampered)),
            Err(AuditError::Broken { line: 3, .. })
        ));
    }

    #[test]
    fn blank_lines_cut_lines_and_foreign_records_are_breaks() {
        let lines = chain(2);
        let with_blank = format!("{}\n\n{}\n", lines[0], lines[1]);
        assert!(matches!(
            verify(&with_blank),
            Err(AuditError::Broken { line: 2, .. })
        ));
        let cut = format!("{}\n{}", lines[0], lines[1]);
        assert!(matches!(
            verify(&cut),
            Err(AuditError::Broken { line: 2, .. })
        ));
        let mut other = body(1, &open_line(&lines[0]).expect("open").hash);
        other.session = "s2".to_owned();
        let (other, _) = seal(&other).expect("seal");
        assert!(matches!(
            verify(&file(&[lines[0].clone(), other])),
            Err(AuditError::Broken { line: 2, ref reason }) if reason.contains("session")
        ));
    }

    /// Reseals `line` after `edit` changed its canonical bytes.
    fn resealed(line: &str, edit: impl Fn(&str) -> String) -> String {
        let cut = line.rfind(HASH_KEY).expect("hash");
        let body = edit(&format!("{}}}", &line[..cut]));
        let hash = sha256_hex(body.as_bytes());
        format!("{}{HASH_KEY}{hash}\"}}", &body[..body.len() - 1])
    }

    #[test]
    fn a_line_that_is_not_the_canonical_form_is_a_break_even_when_resealed() {
        let lines = chain(1);
        let cases: Vec<(&str, String)> = vec![
            (
                "extra field",
                resealed(&lines[0], |b| {
                    b.replacen(r#""agent":"main""#, r#""agent":"main","extra":"x""#, 1)
                }),
            ),
            (
                "escape",
                resealed(&lines[0], |b| {
                    b.replacen(r#""o0""#, &format!("\"{}u006f0\"", char::from(92u8)), 1)
                }),
            ),
            (
                "duplicate key",
                resealed(&lines[0], |b| {
                    b.replacen(r#""agent":"main""#, r#""agent":"main","agent":"evil""#, 1)
                }),
            ),
            (
                "key order",
                resealed(&lines[0], |b| {
                    b.replacen(r#""user":"u","host":"h""#, r#""host":"h","user":"u""#, 1)
                }),
            ),
            (
                "space",
                resealed(&lines[0], |b| b.replacen(r#""seq":0"#, r#""seq": 0"#, 1)),
            ),
        ];
        for (what, line) in cases {
            assert_ne!(line, lines[0], "{what} changed the line");
            assert!(
                matches!(
                    verify(&format!("{line}\n")),
                    Err(AuditError::Broken { line: 1, .. })
                ),
                "{what}: {:?}",
                verify(&format!("{line}\n"))
            );
        }
    }
}
