//! The `list_dir` tool.

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use super::{Access, Tool, ToolContext, ToolFuture, ToolOutput, parse_args, schema_for};

/// Arguments of the `list_dir` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListDirArgs {
    /// Directory to list, relative to the project root or absolute. Default: the project root.
    #[serde(default)]
    pub path: Option<String>,
}

/// Lists one directory, non-recursively.
pub struct ListDir;

impl Tool for ListDir {
    fn name(&self) -> &str {
        "list_dir"
    }

    fn description(&self) -> &str {
        "Lists the entries of one directory, sorted by name, with a trailing slash on \
         subdirectories. Not recursive; use glob to find files by pattern."
    }

    fn schema(&self) -> Value {
        schema_for::<ListDirArgs>()
    }

    fn access(&self) -> Access {
        Access::Read
    }

    fn run<'a>(&'a self, args: Value, ctx: &'a ToolContext) -> ToolFuture<'a> {
        Box::pin(async move {
            let args: ListDirArgs = match parse_args(args) {
                Ok(args) => args,
                Err(output) => return output,
            };
            let path = ctx.resolve(args.path.as_deref().unwrap_or("."));
            let shown = ctx.display_path(&path);
            let entries = match std::fs::read_dir(&path) {
                Ok(entries) => entries,
                Err(error) => return ToolOutput::error(format!("cannot list {shown}: {error}")),
            };
            let mut names: Vec<String> = entries
                .filter_map(Result::ok)
                .map(|entry| {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                        format!("{name}/")
                    } else {
                        name
                    }
                })
                .collect();
            names.sort();
            if names.is_empty() {
                return ToolOutput::ok(format!("{shown}: (empty directory)"));
            }
            ToolOutput::ok(names.join("\n"))
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn schema_snapshot() {
        insta::assert_json_snapshot!(ListDir.schema());
    }

    #[tokio::test]
    async fn lists_sorted_with_dir_markers() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("sub")).expect("mkdir");
        std::fs::write(dir.path().join("b.txt"), "").expect("write");
        std::fs::write(dir.path().join("a.txt"), "").expect("write");
        let ctx = ToolContext::new(dir.path());
        let output = ListDir.run(json!({}), &ctx).await;
        assert_eq!(output.content, "a.txt\nb.txt\nsub/");

        let output = ListDir.run(json!({"path": "sub"}), &ctx).await;
        assert_eq!(output.content, "sub: (empty directory)");

        let output = ListDir.run(json!({"path": "nope"}), &ctx).await;
        assert!(output.is_error);
    }
}
