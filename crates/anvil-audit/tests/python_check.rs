//! The documented Python check (`src/audit/verify_audit.py`) and
//! `anvil_audit::audit::verify` must agree on every file: intact logs with
//! every event type, and each kind of tampering. Skipped when no Python 3 is
//! on the PATH.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use anvil_audit::audit::chain::seal;
use anvil_audit::audit::{
    AuditEvent, Content, GENESIS, ProviderEntry, RecordBody, RecordMode, sha256_hex, verify,
};

const SCRIPT: &str = include_str!("../src/audit/verify_audit.py");

/// The Python 3 interpreter on the PATH, if there is one that runs.
fn python() -> Option<&'static str> {
    ["python3", "python"].into_iter().find(|program| {
        Command::new(program)
            .arg("--version")
            .output()
            .is_ok_and(|out| {
                out.status.success()
                    && String::from_utf8_lossy(&[out.stdout, out.stderr].concat())
                        .starts_with("Python 3")
            })
    })
}

/// One record of every event type, so the script's schema cannot drift
/// from `AuditEvent` unnoticed.
fn events() -> Vec<AuditEvent> {
    vec![
        AuditEvent::SessionStart {
            mode: "run".to_owned(),
            providers: vec![ProviderEntry {
                name: "p".to_owned(),
                host: "h:443".to_owned(),
                model: "m".to_owned(),
            }],
            roles: BTreeMap::from([
                ("summarizer".to_owned(), "p".to_owned()),
                ("coder".to_owned(), "p".to_owned()),
            ]),
            flags: BTreeMap::from([("yes".to_owned(), "true".to_owned())]),
        },
        AuditEvent::PromptSubmitted {
            content: Content::of("é \"quoted\"\n\t\u{1b}\u{7f}\u{2028}", RecordMode::Full),
        },
        AuditEvent::ModelRequest {
            role: "coder".to_owned(),
            provider: "p".to_owned(),
            host: "h:443".to_owned(),
            model: "m".to_owned(),
            input_tokens: u64::MAX,
            output_tokens: 1,
            cached_tokens: 0,
            stop_reason: "end_turn".to_owned(),
            redactions: 2,
        },
        AuditEvent::ToolCall {
            call_id: "c1".to_owned(),
            tool: "shell".to_owned(),
            arguments: Content::of("{\"command\":\"ls\"}", RecordMode::Digest),
        },
        AuditEvent::PermissionDecision {
            tool: "shell".to_owned(),
            decision: "allow".to_owned(),
            scope: "saved".to_owned(),
            source: "prompt".to_owned(),
        },
        AuditEvent::ToolResult {
            call_id: "c1".to_owned(),
            tool: "shell".to_owned(),
            outcome: "failed".to_owned(),
            error: true,
            bytes: 3,
            sha256: sha256_hex(b"abc"),
            truncated: false,
            exit_code: Some(-1),
        },
        AuditEvent::ToolResult {
            call_id: "c2".to_owned(),
            tool: "read_file".to_owned(),
            outcome: "completed".to_owned(),
            error: false,
            bytes: 0,
            sha256: sha256_hex(b""),
            truncated: true,
            exit_code: None,
        },
        AuditEvent::FileChange {
            call_id: "c3".to_owned(),
            path: "a.txt".to_owned(),
            before: None,
            after: sha256_hex(b"x"),
        },
        AuditEvent::PlanApproval {
            decision: "approved".to_owned(),
            answered_by: "user".to_owned(),
        },
        AuditEvent::PolicyRefusal {
            what: "a `shell` call".to_owned(),
            key: "permissions.deny".to_owned(),
        },
        AuditEvent::SessionEnd {
            outcome: "done".to_owned(),
        },
    ]
}

/// A sealed chain of `events`, one line each without newlines.
fn chain() -> Vec<String> {
    let mut prev = GENESIS.to_owned();
    events()
        .into_iter()
        .enumerate()
        .map(|(seq, event)| {
            let body = RecordBody {
                seq: u64::try_from(seq).expect("seq"),
                ts: "2026-11-19T21:00:00.000Z".to_owned(),
                session: "s1".to_owned(),
                agent: "main".to_owned(),
                user: "u".to_owned(),
                host: "h".to_owned(),
                version: "0.0.0".to_owned(),
                commit: "unknown".to_owned(),
                binary_sha256: "unknown".to_owned(),
                policy: "unmanaged".to_owned(),
                event,
                prev: prev.clone(),
            };
            let (line, hash) = seal(&body).expect("seal");
            prev = hash;
            line
        })
        .collect()
}

/// Replaces the canonical bytes of `line` through `edit` and reseals it.
fn resealed(line: &str, edit: impl Fn(&str) -> String) -> String {
    let cut = line.rfind(",\"hash\":\"").expect("hash");
    let body = edit(&format!("{}}}", &line[..cut]));
    format!(
        "{},\"hash\":\"{}\"}}",
        &body[..body.len() - 1],
        sha256_hex(body.as_bytes())
    )
}

/// Reseals every line from `from` on so each `prev` follows again.
fn relinked(mut lines: Vec<String>, from: usize) -> Vec<String> {
    for index in from..lines.len() {
        let prev =
            lines[index - 1][lines[index - 1].len() - 66..lines[index - 1].len() - 2].to_owned();
        let line = lines[index].clone();
        lines[index] = resealed(&line, |body| {
            let at = body.rfind(",\"prev\":\"").expect("prev");
            format!("{},\"prev\":\"{prev}\"}}", &body[..at])
        });
    }
    lines
}

fn file(lines: &[String]) -> Vec<u8> {
    lines
        .iter()
        .map(|line| format!("{line}\n"))
        .collect::<String>()
        .into_bytes()
}

