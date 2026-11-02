//! What a file-writing tool call will do, computed before it runs so the
//! change can be shown and judged.

use serde_json::Value;

use crate::agent::LoopEvent;
use crate::provider::ToolCall;
use crate::tools::ToolContext;

/// Files larger than this are not previewed, to keep the event small.
const MAX_PREVIEW_BYTES: u64 = 1024 * 1024;

/// The edit an `edit_file` or `write_file` call will make.
///
/// The replacement follows the tool exactly: `write_file` replaces the
/// whole file, `edit_file` replaces the first occurrence of `old`, or every
/// one with `replace_all`. A call the tool would reject (bad arguments, a
/// missing file, no match) gives nothing, as does any other tool.
///
/// # Arguments
///
/// * `call` - The call about to run.
/// * `ctx` - The tool context, for resolving the path.
///
/// # Returns
///
/// The event to emit, if the call edits a file.
pub fn proposed_edit(call: &ToolCall, ctx: &ToolContext) -> Option<LoopEvent> {
    let args: Value = serde_json::from_str(&call.arguments_json).ok()?;
    let path = ctx.resolve(args.get("path")?.as_str()?);
    match call.name.as_str() {
        "write_file" => {
            let new = args.get("content")?.as_str()?.to_owned();
            let old = match std::fs::metadata(&path) {
                Ok(meta) if meta.len() > MAX_PREVIEW_BYTES => return None,
                Ok(_) => std::fs::read_to_string(&path).ok()?,
                Err(_) => String::new(),
            };
            Some(LoopEvent::ProposedEdit { path, old, new })
        }
        "edit_file" => {
            let find = args.get("old")?.as_str()?;
            let replace = args.get("new")?.as_str()?;
            let replace_all = args
                .get("replace_all")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if find.is_empty() || std::fs::metadata(&path).ok()?.len() > MAX_PREVIEW_BYTES {
                return None;
            }
            let old = std::fs::read_to_string(&path).ok()?;
            if !old.contains(find) {
                return None;
            }
            let new = if replace_all {
                old.replace(find, replace)
            } else {
                old.replacen(find, replace, 1)
            };
            Some(LoopEvent::ProposedEdit { path, old, new })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, args: &str) -> ToolCall {
        ToolCall {
            id: "c".to_owned(),
            name: name.to_owned(),
            arguments_json: args.to_owned(),
        }
    }

    #[test]
    fn edit_file_previews_the_replacement() {
        let dir = tempfile::tempdir().expect("dir");
        std::fs::write(dir.path().join("f.txt"), "a\nb\na\n").expect("write");
        let ctx = ToolContext::new(dir.path());
        let one = proposed_edit(
            &call("edit_file", r#"{"path":"f.txt","old":"a","new":"z"}"#),
            &ctx,
        );
        match one {
            Some(LoopEvent::ProposedEdit { path, old, new }) => {
                assert_eq!(path, dir.path().join("f.txt"));
                assert_eq!(old, "a\nb\na\n");
                assert_eq!(new, "z\nb\na\n");
            }
            other => panic!("unexpected: {other:?}"),
        }
        let all = proposed_edit(
            &call(
                "edit_file",
                r#"{"path":"f.txt","old":"a","new":"z","replace_all":true}"#,
            ),
            &ctx,
        );
        assert!(matches!(all, Some(LoopEvent::ProposedEdit { new, .. }) if new == "z\nb\nz\n"));
        assert!(
            proposed_edit(
                &call("edit_file", r#"{"path":"f.txt","old":"q","new":"z"}"#),
                &ctx
            )
            .is_none(),
            "no match, no preview"
        );
        assert!(
            proposed_edit(
                &call("edit_file", r#"{"path":"missing.txt","old":"a","new":"z"}"#),
                &ctx
            )
            .is_none()
        );
        assert!(proposed_edit(&call("edit_file", "not json"), &ctx).is_none());
        assert!(
            proposed_edit(&call("shell", r#"{"command":"ls"}"#), &ctx).is_none(),
            "other tools never preview"
        );
    }

    #[test]
    fn write_file_previews_against_the_current_or_empty_file() {
        let dir = tempfile::tempdir().expect("dir");
        let ctx = ToolContext::new(dir.path());
        let fresh = proposed_edit(
            &call("write_file", r#"{"path":"new.txt","content":"hi\n"}"#),
            &ctx,
        );
        assert!(
            matches!(fresh, Some(LoopEvent::ProposedEdit { old, new, .. }) if old.is_empty() && new == "hi\n")
        );
        std::fs::write(dir.path().join("new.txt"), "old\n").expect("write");
        let over = proposed_edit(
            &call("write_file", r#"{"path":"new.txt","content":"hi\n"}"#),
            &ctx,
        );
        assert!(matches!(over, Some(LoopEvent::ProposedEdit { old, .. }) if old == "old\n"));
    }
}
