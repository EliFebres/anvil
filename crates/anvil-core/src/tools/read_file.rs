//! The `read_file` tool.

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use super::{Access, Tool, ToolContext, ToolFuture, ToolOutput, parse_args, schema_for};

/// Lines returned when the model does not set a limit.
pub const DEFAULT_LIMIT: usize = 2000;

/// Arguments of the `read_file` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadFileArgs {
    /// Path of the file, relative to the project root or absolute.
    pub path: String,
    /// First line to return, counting from 1. Default 1.
    #[serde(default)]
    pub offset: Option<usize>,
    /// Maximum number of lines to return. Default 2000.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Reads a text file with line numbers.
pub struct ReadFile;

impl Tool for ReadFile {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Reads a UTF-8 text file and returns its lines prefixed with 1-based line numbers. \
         Use offset and limit to page through large files."
    }

    fn schema(&self) -> Value {
        schema_for::<ReadFileArgs>()
    }

    fn access(&self) -> Access {
        Access::Read
    }

    fn run<'a>(&'a self, args: Value, ctx: &'a ToolContext) -> ToolFuture<'a> {
        Box::pin(async move {
            let args: ReadFileArgs = match parse_args(args) {
                Ok(args) => args,
                Err(output) => return output,
            };
            let path = ctx.resolve(&args.path);
            let shown = ctx.display_path(&path);
            let bytes = match std::fs::read(&path) {
                Ok(bytes) => bytes,
                Err(error) => return ToolOutput::error(format!("cannot read {shown}: {error}")),
            };
            let Ok(text) = String::from_utf8(bytes) else {
                return ToolOutput::error(format!("{shown} is not valid UTF-8 text"));
            };
            ToolOutput::ok(number_lines(
                &text,
                args.offset.unwrap_or(1).max(1),
                args.limit.unwrap_or(DEFAULT_LIMIT),
            ))
        })
    }
}

/// Renders lines `offset..offset+limit` (1-based) with line numbers.
fn number_lines(text: &str, offset: usize, limit: usize) -> String {
    let total = text.lines().count();
    let mut out = String::new();
    for (index, line) in text.lines().enumerate().skip(offset - 1).take(limit) {
        out.push_str(&format!("{:>6}\t{line}\n", index + 1));
    }
    if out.is_empty() {
        if total == 0 {
            return "(empty file)".to_owned();
        }
        return format!("(no lines at offset {offset}; the file has {total} lines)");
    }
    let last_shown = (offset - 1 + limit).min(total);
    if last_shown < total {
        out.push_str(&format!(
            "[{} more lines; continue with offset {}]\n",
            total - last_shown,
            last_shown + 1
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn schema_snapshot() {
        insta::assert_json_snapshot!(ReadFile.schema());
    }

    #[test]
    fn numbering_and_paging() {
        assert_eq!(
            number_lines("a\nb\nc", 1, 10),
            "     1\ta\n     2\tb\n     3\tc\n"
        );
        assert_eq!(
            number_lines("a\nb\nc", 2, 1),
            "     2\tb\n[1 more lines; continue with offset 3]\n"
        );
        assert_eq!(
            number_lines("a\nb", 5, 1),
            "(no lines at offset 5; the file has 2 lines)"
        );
        assert_eq!(number_lines("", 1, 1), "(empty file)");
    }

    #[tokio::test]
    async fn reads_relative_to_cwd_and_reports_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("x.txt"), "one\ntwo\n").expect("write");
        let ctx = ToolContext::new(dir.path());
        let output = ReadFile.run(json!({"path": "x.txt"}), &ctx).await;
        assert_eq!(output.content, "     1\tone\n     2\ttwo\n");
        assert!(!output.is_error);

        let output = ReadFile.run(json!({"path": "missing.txt"}), &ctx).await;
        assert!(output.is_error);
        assert!(output.content.starts_with("cannot read missing.txt"));

        std::fs::write(dir.path().join("bin"), [0xff, 0xfe]).expect("write");
        let output = ReadFile.run(json!({"path": "bin"}), &ctx).await;
        assert!(output.content.contains("not valid UTF-8"));

        let output = ReadFile.run(json!({"offset": 1}), &ctx).await;
        assert!(output.content.starts_with("invalid arguments"));
    }
}
