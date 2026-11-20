//! A readable view of an audit file for `anvil audit show`.

use super::chain::{RecordBody, open_line, verify_log};
use super::event::{AuditEvent, Content};

/// Renders an audit file one line per record, after a header naming the
/// session, and ends with whether the chain verifies.
///
/// # Arguments
///
/// * `text` - The file's contents.
///
/// # Returns
///
/// The text to print.
pub fn render(text: &str) -> String {
    let mut out = String::new();
    let mut header = false;
    for (index, line) in text.lines().enumerate() {
        match open_line(line) {
            Ok(record) => {
                if !header {
                    out.push_str(&header_line(&record.body));
                    header = true;
                }
                out.push_str(&format!(
                    "{:>5}  {}  {:<8} {}\n",
                    record.body.seq,
                    record.body.ts,
                    record.body.agent,
                    describe(&record.body.event)
                ));
            }
            Err(reason) => out.push_str(&format!("line {}: {reason}\n", index + 1)),
        }
    }
    match verify_log(text) {
        Ok(verified) => out.push_str(&format!(
            "chain intact: {} records, last hash {}\n",
            verified.records.len(),
            verified.last_hash()
        )),
        Err(error) => out.push_str(&format!("chain broken: {error}\n")),
    }
    out
}

/// The session, who ran it and with what.
fn header_line(body: &RecordBody) -> String {
    format!(
        "session {} by {}@{}, anvil {} ({}), binary sha256 {}, policy {}\n",
        body.session,
        body.user,
        body.host,
        body.version,
        body.commit,
        body.binary_sha256,
        body.policy
    )
}

/// A prompt or arguments: the text when kept, otherwise size and digest.
fn content(content: &Content) -> String {
    match &content.text {
        Some(text) => format!("{text:?}"),
        None => format!(
            "{} bytes, sha256 {}",
            content.bytes,
            &content.sha256[..content.sha256.len().min(16)]
        ),
    }
}

/// One event in a few words.
fn describe(event: &AuditEvent) -> String {
    match event {
        AuditEvent::SessionStart {
            mode,
            providers,
            roles,
            flags,
        } => {
            let providers: Vec<String> = providers
                .iter()
                .map(|p| format!("{}={}@{}", p.name, p.model, p.host))
                .collect();
            let roles: Vec<String> = roles.iter().map(|(r, p)| format!("{r}={p}")).collect();
            let flags: Vec<String> = flags.iter().map(|(k, v)| format!("{k}={v}")).collect();
            format!(
                "session start ({mode}); providers {}; roles {}; flags {}",
                providers.join(", "),
                roles.join(", "),
                flags.join(", ")
            )
        }
        AuditEvent::PromptSubmitted { content: text } => format!("prompt {}", content(text)),
        AuditEvent::ModelRequest {
            role,
            provider,
            host,
            model,
            input_tokens,
            output_tokens,
            cached_tokens,
            stop_reason,
            redactions,
        } => format!(
            "model request {role} on {provider} ({model} at {host}): {input_tokens} in, {output_tokens} out, {cached_tokens} cached, {stop_reason}, {redactions} redacted"
        ),
        AuditEvent::ToolCall {
            call_id,
            tool,
            arguments,
        } => format!("tool call {tool} [{call_id}] {}", content(arguments)),
        AuditEvent::PermissionDecision {
            tool,
            decision,
            scope,
            source,
        } => format!("permission {decision} {tool} ({scope}, by {source})"),
        AuditEvent::ToolResult {
            call_id,
            tool,
            outcome,
            error,
            bytes,
            sha256,
            truncated,
            exit_code,
        } => {
            let exit = exit_code.map_or_else(String::new, |code| format!(", exit {code}"));
            let error = if *error { ", error" } else { "" };
            let cut = if *truncated { ", truncated" } else { "" };
            format!(
                "tool result {tool} [{call_id}] {outcome}{error}{exit}: {bytes} bytes, sha256 {}{cut}",
                &sha256[..sha256.len().min(16)]
            )
        }
        AuditEvent::FileChange {
            path,
            before,
            after,
            ..
        } => format!(
            "file change {path}: {} -> {}",
            before
                .as_deref()
                .map_or("new", |hash| &hash[..hash.len().min(16)]),
            &after[..after.len().min(16)]
        ),
        AuditEvent::PlanApproval {
            decision,
            answered_by,
        } => format!("plan {decision} by {answered_by}"),
        AuditEvent::PolicyRefusal { what, key } => format!("policy refused {what} ({key})"),
        AuditEvent::SessionEnd { outcome } => format!("session end: {outcome}"),
    }
}

#[cfg(test)]
mod tests {
    use super::super::chain::{GENESIS, seal};
    use super::*;

    #[test]
    fn show_prints_each_record_and_the_verdict() {
        let start = RecordBody {
            seq: 0,
            ts: "2026-11-19T21:00:00.000Z".to_owned(),
            session: "s1".to_owned(),
            agent: "main".to_owned(),
            user: "dev".to_owned(),
            host: "box".to_owned(),
            version: "1.2.3".to_owned(),
            commit: "abc".to_owned(),
            binary_sha256: "ff".to_owned(),
            policy: "unmanaged".to_owned(),
            event: AuditEvent::SessionStart {
                mode: "run".to_owned(),
                providers: Vec::new(),
                roles: Default::default(),
                flags: Default::default(),
            },
            prev: GENESIS.to_owned(),
        };
        let (first, first_hash) = seal(&start).expect("seal");
        let body = RecordBody {
            seq: 1,
            event: AuditEvent::PolicyRefusal {
                what: "shell `curl x`".to_owned(),
                key: "permissions.deny".to_owned(),
            },
            prev: first_hash,
            ..start
        };
        let (line, _) = seal(&body).expect("seal");
        let text = render(&format!("{first}\n{line}\n"));
        assert!(
            text.starts_with("session s1 by dev@box, anvil 1.2.3"),
            "{text}"
        );
        assert!(
            text.contains("policy refused shell `curl x` (permissions.deny)"),
            "{text}"
        );
        assert!(text.contains("chain intact: 2 records"), "{text}");
        let broken = render(&format!("{first}\n{}\n", line.replace("dev", "eve")));
        assert!(broken.contains("chain broken: line 2"), "{broken}");
        let headless = render(&format!("{line}\n"));
        assert!(headless.contains("chain broken: line 1"), "{headless}");
    }
}
