use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use globset::{GlobBuilder, GlobMatcher};
use grep::regex::RegexMatcher;
use grep::searcher::sinks::UTF8;
use grep::searcher::{BinaryDetection, Searcher, SearcherBuilder};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use super::glob::{relative_slashes, walk};
use super::{
    Access, DEFAULT_OUTPUT_CAP, Tool, ToolContext, ToolFuture, ToolOutput, parse_args, schema_for,
};
use crate::outline;

/// Most matching lines returned before the search stops.
pub const MAX_MATCHES: usize = 500;

/// Most matches shown with context and their enclosing item; more come back
/// as bare matching lines.
pub const CONTEXT_MATCHES: usize = 50;

/// Lines of context shown before and after each match.
pub const CONTEXT_LINES: usize = 3;

/// One matching line.
struct Hit {
    /// The file it is in.
    file: PathBuf,
    /// The file as the model sees it.
    shown: String,
    /// 1-based line number.
    line: usize,
    /// The line, trailing whitespace trimmed.
    text: String,
}

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
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Searches file contents for a regular expression. Up to 50 matches come back as \
         `path:line:text` with 3 lines of context around each (`path-line-text`, groups \
         separated by `--`) and, above each group, the enclosing function or item with its \
         line range; more matches come back as bare `path:line:text` lines. Honors \
         .gitignore, skips hidden and binary files. Optionally restrict to a directory and a \
         file glob."
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
            let mut hits = Vec::new();
            let mut cut = false;
            let files: Vec<PathBuf> = if root.is_file() {
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
                if search_file(&mut searcher, &matcher, file, shown, &mut hits) {
                    cut = true;
                    break;
                }
            }
            if hits.is_empty() {
                return ToolOutput::ok(format!("no matches for `{}`", args.pattern));
            }
            let mut text = with_context(&hits)
                .filter(|text| text.len() <= DEFAULT_OUTPUT_CAP)
                .unwrap_or_else(|| bare(&hits));
            if cut {
                text.push_str(&format!(
                    "\n[stopped after {MAX_MATCHES} matches; narrow the pattern or path]"
                ));
            }
            ToolOutput::ok(text)
        })
    }
}

/// Collects each match in `file`; returns whether the cap was hit.
fn search_file(
    searcher: &mut Searcher,
    matcher: &RegexMatcher,
    file: PathBuf,
    shown: String,
    hits: &mut Vec<Hit>,
) -> bool {
    let mut hit_cap = false;
    let mut found = Vec::new();
    let sink = UTF8(|number, line| {
        found.push((number as usize, line.trim_end().to_owned()));
        hit_cap = hits.len() + found.len() >= MAX_MATCHES;
        Ok(!hit_cap)
    });
    if let Err(error) = searcher.search_path(matcher, &file, sink) {
        tracing::debug!(path = %file.display(), %error, "skipping unreadable file");
    }
    hits.extend(found.into_iter().map(|(line, text)| Hit {
        file: file.clone(),
        shown: shown.clone(),
        line,
        text,
    }));
    hit_cap
}

