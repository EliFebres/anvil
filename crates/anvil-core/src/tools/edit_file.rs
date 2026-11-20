use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use super::line_endings::{BOM, Normalized, ending_for_span, to_lf, with_ending};
use super::{
    Access, FileChange, Tool, ToolContext, ToolFuture, ToolOutput, edit_hint, parse_args,
    schema_for, sha256_hex,
};

/// The name the model calls this tool by.
const EDIT_FILE_TOOL: &str = "edit_file";

/// Arguments of the `edit_file` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct EditFileArgs {
    /// Path of the file, relative to the project root or absolute.
    pub path: String,
    /// Exact text to replace. Must occur exactly once unless replace_all is set.
    pub old: String,
    /// Replacement text.
    pub new: String,
    /// Replace every occurrence instead of requiring a unique match.
    #[serde(default)]
    pub replace_all: bool,
}

/// Replaces an exact string in a file.
pub struct EditFile;

/// An edit that applies: the new file text and how many spans it replaced.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Applied {
    /// The whole file after the edit.
    pub(crate) text: String,
    /// How many spans were replaced.
    pub(crate) count: usize,
}

/// Why an edit does not apply.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// `old` is empty, or only a byte order mark.
    EmptyOld,
    /// `old` is not in the file.
    NotFound,
    /// `old` occurs this many times and `replace_all` is not set.
    Ambiguous(usize),
}

/// Applies an edit to a file's text exactly as the `edit_file` tool does.
///
/// `old` matches regardless of line endings: LF in `old` matches CRLF in the
/// file and the other way round. Only the matched spans change; each
/// replacement takes the line ending of the first line it replaces. A byte
/// order mark at the start of the file is kept. When no span matches that
/// way, an exact byte match is tried, so text that splits a CRLF pair on
/// purpose still applies, with `new` inserted as given.
///
/// # Arguments
///
/// * `text` - The file's current text.
/// * `old` - The text to replace.
/// * `new` - The replacement.
/// * `replace_all` - Replace every match instead of requiring one.
///
/// # Returns
///
/// The edited text and the number of replacements.
///
/// # Errors
///
/// [`Refusal`] when `old` is empty, missing, or not unique without
/// `replace_all`.
pub(crate) fn apply(
    text: &str,
    old: &str,
    new: &str,
    replace_all: bool,
) -> Result<Applied, Refusal> {
    let (bom, body) = match text.strip_prefix(BOM) {
        Some(body) => (true, body),
        None => (false, text),
    };
    let (old, new) = match old.strip_prefix(BOM) {
        Some(old) => (old, new.strip_prefix(BOM).unwrap_or(new)),
        None => (old, new),
    };
    if old.is_empty() {
        return Err(Refusal::EmptyOld);
    }
    let normalized = Normalized::new(body);
    let old_lf = to_lf(old);
    let mut spans: Vec<(usize, usize)> = normalized
        .text
        .match_indices(&old_lf)
        .map(|(at, _)| {
            (
                normalized.to_original(at),
                normalized.to_original(at + old_lf.len()),
            )
        })
        .collect();
    let adapt_endings = !spans.is_empty();
    if spans.is_empty() {
        spans = body
            .match_indices(old)
            .map(|(at, found)| (at, at + found.len()))
            .collect();
    }
    let count = spans.len();
    if count == 0 {
        return Err(Refusal::NotFound);
    }
    if count > 1 && !replace_all {
        return Err(Refusal::Ambiguous(count));
    }
    let used = if replace_all { count } else { 1 };
    let mut out = String::with_capacity(text.len() + new.len() * used);
    if bom {
        out.push(BOM);
    }
    let mut last = 0;
    for &(start, end) in spans.iter().take(used) {
        out.push_str(&body[last..start]);
        if adapt_endings {
            out.push_str(&with_ending(new, ending_for_span(body, start)));
        } else {
            out.push_str(new);
        }
        last = end;
    }
    out.push_str(&body[last..]);
    Ok(Applied {
        text: out,
        count: used,
    })
}

impl Tool for EditFile {
    fn name(&self) -> &str {
        EDIT_FILE_TOOL
    }

    fn description(&self) -> &str {
        "Replaces an exact string in a file. Line endings do not need to match (LF and CRLF \
         are treated alike and the file keeps its own); spaces, tabs and indentation must. \
         Fails when the old string is missing or, unless replace_all is true, when it occurs \
         more than once; include more surrounding context to make it unique."
    }

    fn schema(&self) -> Value {
        schema_for::<EditFileArgs>()
    }

    fn access(&self) -> Access {
        Access::Write
    }

    fn target_is_path(&self) -> bool {
        true
    }

