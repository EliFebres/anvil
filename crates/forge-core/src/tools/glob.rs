//! The `glob` tool: finds files by pattern, honoring `.gitignore`.

use std::path::Path;

use globset::GlobBuilder;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use super::{Access, Tool, ToolContext, ToolFuture, ToolOutput, parse_args, schema_for};

/// Most paths returned before the list is cut.
pub const MAX_RESULTS: usize = 1000;

/// Arguments of the `glob` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GlobArgs {
    /// Glob such as `**/*.rs` or `src/*.toml`, matched against paths relative to the search root.
    pub pattern: String,
    /// Directory to search, relative to the project root or absolute. Default: the project root.
    #[serde(default)]
    pub path: Option<String>,
}

/// Finds files matching a glob.
pub struct Glob;

impl Tool for Glob {
    fn name(&self) -> &str {
        "glob"
    }

    fn description(&self) -> &str {
        "Finds files whose path relative to the search root matches a glob pattern. `*` does \
         not cross directory separators; use `**/` to recurse. Honors .gitignore and skips \
         hidden files. Returns paths relative to the project root, sorted."
    }

    fn schema(&self) -> Value {
        schema_for::<GlobArgs>()
    }

    fn access(&self) -> Access {
        Access::Read
    }

    fn run<'a>(&'a self, args: Value, ctx: &'a ToolContext) -> ToolFuture<'a> {
        Box::pin(async move {
            let args: GlobArgs = match parse_args(args) {
                Ok(args) => args,
                Err(output) => return output,
            };
            let matcher = match GlobBuilder::new(&args.pattern)
                .literal_separator(true)
                .build()
            {
                Ok(glob) => glob.compile_matcher(),
                Err(error) => {
                    return ToolOutput::error(format!("invalid glob `{}`: {error}", args.pattern));
                }
            };
            let root = ctx.resolve(args.path.as_deref().unwrap_or("."));
            if !root.is_dir() {
                return ToolOutput::error(format!(
                    "{} is not a directory",
                    ctx.display_path(&root)
                ));
            }
            let mut paths = Vec::new();
            for entry in walk(&root) {
                if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                    continue;
                }
                let relative = relative_slashes(entry.path(), &root);
                if matcher.is_match(&relative) {
                    paths.push(ctx.display_path(entry.path()));
                }
            }
            paths.sort();
            if paths.is_empty() {
                return ToolOutput::ok(format!("no files match `{}`", args.pattern));
            }
            let total = paths.len();
            paths.truncate(MAX_RESULTS);
            let mut text = paths.join("\n");
            if total > MAX_RESULTS {
                text.push_str(&format!(
                    "\n[{} more matches not shown; narrow the pattern]",
                    total - MAX_RESULTS
                ));
            }
            ToolOutput::ok(text)
        })
    }
}

/// Walks `root` respecting `.gitignore`, skipping hidden entries and the root itself.
pub(crate) fn walk(root: &Path) -> impl Iterator<Item = ignore::DirEntry> {
    ignore::WalkBuilder::new(root)
        .require_git(false)
        .sort_by_file_name(|a, b| a.cmp(b))
        .build()
        .filter_map(Result::ok)
        .filter(|entry| entry.depth() > 0)
}

/// The path relative to `root` with forward slashes.
pub(crate) fn relative_slashes(path: &Path, root: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        for (path, content) in [
            ("src/main.rs", "fn main() {}"),
            ("src/lib/mod.rs", ""),
            ("README.md", "# hi"),
            ("target/debug/out.rs", "ignored"),
            (".gitignore", "target/\n"),
            (".hidden/x.rs", ""),
        ] {
            let full = dir.path().join(path);
            std::fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
            std::fs::write(full, content).expect("write");
        }
        dir
    }

    #[test]
    fn schema_snapshot() {
        insta::assert_json_snapshot!(Glob.schema());
    }

    #[tokio::test]
    async fn recursive_glob_honors_gitignore_and_hidden() {
        let dir = project();
        let ctx = ToolContext::new(dir.path());
        let output = Glob.run(json!({"pattern": "**/*.rs"}), &ctx).await;
        assert_eq!(output.content, "src/lib/mod.rs\nsrc/main.rs");
    }

    #[tokio::test]
    async fn star_does_not_cross_directories_and_path_scopes() {
        let dir = project();
        let ctx = ToolContext::new(dir.path());
        let output = Glob.run(json!({"pattern": "*.rs"}), &ctx).await;
        assert_eq!(output.content, "no files match `*.rs`");
        let output = Glob
            .run(json!({"pattern": "*.rs", "path": "src"}), &ctx)
            .await;
        assert_eq!(output.content, "src/main.rs");
        let output = Glob.run(json!({"pattern": "*.md"}), &ctx).await;
        assert_eq!(output.content, "README.md");
    }

    #[tokio::test]
    async fn bad_pattern_and_bad_root() {
        let dir = project();
        let ctx = ToolContext::new(dir.path());
        let output = Glob.run(json!({"pattern": "[", "path": "src"}), &ctx).await;
        assert!(output.is_error);
        let output = Glob
            .run(json!({"pattern": "*", "path": "README.md"}), &ctx)
            .await;
        assert!(output.content.contains("not a directory"));
    }
}