/// `path:line:text` per match.
fn bare(hits: &[Hit]) -> String {
    hits.iter()
        .map(|hit| format!("{}:{}:{}", hit.shown, hit.line, hit.text))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Every match with [`CONTEXT_LINES`] around it, nearby matches merged into
/// one group, each group headed by its enclosing item; `None` above
/// [`CONTEXT_MATCHES`] matches.
fn with_context(hits: &[Hit]) -> Option<String> {
    if hits.len() > CONTEXT_MATCHES {
        return None;
    }
    let mut by_file: BTreeMap<&Path, Vec<&Hit>> = BTreeMap::new();
    for hit in hits {
        by_file.entry(&hit.file).or_default().push(hit);
    }
    let mut groups = Vec::new();
    // Files keep the order the walk found them in.
    let mut order: Vec<&Path> = Vec::new();
    for hit in hits {
        if !order.contains(&hit.file.as_path()) {
            order.push(&hit.file);
        }
    }
    for file in order {
        let file_hits = &by_file[file];
        let Ok(text) = std::fs::read_to_string(file) else {
            groups.extend(file_hits.iter().map(|hit| bare(std::slice::from_ref(*hit))));
            continue;
        };
        let lines: Vec<&str> = text.lines().collect();
        let items = if outline::is_source(file) {
            outline::outline_source(file, &text, usize::MAX)
        } else {
            Vec::new()
        };
        for run in file_hits.chunk_by(|a, b| b.line <= a.line + 2 * CONTEXT_LINES + 1) {
            groups.push(render_group(run, &lines, &items));
        }
    }
    Some(groups.join("\n--\n"))
}

/// One group of nearby matches in one file, with context and its header.
fn render_group(run: &[&Hit], lines: &[&str], items: &[outline::Item]) -> String {
    debug_assert!(!run.is_empty(), "a group has at least one match");
    let shown = &run[0].shown;
    let first = run[0].line.saturating_sub(CONTEXT_LINES).max(1);
    let last = (run[run.len() - 1].line + CONTEXT_LINES).min(lines.len());
    let mut out = Vec::new();
    // A header for an item the group already shows whole would repeat it.
    if let Some(item) = outline::enclosing(items, run[0].line)
        .filter(|item| !item.signature.is_empty() && (item.start < first || item.end > last))
    {
        out.push(format!(
            "{shown}:{}-{}: {}",
            item.start, item.end, item.signature
        ));
    }
    for number in first..=last {
        let text = lines.get(number - 1).map_or("", |line| line.trim_end());
        let separator = if run.iter().any(|hit| hit.line == number) {
            ':'
        } else {
            '-'
        };
        out.push(format!("{shown}{separator}{number}{separator}{text}"));
    }
    out.join("\n")
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
            "notes.md:1:todo: write docs\n--\n\
             src/lib.rs:1:pub fn todo_list() {}\n--\n\
             src/main.rs-1-fn main() {\nsrc/main.rs:2:    todo!()\nsrc/main.rs-3-}"
        );
    }

    #[tokio::test]
    async fn nearby_matches_share_a_group_and_distant_ones_split() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut source = String::from("fn first() {\n    hit();\n    hit();\n\n\n\n\n}\n");
        source.push_str(&"\n".repeat(20));
        source.push_str("fn second() {\n\n\n\n    hit();\n}\n");
        std::fs::write(dir.path().join("a.rs"), source).expect("write");
        let ctx = ToolContext::new(dir.path());
        let output = Grep.run(json!({"pattern": "hit\\("}), &ctx).await;
        let groups: Vec<&str> = output.content.split("\n--\n").collect();
        assert_eq!(groups.len(), 2, "{}", output.content);
        assert!(groups[0].starts_with("a.rs:1-8: fn first()\na.rs-1-fn first() {\na.rs:2:"));
        assert!(groups[0].contains("a.rs:3:    hit();"));
        assert!(groups[1].starts_with("a.rs:29-34: fn second()\na.rs-30-\n"));
    }

    #[tokio::test]
    async fn many_matches_come_back_bare() {
        let dir = tempfile::tempdir().expect("tempdir");
        let body: String = (0..=CONTEXT_MATCHES).map(|i| format!("x{i}\n")).collect();
        std::fs::write(dir.path().join("many.txt"), body).expect("write");
        let ctx = ToolContext::new(dir.path());
        let output = Grep.run(json!({"pattern": "^x"}), &ctx).await;
        assert!(output.content.starts_with("many.txt:1:x0\nmany.txt:2:x1\n"));
        assert!(!output.content.contains("--"));
    }

    #[tokio::test]
    async fn glob_filters_by_name_or_path_and_path_scopes() {
        let dir = project();
        let ctx = ToolContext::new(dir.path());
        let output = Grep
            .run(json!({"pattern": "todo", "glob": "*.md"}), &ctx)
            .await;
        assert!(output.content.contains("notes.md:1:todo: write docs"));
        assert!(!output.content.contains("src/"));
        let output = Grep
            .run(json!({"pattern": "todo", "glob": "src/**/*.rs"}), &ctx)
            .await;
        assert!(output.content.contains("src/lib.rs"));
        assert!(!output.content.contains("notes.md"));
        let output = Grep
            .run(json!({"pattern": "todo!", "path": "src"}), &ctx)
            .await;
        assert!(output.content.contains("src/main.rs:2:    todo!()"));
        assert!(!output.content.contains("lib.rs"));
        let output = Grep
            .run(json!({"pattern": "fn", "path": "src/main.rs"}), &ctx)
            .await;
        assert!(output.content.contains("src/main.rs:1:fn main() {"));
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
