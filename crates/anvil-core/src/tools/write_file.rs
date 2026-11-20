use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use super::line_endings::{Ending, is_all_crlf, is_lf_only, with_ending};
use super::{
    Access, FileChange, Tool, ToolContext, ToolFuture, ToolOutput, parse_args, schema_for,
    sha256_hex,
};

/// Arguments of the `write_file` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct WriteFileArgs {
    /// Path of the file, relative to the project root or absolute.
    pub path: String,
    /// The complete new contents.
    pub content: String,
}

/// Creates or overwrites a file.
pub struct WriteFile;

/// The text `write_file` writes for `content`, given what the file holds.
///
/// A file that uses CRLF throughout stays CRLF when the new content uses LF
/// only, so a rewrite does not turn every line into a change. A new file, a
/// file with mixed or LF endings, and content with any CRLF are written as
/// given.
///
/// # Arguments
///
/// * `existing` - The file's current text, or `None` when it does not exist
///   or is not UTF-8 text.
/// * `content` - The content the model asked for.
///
/// # Returns
///
/// The text to write, and whether its line endings were changed.
pub(crate) fn content_to_write(existing: Option<&str>, content: &str) -> (String, bool) {
    match existing {
        Some(existing) if is_all_crlf(existing) && is_lf_only(content) => {
            (with_ending(content, Ending::CrLf), true)
        }
        _ => (content.to_owned(), false),
    }
}

impl Tool for WriteFile {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        "Writes the given content to a file, creating parent directories and replacing any \
         existing contents. An existing file that uses CRLF line endings throughout keeps \
         them. Prefer edit_file for small changes to existing files."
    }

    fn schema(&self) -> Value {
        schema_for::<WriteFileArgs>()
    }

    fn access(&self) -> Access {
        Access::Write
    }

    fn target_is_path(&self) -> bool {
        true
    }

    fn run<'a>(&'a self, args: Value, ctx: &'a ToolContext) -> ToolFuture<'a> {
        Box::pin(async move {
            let args: WriteFileArgs = match parse_args(args) {
                Ok(args) => args,
                Err(output) => return output,
            };
            let path = match ctx.checked_path(&args.path, true) {
                Ok(path) => path,
                Err(reason) => return ToolOutput::error(reason),
            };
            let shown = ctx.display_path(&path);
            if let Some(parent) = path.parent()
                && let Err(error) = std::fs::create_dir_all(parent)
            {
                return ToolOutput::error(format!("cannot create directory for {shown}: {error}"));
            }
            let existing = std::fs::read(&path).ok();
            let before = existing.as_deref().map(sha256_hex);
            let existing_text = existing
                .as_deref()
                .and_then(|bytes| std::str::from_utf8(bytes).ok());
            let (content, converted) = content_to_write(existing_text, &args.content);
            let note = if converted {
                " (as CRLF, the line ending the file already used)"
            } else {
                ""
            };
            match std::fs::write(&path, &content) {
                Ok(()) => ToolOutput::ok(format!("wrote {} bytes to {shown}{note}", content.len()))
                    .with_change(FileChange {
                        path: shown,
                        before,
                        after: sha256_hex(content.as_bytes()),
                    }),
                Err(error) => ToolOutput::error(format!("cannot write {shown}: {error}")),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn schema_snapshot() {
        insta::assert_json_snapshot!(WriteFile.schema());
    }

    #[tokio::test]
    async fn writes_and_creates_parents() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = ToolContext::new(dir.path());
        let output = WriteFile
            .run(json!({"path": "a/b/c.txt", "content": "hey"}), &ctx)
            .await;
        assert_eq!(output.content, "wrote 3 bytes to a/b/c.txt");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a/b/c.txt")).expect("read"),
            "hey"
        );
        let output = WriteFile
            .run(json!({"path": "a/b/c.txt", "content": ""}), &ctx)
            .await;
        assert!(!output.is_error);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a/b/c.txt")).expect("read"),
            ""
        );
    }

    #[tokio::test]
    async fn reports_write_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = ToolContext::new(dir.path());
        let output = WriteFile
            .run(json!({"path": ".", "content": "x"}), &ctx)
            .await;
        assert!(output.is_error);
        assert!(
            output.content.starts_with("cannot write"),
            "{}",
            output.content
        );
    }

    /// Writes `content` over a file holding `existing` (or a new file when it
    /// is `None`) and returns the output and the bytes on disk.
    async fn write_over(existing: Option<&str>, content: &str) -> (ToolOutput, Vec<u8>) {
        let dir = tempfile::tempdir().expect("tempdir");
        if let Some(existing) = existing {
            std::fs::write(dir.path().join("f.txt"), existing).expect("write");
        }
        let ctx = ToolContext::new(dir.path());
        let output = WriteFile
            .run(json!({"path": "f.txt", "content": content}), &ctx)
            .await;
        let bytes = std::fs::read(dir.path().join("f.txt")).expect("read");
        (output, bytes)
    }

    #[tokio::test]
    async fn lf_content_over_a_crlf_file_is_written_as_crlf() {
        let (output, bytes) = write_over(Some("a\r\nb\r\n"), "a\nB\nc\n").await;
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(bytes, b"a\r\nB\r\nc\r\n");
        assert!(output.content.contains("as CRLF"), "{}", output.content);
        assert_eq!(
            output.content.split(' ').nth(1),
            Some("9"),
            "{}",
            output.content
        );
    }

    #[tokio::test]
    async fn mixed_lf_and_new_files_are_written_as_given() {
        let (output, bytes) = write_over(Some("a\r\nb\n"), "x\ny\n").await;
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(bytes, b"x\ny\n", "a mixed file is written as given");
        let (_, bytes) = write_over(Some("a\nb\n"), "x\ny\n").await;
        assert_eq!(bytes, b"x\ny\n", "an LF file is written as given");
        let (_, bytes) = write_over(Some("a\r\nb\r\n"), "x\r\ny\n").await;
        assert_eq!(
            bytes, b"x\r\ny\n",
            "content with a CRLF is written as given"
        );
        let (output, bytes) = write_over(None, "x\ny\n").await;
        assert!(!output.content.contains("CRLF"), "{}", output.content);
        assert_eq!(bytes, b"x\ny\n", "a new file is written as given");
    }
}
