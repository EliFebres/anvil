use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use super::{
    Access, DEFAULT_OUTPUT_CAP, Tool, ToolContext, ToolFuture, ToolOutput, parse_args, schema_for,
};

/// Lines returned when the model does not set a limit.
pub const DEFAULT_LIMIT: usize = 2000;

/// Most files one call may read.
pub const MAX_FILES: usize = 20;

/// Bytes kept back from each file's share of the output cap for its header
/// and continuation note.
const HEADER_ALLOWANCE: usize = 256;

/// Arguments of the `read_file` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadFileArgs {
    /// Path of one file, relative to the project root or absolute. Omit when
    /// passing `files`.
    #[serde(default)]
    pub path: Option<String>,
    /// First line to return, counting from 1. Default 1.
    #[serde(default)]
    pub offset: Option<usize>,
    /// Maximum number of lines to return. Default 2000.
    #[serde(default)]
    pub limit: Option<usize>,
    /// Several files or line ranges to read in one call, instead of `path`.
    #[serde(default)]
    pub files: Option<Vec<FileRange>>,
}

/// One file or line range in a multi-file read.
#[derive(Debug, Deserialize, JsonSchema)]
// Inlined: some OpenAI-compatible servers do not resolve `$ref`.
#[schemars(inline)]
pub struct FileRange {
    /// Path of the file, relative to the project root or absolute.
    pub path: String,
    /// First line to return, counting from 1. Default 1.
    #[serde(default)]
    pub offset: Option<usize>,
    /// Maximum number of lines to return. Default 2000.
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Reads one or several text files with line numbers.
pub struct ReadFile;

impl Tool for ReadFile {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Reads UTF-8 text files and returns their lines prefixed with 1-based line numbers. \
         Pass `path` (with optional offset and limit) for one file, or `files` with up to 20 \
         {path, offset?, limit?} entries to read several files or ranges in one call; each \
         then comes under a `==> path <==` header."
    }

    fn schema(&self) -> Value {
        schema_for::<ReadFileArgs>()
    }

    fn access(&self) -> Access {
        Access::Read
    }

    fn targets(&self, args: &Value) -> Vec<Option<String>> {
        let listed = args
            .get("files")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|file| file.get("path").and_then(Value::as_str).map(str::to_owned));
        let single = args.get("path").and_then(Value::as_str).map(str::to_owned);
        let targets: Vec<Option<String>> = single.into_iter().map(Some).chain(listed).collect();
        if targets.is_empty() {
            vec![None]
        } else {
            targets
        }
    }

    fn run<'a>(&'a self, args: Value, ctx: &'a ToolContext) -> ToolFuture<'a> {
        Box::pin(async move {
            let args: ReadFileArgs = match parse_args(args) {
                Ok(args) => args,
                Err(output) => return output,
            };
            match (args.path, args.files) {
                (Some(path), None) => {
                    let range = FileRange {
                        path,
                        offset: args.offset,
                        limit: args.limit,
                    };
                    match read_range(&range, ctx, usize::MAX) {
                        Ok(text) => ToolOutput::ok(text),
                        Err(error) => ToolOutput::error(error),
                    }
                }
                (None, Some(files)) => read_many(&files, ctx),
                (Some(_), Some(_)) => {
                    ToolOutput::error("pass either `path` or `files`, not both".to_owned())
                }
                (None, None) => ToolOutput::error(
                    "invalid arguments: pass `path` for one file or `files` for several".to_owned(),
                ),
            }
        })
    }
}

