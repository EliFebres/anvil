//! The `grep` tool: regex search over files, honoring `.gitignore`.

use std::path::Path;

use globset::{GlobBuilder, GlobMatcher};
use grep::regex::RegexMatcher;
use grep::searcher::sinks::UTF8;
use grep::searcher::{BinaryDetection, Searcher, SearcherBuilder};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use super::glob::{relative_slashes, walk};
use super::{Access, Tool, ToolContext, ToolFuture, ToolOutput, parse_args, schema_for};

/// Most matching lines returned before the search stops.
pub const MAX_MATCHES: usize = 500;

/// Arguments of the `grep` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct GrepArgs {
    /// Regular expression (Rust regex syntax) matched against each line.
    pub pattern: String,
    /// File or directory to search, relative to the project root or absolute. Default: the project root.
    #[serde(default)]
    pub path: Option<String>,
    /// Only search files matching this glob, such as `*.rs` (by file name) or `src/**/*.rs` (by path).
    #[serde(default)]
    pub glob: Option<String>,
}

/// Searches file contents with a regex.
pub struct Grep;

impl Tool for Grep {
    fn name(&self) -> &'static str {
        "grep"
    }

    fn description(&self) -> &'static str {
        "Searches file contents for a regular expression and returns matching lines as \
         `path:line:text`. Honors .gitignore, skips hidden and binary files. Optionally \
         restrict to a directory and a file glob."
    }

    fn schema(&self) -> Value {
        schema_for::<GrepArgs>()
    }

    fn access(&self) -> Access {
        Access::Read
    }

    fn run<'a>(&'a self, args: Value, ctx: &'a ToolContext) -> ToolFuture<'a> {
        Box::pin(async move {
            let args: GrepArgs = match parse_args(args) {
                Ok(args) => args,
                Err(output) => return output,
            };
            let matcher = match RegexMatcher::new_line_matcher(&args.pattern) {
                Ok(matcher) => matcher,
                Err(error) => {
                    return ToolOutput::error(format!("invalid regex `{}`: {error}", args.pattern));
                }
            };
            let filter = match args.glob.as_deref().map(FileFilter::new) {
                Some(Ok(filter)) => Some(filter),
                Some(Err(error)) => return ToolOutput::error(error),
                None => None,
            };
            let root = ctx.resolve(args.path.as_deref().unwrap_or("."));
            if !root.exists() {
                return ToolOutput::error(format!("{} does not exist", ctx.display_path(&root)));
            }
            let mut searcher = SearcherBuilder::new()
                .line_number(true)
                .binary_detection(BinaryDetection::quit(b'\x00'))
                .build();
            let mut lines = Vec::new();
            let mut cut = false;
            let files: Vec<std::path::PathBuf> = if root.is_file() {
                vec![root.clone()]
            } else {
                walk(&root)
                    .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
                    .map(|entry| entry.path().to_path_buf())
                    .filter(|path| {
                        filter
                            .as_ref()
                            .is_none_or(|filter| filter.accepts(path, &root))
                    })
                    .collect()
            };
            for file in files {
                let shown = ctx.display_path(&file);
                let hit_cap = search_file(&mut searcher, &matcher, &file, &shown, &mut lines);
                if hit_cap {
                    cut = true;
                    break;
                }
            }
            if lines.is_empty() {
                return ToolOutput::ok(format!("no matches for `{}`", args.pattern));
            }
            let mut text = lines.join("\n");
            if cut {
                text.push_str(&format!(
                    "\n[stopped after {MAX_MATCHES} matches; narrow the pattern or path]"
                ));
            }
            ToolOutput::ok(text)
        })
    }
}

/// Appends `shown:line:text` for each match; returns whether the cap was hit.
fn search_file(
    searcher: &mut Searcher,
    matcher: &RegexMatcher,
    file: &Path,
    shown: &str,
    lines: &mut Vec<String>,
) -> bool {
    let mut hit_cap = false;
    let sink = UTF8(|number, line| {
        lines.push(format!("{shown}:{number}:{}", line.trim_end()));
        hit_cap = lines.len() >= MAX_MATCHES;
        Ok(!hit_cap)
    });
    if let Err(error) = searcher.search_path(matcher, file, sink) {
        tracing::debug!(path = %file.display(), %error, "skipping unreadable file");
    }
    hit_cap
}