/// The files both verifiers read, with whether each must verify.
fn cases() -> Vec<(&'static str, Vec<u8>, bool)> {
    let lines = chain();
    let edit = |index: usize, from: &str, to: &str| {
        let mut changed = lines.clone();
        changed[index] = resealed(&changed[index], |body| body.replacen(from, to, 1));
        file(&relinked(changed, index + 1))
    };
    let mut swapped = lines.clone();
    swapped.swap(2, 3);
    let mut removed = lines.clone();
    removed.remove(4);
    let mut duplicated = lines.clone();
    duplicated.insert(3, lines[2].clone());
    let crlf = lines
        .iter()
        .map(|line| format!("{line}\r\n"))
        .collect::<String>()
        .into_bytes();
    let mut cut_last = file(&lines);
    cut_last.truncate(cut_last.len() - 30);
    let mut hand_edit = lines.clone();
    hand_edit[0] = hand_edit[0].replacen("\"run\"", "\"chat\"", 1);
    vec![
        ("intact", file(&lines), true),
        ("empty", Vec::new(), true),
        ("end cut at a line", file(&lines[..5]), true),
        ("edited", file(&hand_edit), false),
        (
            "edited and every later record resealed",
            edit(8, "\"answered_by\":\"user\"", "\"answered_by\":\"bot\""),
            true,
        ),
        ("swapped", file(&swapped), false),
        ("removed", file(&removed), false),
        ("duplicate seq", file(&duplicated), false),
        ("crlf", crlf, false),
        ("last line cut", cut_last, false),
        (
            "blank line",
            [file(&lines[..2]), b"\n".to_vec(), file(&lines[2..])].concat(),
            false,
        ),
        (
            "extra field",
            edit(0, "\"agent\":\"main\"", "\"agent\":\"main\",\"x\":1"),
            false,
        ),
        (
            "extra event field",
            edit(10, "\"outcome\":\"done\"", "\"outcome\":\"done\",\"x\":1"),
            false,
        ),
        (
            "duplicate key",
            edit(
                0,
                "\"agent\":\"main\"",
                "\"agent\":\"main\",\"agent\":\"x\"",
            ),
            false,
        ),
        (
            "duplicate event key",
            edit(
                10,
                "\"type\":\"session_end\"",
                "\"type\":\"session_end\",\"type\":\"session_end\"",
            ),
            false,
        ),
        (
            "escaped letter",
            edit(0, "\"main\"", "\"m\\u0061in\""),
            false,
        ),
        ("uppercase escape", edit(1, "\\u001b", "\\u001B"), false),
        ("space", edit(0, "\"seq\":0", "\"seq\": 0"), false),
        (
            "reordered fields",
            edit(
                0,
                "\"user\":\"u\",\"host\":\"h\"",
                "\"host\":\"h\",\"user\":\"u\"",
            ),
            false,
        ),
        (
            "reordered event fields",
            edit(
                9,
                "\"what\":\"a `shell` call\",\"key\":\"permissions.deny\"",
                "\"key\":\"permissions.deny\",\"what\":\"a `shell` call\"",
            ),
            false,
        ),
        (
            "unsorted roles",
            edit(
                0,
                "\"coder\":\"p\",\"summarizer\":\"p\"",
                "\"summarizer\":\"p\",\"coder\":\"p\"",
            ),
            false,
        ),
        (
            "string for a number",
            edit(2, "\"output_tokens\":1", "\"output_tokens\":\"1\""),
            false,
        ),
        (
            "float for a number",
            edit(2, "\"output_tokens\":1", "\"output_tokens\":1.0"),
            false,
        ),
        (
            "number beyond u64",
            edit(
                2,
                &format!("\"input_tokens\":{}", u64::MAX),
                "\"input_tokens\":18446744073709551616",
            ),
            false,
        ),
        ("null text", edit(3, "}}", ",\"text\":null}}"), false),
        (
            "unknown event",
            edit(10, "\"session_end\"", "\"session_stop\""),
            false,
        ),
        (
            "uppercase hash",
            {
                let mut upper = lines.clone();
                let at = upper[10].len() - 66;
                let hash = upper[10][at..at + 64].to_ascii_uppercase();
                upper[10].replace_range(at..at + 64, &hash);
                file(&upper)
            },
            false,
        ),
    ]
}

#[test]
fn rust_verify_matches_the_expected_verdicts() {
    for (name, bytes, intact) in cases() {
        let verdict = String::from_utf8(bytes)
            .ok()
            .is_some_and(|text| verify(&text).is_ok());
        assert_eq!(verdict, intact, "{name}");
    }
}

#[test]
fn the_documented_python_check_agrees_with_verify() {
    let Some(python) = python() else {
        eprintln!("skipped: no Python 3 on the PATH");
        return;
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let script = dir.path().join("verify_audit.py");
    std::fs::write(&script, SCRIPT).expect("script");
    for (index, (name, bytes, intact)) in cases().into_iter().enumerate() {
        let path = dir.path().join(format!("case{index}.jsonl"));
        std::fs::write(&path, &bytes).expect("case");
        assert_eq!(run(python, &script, &path), intact, "python on {name}");
    }
}

/// Whether the Python check says the file is intact.
fn run(python: &str, script: &Path, file: &Path) -> bool {
    let out = Command::new(python)
        .arg("-I")
        .arg(script)
        .arg(file)
        .output()
        .expect("python runs");
    let printed = String::from_utf8_lossy(&out.stdout);
    assert!(
        printed.starts_with("intact") || printed.starts_with("broken"),
        "{printed} {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.status.success()
}