/// Reads every listed range, each under a header and within an equal share
/// of the output cap so one long file cannot push the others out.
fn read_many(files: &[FileRange], ctx: &ToolContext) -> ToolOutput {
    if files.is_empty() {
        return ToolOutput::error("`files` is empty; list at least one file".to_owned());
    }
    if files.len() > MAX_FILES {
        return ToolOutput::error(format!(
            "`files` lists {} entries; read at most {MAX_FILES} per call",
            files.len()
        ));
    }
    let share = (DEFAULT_OUTPUT_CAP / files.len()).saturating_sub(HEADER_ALLOWANCE);
    let mut out = String::new();
    let mut failures = 0;
    for range in files {
        let shown = ctx.display_path(&ctx.resolve(&range.path));
        out.push_str(&format!("==> {shown} <==\n"));
        match read_range(range, ctx, share) {
            Ok(text) => out.push_str(&text),
            Err(error) => {
                failures += 1;
                out.push_str(&error);
            }
        }
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    if failures == files.len() {
        ToolOutput::error(out)
    } else {
        ToolOutput::ok(out)
    }
}

/// Reads one range of one file, numbered, in at most `max_bytes`.
fn read_range(range: &FileRange, ctx: &ToolContext, max_bytes: usize) -> Result<String, String> {
    let path = ctx.resolve(&range.path);
    let shown = ctx.display_path(&path);
    let bytes = std::fs::read(&path).map_err(|error| format!("cannot read {shown}: {error}"))?;
    let text = String::from_utf8(bytes).map_err(|_| format!("{shown} is not valid UTF-8 text"))?;
    Ok(number_lines(
        &text,
        range.offset.unwrap_or(1).max(1),
        range.limit.unwrap_or(DEFAULT_LIMIT),
        max_bytes,
    ))
}

/// Renders lines `offset..offset+limit` (1-based) with line numbers, stopping
/// early once `max_bytes` would be exceeded.
fn number_lines(text: &str, offset: usize, limit: usize, max_bytes: usize) -> String {
    let total = text.lines().count();
    let mut out = String::new();
    let mut last_shown = offset - 1;
    for (index, line) in text.lines().enumerate().skip(offset - 1).take(limit) {
        let numbered = format!("{:>6}\t{line}\n", index + 1);
        if !out.is_empty() && out.len() + numbered.len() > max_bytes {
            break;
        }
        out.push_str(&numbered);
        last_shown = index + 1;
    }
    if out.is_empty() {
        if total == 0 {
            return "(empty file)".to_owned();
        }
        return format!("(no lines at offset {offset}; the file has {total} lines)");
    }
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
            number_lines("a\nb\nc", 1, 10, usize::MAX),
            "     1\ta\n     2\tb\n     3\tc\n"
        );
        assert_eq!(
            number_lines("a\nb\nc", 2, 1, usize::MAX),
            "     2\tb\n[1 more lines; continue with offset 3]\n"
        );
        assert_eq!(
            number_lines("a\nb", 5, 1, usize::MAX),
            "(no lines at offset 5; the file has 2 lines)"
        );
        assert_eq!(number_lines("", 1, 1, usize::MAX), "(empty file)");
        assert_eq!(
            number_lines("a\nb\nc", 1, 10, 10),
            "     1\ta\n[2 more lines; continue with offset 2]\n"
        );
    }

    #[tokio::test]
    async fn reads_several_files_and_ranges_in_one_call() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.txt"), "a1\na2\na3\n").expect("write");
        std::fs::write(dir.path().join("b.txt"), "b1\nb2\n").expect("write");
        let ctx = ToolContext::new(dir.path());
        let output = ReadFile
            .run(
                json!({"files": [
                    {"path": "a.txt", "offset": 2, "limit": 1},
                    {"path": "b.txt"},
                    {"path": "missing.txt"}
                ]}),
                &ctx,
            )
            .await;
        assert!(!output.is_error);
        assert!(output.content.starts_with(
            "==> a.txt <==\n     2\ta2\n[1 more lines; continue with offset 3]\n\
             ==> b.txt <==\n     1\tb1\n     2\tb2\n\
             ==> missing.txt <==\ncannot read missing.txt: "
        ));

        let both = ReadFile
            .run(json!({"path": "a.txt", "files": [{"path": "b.txt"}]}), &ctx)
            .await;
        assert!(both.is_error && both.content.contains("not both"));
        let empty = ReadFile.run(json!({"files": []}), &ctx).await;
        assert!(empty.is_error);
        let all_missing = ReadFile
            .run(json!({"files": [{"path": "nope"}]}), &ctx)
            .await;
        assert!(all_missing.is_error);
    }

    #[tokio::test]
    async fn each_file_gets_a_share_of_the_cap() {
        let dir = tempfile::tempdir().expect("tempdir");
        let long: String = (0..5000).map(|i| format!("line {i}\n")).collect();
        std::fs::write(dir.path().join("long.txt"), &long).expect("write");
        std::fs::write(dir.path().join("short.txt"), "s\n").expect("write");
        let ctx = ToolContext::new(dir.path());
        let output = ReadFile
            .run(
                json!({"files": [{"path": "long.txt"}, {"path": "short.txt"}]}),
                &ctx,
            )
            .await;
        assert!(output.content.len() <= DEFAULT_OUTPUT_CAP);
        assert!(output.content.contains("continue with offset"));
        assert!(output.content.ends_with("==> short.txt <==\n     1\ts\n"));
    }

    #[test]
    fn every_listed_path_is_a_permission_target() {
        let targets = ReadFile.targets(&json!({"files": [{"path": "a"}, {"path": ".env"}]}));
        assert_eq!(targets, [Some("a".to_owned()), Some(".env".to_owned())]);
        assert_eq!(
            ReadFile.targets(&json!({"path": "x"})),
            [Some("x".to_owned())]
        );
        assert_eq!(ReadFile.targets(&json!({})), [None]);
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