    fn run<'a>(&'a self, args: Value, ctx: &'a ToolContext) -> ToolFuture<'a> {
        Box::pin(async move {
            let args: EditFileArgs = match parse_args(args) {
                Ok(args) => args,
                Err(output) => return output,
            };
            let path = match ctx.checked_path(&args.path, true) {
                Ok(path) => path,
                Err(reason) => return ToolOutput::error(reason),
            };
            let shown = ctx.display_path(&path);
            if args.old.is_empty() {
                return ToolOutput::error(EMPTY_OLD);
            }
            let text = match std::fs::read_to_string(&path) {
                Ok(text) => text,
                Err(error) => return ToolOutput::error(format!("cannot read {shown}: {error}")),
            };
            let applied = match apply(&text, &args.old, &args.new, args.replace_all) {
                Ok(applied) => applied,
                Err(Refusal::EmptyOld) => return ToolOutput::error(EMPTY_OLD),
                Err(Refusal::Ambiguous(count)) => {
                    return ToolOutput::error(format!(
                        "old string occurs {count} times in {shown}; include more context to \
                         make it unique, or set replace_all"
                    ));
                }
                Err(Refusal::NotFound) => return not_found(ctx, &shown, &text, &args.old),
            };
            let change = FileChange {
                path: shown.clone(),
                before: Some(sha256_hex(text.as_bytes())),
                after: sha256_hex(applied.text.as_bytes()),
            };
            match std::fs::write(&path, applied.text) {
                Ok(()) => ToolOutput::ok(format!(
                    "replaced {} occurrence(s) in {shown}",
                    applied.count
                ))
                .with_change(change),
                Err(error) => ToolOutput::error(format!("cannot write {shown}: {error}")),
            }
        })
    }
}

/// The error for an empty `old` argument.
const EMPTY_OLD: &str =
    "argument `old` must not be empty: pass the exact text to replace, as read_file shows it";

