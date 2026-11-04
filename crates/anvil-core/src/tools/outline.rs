use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use super::glob::walk;
use super::{Access, Tool, ToolContext, ToolFuture, ToolOutput, parse_args, schema_for};
use crate::outline::{self, DEFAULT_DEPTH};

/// Arguments of the `outline` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct OutlineArgs {
    /// File or directory, relative to the project root or absolute.
    pub path: String,
    /// Nesting levels to show; 1 shows top-level items only. Default 2.
    #[serde(default)]
    pub depth: Option<usize>,
}

/// Shows the structure of a file or directory without the bodies.
pub struct Outline;

impl Tool for Outline {
    fn name(&self) -> &str {
        "outline"
    }

    fn description(&self) -> &str {
        "Shows a file's structure without its bodies: imports, then each function, method, \
         class, struct, enum, trait, impl, module, table or heading with its line range and \
         first doc line. On a directory, does the same for every source file, honoring \
         .gitignore. Use it to learn what a file or directory defines and where, at a \
         fraction of the cost of reading it."
    }

    fn schema(&self) -> Value {
        schema_for::<OutlineArgs>()
    }

    fn access(&self) -> Access {
        Access::Read
    }

    fn output_cap(&self) -> usize {
        // Above the outline's own cap, so its truncation note is never cut.
        outline::DIR_CAP + 1024
    }

    fn run<'a>(&'a self, args: Value, ctx: &'a ToolContext) -> ToolFuture<'a> {
        Box::pin(async move {
            let args: OutlineArgs = match parse_args(args) {
                Ok(args) => args,
                Err(output) => return output,
            };
            let depth = args.depth.unwrap_or(DEFAULT_DEPTH);
            let path = ctx.resolve(&args.path);
            let shown = ctx.display_path(&path);
            if path.is_dir() {
                let files: Vec<String> = walk(&path)
                    .filter(|entry| entry.file_type().is_some_and(|t| t.is_file()))
                    .filter(|entry| outline::is_source(entry.path()))
                    .filter_map(|entry| {
                        let text = std::fs::read_to_string(entry.path()).ok()?;
                        let name = ctx.display_path(entry.path());
                        Some(outline::render_file(&name, &text, depth))
                    })
                    .collect();
                return ToolOutput::ok(outline::render_dir(&files));
            }
            let bytes = match std::fs::read(&path) {
                Ok(bytes) => bytes,
                Err(error) => return ToolOutput::error(format!("cannot read {shown}: {error}")),
            };
            let Ok(text) = String::from_utf8(bytes) else {
                return ToolOutput::error(format!("{shown} is not valid UTF-8 text"));
            };
            ToolOutput::ok(outline::render_file(&shown, &text, depth))
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn schema_snapshot() {
        insta::assert_json_snapshot!(Outline.schema());
    }

    #[tokio::test]
    async fn outlines_a_file_and_reports_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("a.py"),
            "def f():\n    \"\"\"Does f.\"\"\"\n",
        )
        .expect("write");
        let ctx = ToolContext::new(dir.path());
        let output = Outline.run(json!({"path": "a.py"}), &ctx).await;
        assert!(!output.is_error);
        assert_eq!(output.content, "a.py (2 lines)\n1-2 def f():  // Does f.\n");

        let output = Outline.run(json!({"path": "missing.rs"}), &ctx).await;
        assert!(output.is_error);
        assert!(output.content.starts_with("cannot read missing.rs"));
    }

    #[tokio::test]
    async fn a_directory_groups_source_files_with_line_counts() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("src")).expect("mkdir");
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "pub fn a() {}\n\npub fn b() {}\n",
        )
        .expect("write");
        std::fs::write(dir.path().join("src/main.go"), "func main() {\n}\n").expect("write");
        std::fs::write(dir.path().join("src/notes.txt"), "fn not_source\n").expect("write");
        let ctx = ToolContext::new(dir.path());
        let output = Outline.run(json!({"path": "src"}), &ctx).await;
        assert_eq!(
            output.content,
            "src/lib.rs (3 lines)\n1-1 pub fn a() {}\n3-3 pub fn b() {}\n\
             src/main.go (2 lines)\n1-2 func main()\n"
        );
    }
}
