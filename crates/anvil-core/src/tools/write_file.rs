use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

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

impl Tool for WriteFile {
    fn name(&self) -> &str {
        "write_file"
    }

    fn description(&self) -> &str {
        "Writes the given content to a file, creating parent directories and replacing any \
         existing contents. Prefer edit_file for small changes to existing files."
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
            let before = std::fs::read(&path).ok().map(|old| sha256_hex(&old));
            match std::fs::write(&path, &args.content) {
                Ok(()) => ToolOutput::ok(format!("wrote {} bytes to {shown}", args.content.len()))
                    .with_change(FileChange {
                        path: shown,
                        before,
                        after: sha256_hex(args.content.as_bytes()),
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
}
