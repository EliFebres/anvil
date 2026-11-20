//! The regression test for the line-ending incident of Nov 19, 2026: a
//! model read a CRLF file with `read_file`, copied a multi-line region from
//! what it was shown, and `edit_file` answered `old string not found`. Here
//! the `anvil` binary runs a scripted `fake` provider through the same two
//! calls, and the edit must succeed on the first try and leave the file CRLF.

use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::Value;

/// The file under edit, with CRLF line endings as a Windows checkout has.
const HERO: &str = "export function Hero() {\r\n  return (\r\n    <nav>\r\n      <a href=\"/docs\">Docs</a>\r\n      <a href=\"/blog\">Blog</a>\r\n    </nav>\r\n  );\r\n}\r\n";

/// The region the model copies from `read_file`'s output: LF breaks, as
/// `read_file` shows lines without their endings.
const OLD: &str = "      <a href=\"/docs\">Docs</a>\n      <a href=\"/blog\">Blog</a>";

/// The replacement the model writes.
const NEW: &str = "      <a href=\"/docs\">Guides</a>\n      <a href=\"/blog\">News</a>";

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, text).expect("write");
}

/// Every string anywhere inside a JSON value.
fn strings(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(text) => out.push(text.clone()),
        Value::Array(items) => items.iter().for_each(|item| strings(item, out)),
        Value::Object(map) => map.values().for_each(|item| strings(item, out)),
        _ => {}
    }
}

/// The text `read_file` showed, with its line-number prefixes removed.
fn shown_text(read_result: &str) -> String {
    read_result
        .lines()
        .filter_map(|line| {
            let (number, text) = line.split_once('\t')?;
            number.trim().parse::<usize>().ok().map(|_| text)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn an_edit_copied_from_read_file_succeeds_first_try_on_a_crlf_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path().join("repo");
    let home = dir.path().join("home");
    let script = dir.path().join("script.json");
    write(&repo.join("Hero.tsx"), HERO);
    let turns = serde_json::json!({"turns": [
        {"text": "1. [ ] rename the two navigation labels"},
        {"tool_calls": [{"name": "read_file", "arguments": {"path": "Hero.tsx"}}]},
        {"tool_calls": [{"name": "edit_file", "arguments": {"path": "Hero.tsx", "old": OLD, "new": NEW}}]},
        {"text": "renamed"},
    ]});
    write(&script, &turns.to_string());
    write(
        &home.join(".anvil").join("config.toml"),
        &format!(
            "[providers.fake]\nprotocol = \"fake\"\nmodel = \"m\"\ncontext_window = 100000\nsupports_tools = true\nsupports_reasoning = false\nscript = '{}'\n\n[roles]\ncoder = \"fake\"\nsummarizer = \"fake\"\n",
            script.display()
        ),
    );
    let out = Command::new(env!("CARGO_BIN_EXE_anvil"))
        .current_dir(&repo)
        .env("ANVIL_HOME", &home)
        .env("ANVIL_LOG", "off")
        .env("ANVIL_NO_ANIMATION", "1")
        .stdin(Stdio::null())
        .args(["--yes", "run", "rename the two navigation labels"])
        .output()
        .expect("anvil runs");
    let stderr = String::from_utf8_lossy(&out.stderr);

    let sessions = repo.join(".anvil").join("sessions");
    let mut texts = Vec::new();
    for entry in std::fs::read_dir(&sessions)
        .unwrap_or_else(|error| panic!("no sessions ({error}): {stderr}"))
        .flatten()
    {
        let Ok(log) = std::fs::read_to_string(entry.path().join("session.jsonl")) else {
            continue;
        };
        for line in log.lines() {
            if let Ok(value) = serde_json::from_str::<Value>(line) {
                strings(&value, &mut texts);
            }
        }
    }
    let read_result = texts
        .iter()
        .find(|text| text.contains("     1\texport function Hero() {"))
        .unwrap_or_else(|| panic!("no read_file result in the transcript: {stderr}"));
    assert!(
        shown_text(read_result).contains(OLD),
        "the old text is exactly what read_file showed: {read_result}"
    );
    assert!(
        texts
            .iter()
            .any(|text| text.contains("replaced 1 occurrence(s) in Hero.tsx")),
        "the first edit succeeded: {stderr}"
    );
    assert!(
        !texts.iter().any(|text| text.contains("not found in")),
        "no edit failed"
    );
    assert_eq!(
        std::fs::read(repo.join("Hero.tsx")).expect("read"),
        HERO.replace(">Docs<", ">Guides<")
            .replace(">Blog<", ">News<")
            .into_bytes(),
        "the file keeps CRLF on every line"
    );
}
