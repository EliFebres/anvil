//! The `edit_file` tool: exact-string replacement that refuses ambiguity.

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use super::{Access, Tool, ToolContext, ToolFuture, ToolOutput, parse_args, schema_for};

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

impl Tool for EditFile {
    fn name(&self) -> &'static str {
        "edit_file"
    }

    fn description(&self) -> &'static str {
        "Replaces an exact string in a file. Fails when the old string is missing or, unless \
         replace_all is true, when it occurs more than once; include more surrounding context \
         to make it unique."
    }

    fn schema(&self) -> Value {
        schema_for::<EditFileArgs>()
    }

    fn access(&self) -> Access {
        Access::Write
    }

    fn run<'a>(&'a self, args: Value, ctx: &'a ToolContext) -> ToolFuture<'a> {
        Box::pin(async move {
            let args: EditFileArgs = match parse_args(args) {
                Ok(args) => args,
                Err(output) => return output,
            };
            let path = ctx.resolve(&args.path);
            let shown = ctx.display_path(&path);
            if args.old.is_empty() {
                return ToolOutput::error("old must not be empty");
            }
            let text = match std::fs::read_to_string(&path) {
                Ok(text) => text,
                Err(error) => return ToolOutput::error(format!("cannot read {shown}: {error}")),
            };
            let count = text.matches(&args.old).count();
            if count == 0 {
                return ToolOutput::error(format!("old string not found in {shown}"));
            }
            if count > 1 && !args.replace_all {
                return ToolOutput::error(format!(
                    "old string occurs {count} times in {shown}; include more context to make it \
                     unique, or set replace_all"
                ));
            }
            let edited = if args.replace_all {
                text.replace(&args.old, &args.new)
            } else {
                text.replacen(&args.old, &args.new, 1)
            };
            match std::fs::write(&path, edited) {
                Ok(()) => ToolOutput::ok(format!("replaced {count} occurrence(s) in {shown}")),
                Err(error) => ToolOutput::error(format!("cannot write {shown}: {error}")),
            }
        })
    }
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
}
