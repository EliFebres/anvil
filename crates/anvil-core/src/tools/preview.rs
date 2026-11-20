use serde_json::Value;

use crate::agent::LoopEvent;
use crate::provider::ToolCall;
use crate::tools::ToolContext;
use crate::tools::edit_file::apply;
use crate::tools::write_file::content_to_write;

/// Files larger than this are not previewed, to keep the event small.
const MAX_PREVIEW_BYTES: u64 = 1024 * 1024;

/// The edit an `edit_file` or `write_file` call will make.
///
/// The replacement follows the tool exactly, through the same functions:
/// `write_file` replaces the whole file and keeps a CRLF file CRLF, and
/// `edit_file` replaces the one occurrence of `old`, or every one with
/// `replace_all`, whatever the line endings. A call the tool would reject (bad arguments, a
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
    let path = ctx.checked_path(args.get("path")?.as_str()?, true).ok()?;
    match call.name.as_str() {
        "write_file" => {
            let content = args.get("content")?.as_str()?;
            let (old, exists) = match std::fs::metadata(&path) {
                Ok(meta) if meta.len() > MAX_PREVIEW_BYTES => return None,
                Ok(_) => (std::fs::read_to_string(&path).ok()?, true),
                Err(_) => (String::new(), false),
            };
            let (new, _) = content_to_write(exists.then_some(old.as_str()), content);
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
            let new = apply(&old, find, replace, replace_all).ok()?.text;
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
            &call("edit_file", r#"{"path":"f.txt","old":"a\nb","new":"z\nb"}"#),
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
        assert!(
            proposed_edit(
                &call("edit_file", r#"{"path":"f.txt","old":"a","new":"z"}"#),
                &ctx
            )
            .is_none(),
            "an ambiguous match the tool refuses has no preview"
        );
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

    #[test]
    fn previews_follow_the_tools_on_crlf_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("f.txt"), "a\r\nb\r\n").expect("write");
        let ctx = ToolContext::new(dir.path());
        let edit = proposed_edit(
            &call("edit_file", r#"{"path":"f.txt","old":"a\nb","new":"x\ny"}"#),
            &ctx,
        );
        assert!(
            matches!(&edit, Some(LoopEvent::ProposedEdit { new, .. }) if new == "x\r\ny\r\n"),
            "{edit:?}"
        );
        let write = proposed_edit(
            &call("write_file", r#"{"path":"f.txt","content":"x\ny\n"}"#),
            &ctx,
        );
        assert!(
            matches!(&write, Some(LoopEvent::ProposedEdit { new, .. }) if new == "x\r\ny\r\n"),
            "{write:?}"
        );
    }
}