/// The not-found error, built from the file redacted whole first, so a key
/// block is redacted whichever of its lines the message shows.
fn not_found(ctx: &ToolContext, shown: &str, text: &str, old: &str) -> ToolOutput {
    let body = text.strip_prefix(BOM).unwrap_or(text);
    let old = old.strip_prefix(BOM).unwrap_or(old);
    let redacted = ctx.guard.redact_lines(&to_lf(body));
    ctx.guard.record(EDIT_FILE_TOOL, &redacted.counts);
    ToolOutput::error(edit_hint::not_found(shown, &redacted.text, &to_lf(old)))
        .with_redactions(redacted.counts)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn setup(content: &str) -> (tempfile::TempDir, ToolContext) {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("f.txt"), content).expect("write");
        let ctx = ToolContext::new(dir.path());
        (dir, ctx)
    }

    #[test]
    fn schema_snapshot() {
        insta::assert_json_snapshot!(EditFile.schema());
    }

    #[tokio::test]
    async fn replaces_a_unique_match() {
        let (dir, ctx) = setup("fn a() {}\nfn b() {}\n");
        let output = EditFile
            .run(
                json!({"path": "f.txt", "old": "fn a()", "new": "fn alpha()"}),
                &ctx,
            )
            .await;
        assert_eq!(output.content, "replaced 1 occurrence(s) in f.txt");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).expect("read"),
            "fn alpha() {}\nfn b() {}\n"
        );
    }

    #[tokio::test]
    async fn refuses_ambiguous_and_missing_matches() {
        let (dir, ctx) = setup("x = 1\nx = 1\n");
        let output = EditFile
            .run(
                json!({"path": "f.txt", "old": "x = 1", "new": "x = 2"}),
                &ctx,
            )
            .await;
        assert!(output.is_error);
        assert!(
            output.content.contains("occurs 2 times"),
            "{}",
            output.content
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).expect("read"),
            "x = 1\nx = 1\n",
            "file untouched"
        );

        let output = EditFile
            .run(json!({"path": "f.txt", "old": "nope", "new": "x"}), &ctx)
            .await;
        assert!(output.content.contains("not found"));

        let output = EditFile
            .run(json!({"path": "f.txt", "old": "", "new": "x"}), &ctx)
            .await;
        assert!(output.content.contains("must not be empty"));
    }

    #[tokio::test]
    async fn replace_all_replaces_every_occurrence() {
        let (dir, ctx) = setup("x = 1\nx = 1\n");
        let output = EditFile
            .run(
                json!({"path": "f.txt", "old": "x = 1", "new": "y = 2", "replace_all": true}),
                &ctx,
            )
            .await;
        assert_eq!(output.content, "replaced 2 occurrence(s) in f.txt");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).expect("read"),
            "y = 2\ny = 2\n"
        );
    }

    /// Runs one edit on a file holding `content` and returns the output and
    /// the file's bytes afterwards.
    async fn edit_bytes(content: &str, args: Value) -> (ToolOutput, Vec<u8>) {
        let (dir, ctx) = setup(content);
        let mut args = args;
        args["path"] = json!("f.txt");
        let output = EditFile.run(args, &ctx).await;
        let bytes = std::fs::read(dir.path().join("f.txt")).expect("read");
        (output, bytes)
    }

    #[tokio::test]
    async fn an_lf_old_text_edits_a_crlf_file_and_every_line_stays_crlf() {
        let (output, bytes) = edit_bytes(
            "a\r\nb\r\nc\r\n",
            json!({"old": "a\nb\n", "new": "x\ny\nz\n"}),
        )
        .await;
        assert_eq!(output.content, "replaced 1 occurrence(s) in f.txt");
        assert_eq!(bytes, b"x\r\ny\r\nz\r\nc\r\n");
    }

    #[tokio::test]
    async fn a_crlf_old_text_edits_an_lf_file_and_the_file_stays_lf() {
        let (output, bytes) =
            edit_bytes("a\nb\nc\n", json!({"old": "a\r\nb", "new": "x\r\ny"})).await;
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(bytes, b"x\ny\nc\n");
    }

    #[tokio::test]
    async fn in_a_mixed_file_only_the_replaced_lines_change() {
        let (output, bytes) = edit_bytes(
            "l1\nl2\r\nl3\r\nl4\n",
            json!({"old": "l2\nl3", "new": "m2\nm3\nm3b"}),
        )
        .await;
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(bytes, b"l1\nm2\r\nm3\r\nm3b\r\nl4\n");

        let (output, bytes) =
            edit_bytes("a\r\nb\nc\r\n", json!({"old": "b\n", "new": "B1\r\nB2\n"})).await;
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(bytes, b"a\r\nB1\nB2\nc\r\n");
    }

    #[tokio::test]
    async fn a_byte_order_mark_is_kept() {
        let (output, bytes) =
            edit_bytes("\u{feff}a\r\nb\r\n", json!({"old": "a\nb", "new": "c\nd"})).await;
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(bytes, b"\xef\xbb\xbfc\r\nd\r\n");

        let (output, bytes) = edit_bytes(
            "\u{feff}a\r\nb\r\n",
            json!({"old": "\u{feff}a", "new": "z"}),
        )
        .await;
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(bytes, b"\xef\xbb\xbfz\r\nb\r\n");
    }

    #[tokio::test]
    async fn replace_all_works_across_crlf_lines() {
        let (output, bytes) = edit_bytes(
            "x = 1\r\ny\r\nq\r\nx = 1\r\ny\r\n",
            json!({"old": "x = 1\ny", "new": "x = 2\ny\nz", "replace_all": true}),
        )
        .await;
        assert_eq!(output.content, "replaced 2 occurrence(s) in f.txt");
        assert_eq!(bytes, b"x = 2\r\ny\r\nz\r\nq\r\nx = 2\r\ny\r\nz\r\n");
    }

    #[tokio::test]
    async fn a_match_that_is_ambiguous_across_line_endings_is_refused() {
        let (output, bytes) =
            edit_bytes("a\r\nb\r\na\nb\n", json!({"old": "a\nb", "new": "z"})).await;
        assert!(output.is_error);
        assert!(
            output.content.contains("occurs 2 times"),
            "{}",
            output.content
        );
        assert_eq!(bytes, b"a\r\nb\r\na\nb\n", "file untouched");
    }

    #[tokio::test]
    async fn edge_cases_of_line_endings() {
        // A lone carriage return in old still matches exactly.
        let (output, bytes) = edit_bytes("a\r\nb", json!({"old": "a\r", "new": "z"})).await;
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(bytes, b"z\nb");
        // A file without a final line break.
        let (output, bytes) = edit_bytes("a\r\nb", json!({"old": "a\nb", "new": "c\nd"})).await;
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(bytes, b"c\r\nd");
        // An old text that ends mid-line, and an empty new text.
        let (output, bytes) = edit_bytes(
            "one two\r\nthree\r\n",
            json!({"old": "two\nthr", "new": ""}),
        )
        .await;
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(bytes, b"one ee\r\n");
        // An old text that starts with the line break.
        let (output, bytes) =
            edit_bytes("a\r\nb\r\n", json!({"old": "\nb", "new": "\nc\nd"})).await;
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(bytes, b"a\r\nc\r\nd\r\n");
        // Lone carriage returns in the file are ordinary text and stay.
        let (output, bytes) =
            edit_bytes("a\rb\r\nc\r\n", json!({"old": "b\nc", "new": "B\nC"})).await;
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(bytes, b"a\rB\r\nC\r\n");
        // A very long line.
        let long = "x".repeat(100_000);
        let (output, bytes) = edit_bytes(
            &format!("{long}\r\nend\r\n"),
            json!({"old": format!("{long}\nend"), "new": "short\nend"}),
        )
        .await;
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(bytes, b"short\r\nend\r\n");
    }

    #[tokio::test]
    async fn not_found_names_a_white_space_difference_and_shows_it() {
        let file = "fn f() {\r\n    let x = 1;\r\n}\r\n";
        let (output, bytes) = edit_bytes(
            file,
            json!({"old": "fn f() {\n  let x = 1;\n}", "new": "z"}),
        )
        .await;
        assert!(output.is_error);
        let message = &output.content;
        assert!(
            message.contains("matches at line 1 when leading and trailing white space is ignored"),
            "{message}"
        );
        assert!(message.contains("At line 2:"), "{message}");
        assert!(message.contains("file: |····let·x·=·1;|"), "{message}");
        assert!(message.contains("old:  |··let·x·=·1;|"), "{message}");
        assert_eq!(bytes, file.as_bytes(), "file untouched");
    }

    #[tokio::test]
    async fn not_found_shows_only_the_region_where_the_first_line_occurs() {
        let mut file = String::from("fn f() {\r\n    let x = 1;\r\n}\r\n");
        for i in 0..40 {
            file.push_str(&format!("line {i}\r\n"));
        }
        let (output, _) = edit_bytes(
            &file,
            json!({"old": "fn f() {\n    let y = 2;\n}\n", "new": "z"}),
        )
        .await;
        assert!(output.is_error);
        let message = &output.content;
        assert!(message.contains("`fn f() {` occurs at line 1"), "{message}");
        assert!(
            message.ends_with("     1\tfn f() {\n     2\t    let x = 1;\n     3\t}\n"),
            "{message}"
        );
        assert!(!message.contains("line 0"), "only the region: {message}");
    }

    #[tokio::test]
    async fn not_found_says_plainly_when_nothing_is_similar() {
        let (output, _) = edit_bytes("a\nb\n", json!({"old": "zzz\nyyy", "new": "q"})).await;
        assert!(output.is_error);
        assert!(
            output
                .content
                .contains("no line of the file contains `zzz`, the first line of the old text"),
            "{}",
            output.content
        );
        assert!(output.content.contains("read it again with read_file"));
    }

    #[tokio::test]
    async fn not_found_stays_within_its_size_limit() {
        let wide = "w".repeat(300);
        let file: String = (0..200).map(|i| format!("{i} {wide}\r\n")).collect();
        let old: String = std::iter::once(format!("0 {wide}\n"))
            .chain((0..100).map(|i| format!("different {i}\n")))
            .collect();
        let (output, _) = edit_bytes(&file, json!({"old": old, "new": "q"})).await;
        assert!(output.is_error);
        let message = &output.content;
        assert!(message.len() <= edit_hint::MAX_BYTES, "{}", message.len());
        assert!(message.contains("Showing 20 of the 101 lines"), "{message}");
        let shown = message.lines().filter(|line| line.contains('\t')).count();
        assert!(shown <= edit_hint::MAX_LINES, "{shown} lines shown");
        assert!(!message.contains(&wide), "a long line is cut");
        assert!(!message.contains("\t20 "), "nothing past the region");
    }

    #[tokio::test]
    async fn the_not_found_message_redacts_a_key_block_whichever_lines_it_shows() {
        let (dir, ctx) = setup("");
        std::fs::write(
            dir.path().join("deploy_key.txt"),
            crate::tools::read_file::tests::key_file(),
        )
        .expect("write");
        let output = EditFile
            .run(
                json!({
                    "path": "deploy_key.txt",
                    "old": "first\nnot the body\nnor this\nnor that",
                    "new": "x"
                }),
                &ctx,
            )
            .await;
        assert!(output.is_error);
        assert!(
            output.content.contains("occurs at line 1"),
            "{}",
            output.content
        );
        assert!(!output.content.contains("TUlJ"), "{}", output.content);
        assert!(!output.content.contains("BEGIN"), "{}", output.content);
        assert_eq!(output.redactions.get("private_key"), Some(&1));
    }

    #[tokio::test]
    async fn argument_errors_name_the_argument_and_what_it_accepts() {
        let (_dir, ctx) = setup("a\n");
        let output = EditFile
            .run(json!({"path": "f.txt", "new": "x"}), &ctx)
            .await;
        assert!(output.is_error);
        assert!(
            output
                .content
                .contains("missing argument `old`: a string is required"),
            "{}",
            output.content
        );
        let output = EditFile
            .run(
                json!({"path": "f.txt", "old": "a", "new": "b", "replace_all": "yes"}),
                &ctx,
            )
            .await;
        assert!(
            output
                .content
                .contains("argument `replace_all` is the string \"yes\"; it must be true or false"),
            "{}",
            output.content
        );
    }
}