/// Restricts a search to files matching a glob.
struct FileFilter {
    /// The compiled glob.
    matcher: GlobMatcher,
    /// Whether the glob applies to the whole relative path or only the file name.
    by_path: bool,
}

impl FileFilter {
    /// Compiles the glob; one with a `/` matches the relative path, otherwise the file name.
    fn new(glob: &str) -> Result<Self, String> {
        let by_path = glob.contains('/');
        let matcher = GlobBuilder::new(glob)
            .literal_separator(by_path)
            .build()
            .map_err(|error| format!("invalid glob `{glob}`: {error}"))?
            .compile_matcher();
        Ok(Self { matcher, by_path })
    }

    /// Whether `path` passes the filter.
    fn accepts(&self, path: &Path, root: &Path) -> bool {
        if self.by_path {
            self.matcher.is_match(relative_slashes(path, root))
        } else {
            path.file_name()
                .is_some_and(|name| self.matcher.is_match(name))
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        for (path, content) in [
            ("src/main.rs", "fn main() {\n    todo!()\n}\n"),
            ("src/lib.rs", "pub fn todo_list() {}\n"),
            ("notes.md", "todo: write docs\n"),
            ("target/x.rs", "todo!()\n"),
            (".gitignore", "target/\n"),
        ] {
            let full = dir.path().join(path);
            std::fs::create_dir_all(full.parent().expect("parent")).expect("mkdir");
            std::fs::write(full, content).expect("write");
        }
        std::fs::write(dir.path().join("blob.bin"), b"todo\x00binary").expect("write");
        dir
    }

    #[test]
    fn schema_snapshot() {
        insta::assert_json_snapshot!(Grep.schema());
    }

    #[tokio::test]
    async fn finds_lines_across_files_skipping_ignored_and_binary() {
        let dir = project();
        let ctx = ToolContext::new(dir.path());
        let output = Grep.run(json!({"pattern": "todo"}), &ctx).await;
        assert_eq!(
            output.content,
            "notes.md:1:todo: write docs\nsrc/lib.rs:1:pub fn todo_list() {}\nsrc/main.rs:2:    todo!()"
        );
    }

    #[tokio::test]
    async fn glob_filters_by_name_or_path_and_path_scopes() {
        let dir = project();
        let ctx = ToolContext::new(dir.path());
        let output = Grep
            .run(json!({"pattern": "todo", "glob": "*.md"}), &ctx)
            .await;
        assert_eq!(output.content, "notes.md:1:todo: write docs");
        let output = Grep
            .run(json!({"pattern": "todo", "glob": "src/**/*.rs"}), &ctx)
            .await;
        assert!(output.content.contains("src/lib.rs"));
        assert!(!output.content.contains("notes.md"));
        let output = Grep
            .run(json!({"pattern": "todo!", "path": "src"}), &ctx)
            .await;
        assert_eq!(output.content, "src/main.rs:2:    todo!()");
        let output = Grep
            .run(json!({"pattern": "fn", "path": "src/main.rs"}), &ctx)
            .await;
        assert_eq!(output.content, "src/main.rs:1:fn main() {");
    }

    #[tokio::test]
    async fn no_matches_and_errors() {
        let dir = project();
        let ctx = ToolContext::new(dir.path());
        let output = Grep.run(json!({"pattern": "zzz"}), &ctx).await;
        assert_eq!(output.content, "no matches for `zzz`");
        let output = Grep.run(json!({"pattern": "("}), &ctx).await;
        assert!(output.is_error);
        let output = Grep.run(json!({"pattern": "x", "glob": "["}), &ctx).await;
        assert!(output.is_error);
        let output = Grep
            .run(json!({"pattern": "x", "path": "nope"}), &ctx)
            .await;
        assert!(output.content.contains("does not exist"));
    }
}
