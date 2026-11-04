//! Structural outlines of source files: what is defined where, without the bodies.
//!
//! An outline lists imports (collapsed to one line), then every function,
//! method, class, struct, enum, trait, impl block, module, TOML table or
//! Markdown heading with its line range and the first line of its doc
//! comment. Rust, Python, TypeScript/JavaScript, TOML and Markdown are parsed
//! with tree-sitter; every other file falls back to a keyword regex.

mod fallback;
mod languages;

use std::path::Path;

pub use languages::Language;

/// Most bytes of one file's outline.
pub const FILE_CAP: usize = 4 * 1024;

/// Most bytes of a directory's outline.
pub const DIR_CAP: usize = 30 * 1024;

/// Nesting levels shown when the caller does not say.
pub const DEFAULT_DEPTH: usize = 2;

/// The last line of an outline that hit its cap.
pub const TRUNCATED_NOTE: &str = "[truncated, narrow the path]";

/// Longest signature shown, in characters.
const SIGNATURE_CHARS: usize = 80;

/// Longest doc line shown, in characters.
const DOC_CHARS: usize = 60;

/// One entry of an outline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// The declaration's first line, trimmed, or `imports (N)`.
    pub signature: String,
    /// First line, 1-based.
    pub start: usize,
    /// Last line, 1-based, inclusive.
    pub end: usize,
    /// The first line of the doc comment, if there is one.
    pub doc: Option<String>,
    /// Items nested inside this one.
    pub children: Vec<Item>,
}

impl Item {
    /// Builds an item, shortening the signature and doc to their display widths.
    fn new(signature: &str, start: usize, end: usize, doc: Option<String>) -> Self {
        debug_assert!(start <= end, "an item cannot end before it starts");
        let signature = signature.trim().trim_end_matches('{').trim_end();
        Item {
            signature: shorten(signature, SIGNATURE_CHARS),
            start,
            end,
            doc: doc
                .map(|d| shorten(d.trim(), DOC_CHARS))
                .filter(|d| !d.is_empty()),
            children: Vec::new(),
        }
    }

    /// One line standing for `count` consecutive imports.
    fn imports(start: usize, end: usize, count: usize) -> Self {
        Item::new(&format!("imports ({count})"), start, end, None)
    }
}

/// Outlines one file's text.
///
/// # Arguments
///
/// * `path` - The file's path; only its extension is read, to pick the language.
/// * `text` - The file's contents.
/// * `depth` - Nesting levels to keep; 1 keeps top-level items only. 0 is
///   treated as 1.
///
/// # Returns
///
/// The top-level items, children nested under their parents.
pub fn outline_source(path: &Path, text: &str, depth: usize) -> Vec<Item> {
    let depth = depth.max(1);
    let items = match Language::of(path) {
        Some(language) => languages::outline(language, text).unwrap_or_else(|| {
            tracing::warn!(path = %path.display(), "tree-sitter failed; using the keyword outline");
            fallback::outline(text)
        }),
        None => fallback::outline(text),
    };
    prune(items, depth)
}

/// The innermost item whose line range holds `line`, skipping import runs.
///
/// # Arguments
///
/// * `items` - An outline from [`outline_source`].
/// * `line` - A 1-based line number.
///
/// # Returns
///
/// The deepest enclosing item, or `None` when the line is outside every item.
pub fn enclosing(items: &[Item], line: usize) -> Option<&Item> {
    let item = items.iter().find(|item| {
        item.start <= line && line <= item.end && !item.signature.starts_with("imports (")
    })?;
    enclosing(&item.children, line).or(Some(item))
}

/// Whether `path` is a file a directory outline includes.
///
/// # Arguments
///
/// * `path` - The file's path.
///
/// # Returns
///
/// `true` for a parsed language or a language the keyword fallback knows.
pub fn is_source(path: &Path) -> bool {
    Language::of(path).is_some() || fallback::knows(path)
}

/// Renders one file's outline, capped at [`FILE_CAP`].
///
/// # Arguments
///
/// * `shown` - The path to print in the header.
/// * `text` - The file's contents.
/// * `depth` - Nesting levels to keep (see [`outline_source`]).
///
/// # Returns
///
/// A header `<shown> (<N> lines)`, then one line per item, ending with
/// [`TRUNCATED_NOTE`] when the cap was hit.
pub fn render_file(shown: &str, text: &str, depth: usize) -> String {
    let items = outline_source(Path::new(shown), text, depth);
    let mut lines = vec![format!("{shown} ({} lines)", text.lines().count())];
    if items.is_empty() {
        lines.push("(no definitions found)".to_owned());
    }
    push_items(&items, 0, &mut lines);
    cap_lines(&lines, FILE_CAP)
}

/// Joins already rendered file outlines, capped at [`DIR_CAP`].
///
/// # Arguments
///
/// * `files` - One [`render_file`] result per file, in display order.
///
/// # Returns
///
/// The outlines, ending with [`TRUNCATED_NOTE`] when the cap was hit, or
/// `(no source files)` when there were none.
pub fn render_dir(files: &[String]) -> String {
    if files.is_empty() {
        return "(no source files)".to_owned();
    }
    let lines: Vec<String> = files
        .iter()
        .flat_map(|file| file.lines().map(str::to_owned))
        .collect();
    cap_lines(&lines, DIR_CAP)
}

/// Drops items nested deeper than `depth`.
fn prune(items: Vec<Item>, depth: usize) -> Vec<Item> {
    items
        .into_iter()
        .map(|mut item| {
            item.children = if depth > 1 {
                prune(item.children, depth - 1)
            } else {
                Vec::new()
            };
            item
        })
        .collect()
}

/// Appends `start-end signature  // doc` lines, two spaces of indent per level.
fn push_items(items: &[Item], level: usize, lines: &mut Vec<String>) {
    for item in items {
        let indent = "  ".repeat(level);
        let mut line = format!("{indent}{}-{} {}", item.start, item.end, item.signature);
        if let Some(doc) = &item.doc {
            line.push_str("  // ");
            line.push_str(doc);
        }
        lines.push(line);
        push_items(&item.children, level + 1, lines);
    }
}

/// Joins lines with newlines, stopping before `cap` bytes and noting the cut.
fn cap_lines(lines: &[String], cap: usize) -> String {
    let whole: usize = lines.iter().map(|line| line.len() + 1).sum();
    if whole <= cap {
        return lines.iter().map(|line| format!("{line}\n")).collect();
    }
    let room = cap - TRUNCATED_NOTE.len() - 1;
    let mut out = String::new();
    for line in lines {
        if out.len() + line.len() + 1 > room {
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(TRUNCATED_NOTE);
    out.push('\n');
    debug_assert!(out.len() <= cap, "capped outline exceeds its cap");
    out
}

/// `text` cut to `max` characters, with `…` marking a cut.
fn shorten(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max - 1).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUST: &str = include_str!("fixtures/sample.rs");
    const PYTHON: &str = include_str!("fixtures/sample.py");
    const TYPESCRIPT: &str = include_str!("fixtures/sample.ts");

    #[test]
    fn rust_outline_snapshot() {
        insta::assert_snapshot!(render_file("sample.rs", RUST, DEFAULT_DEPTH));
    }

    #[test]
    fn enclosing_finds_the_innermost_item() {
        let items = outline_source(Path::new("sample.rs"), RUST, usize::MAX);
        let inner = enclosing(&items, 15).expect("inside increment");
        assert_eq!(inner.signature, "pub fn increment(&mut self, name: &str)");
        assert_eq!(
            enclosing(&items, 13).expect("in impl").signature,
            "impl Counter"
        );
        assert!(
            enclosing(&items, 3).is_none(),
            "imports are not an enclosing item"
        );
        assert!(enclosing(&items, 1).is_none());
    }

    #[test]
    fn python_outline_snapshot() {
        insta::assert_snapshot!(render_file("sample.py", PYTHON, DEFAULT_DEPTH));
    }

    #[test]
    fn typescript_outline_snapshot() {
        insta::assert_snapshot!(render_file("sample.ts", TYPESCRIPT, DEFAULT_DEPTH));
    }

    #[test]
    fn toml_and_markdown_outline_snapshot() {
        let toml = "# The package.\n[package]\nname = \"x\"\n\n[[bin]]\nname = \"y\"\n";
        let markdown = "# Title\n\nText.\n\n## Install\n\nRun it.\n\n### Linux\n\nApt.\n\n## Use\n";
        insta::assert_snapshot!(format!(
            "{}{}",
            render_file("Cargo.toml", toml, DEFAULT_DEPTH),
            render_file("README.md", markdown, 3)
        ));
    }

    #[test]
    fn depth_one_hides_methods() {
        let shallow = render_file("sample.rs", RUST, 1);
        assert!(shallow.contains("impl Counter"));
        assert!(!shallow.contains("fn increment"));
        let deep = render_file("sample.rs", RUST, 2);
        assert!(deep.contains("  "), "methods are indented under their impl");
        assert!(deep.contains("fn increment"));
    }

    #[test]
    fn unknown_languages_fall_back_to_keywords() {
        let go = "package main\n\nimport \"fmt\"\n\nfunc main() {\n\tfmt.Println(1)\n}\n\ntype T struct {\n}\n";
        let out = render_file("main.go", go, DEFAULT_DEPTH);
        assert!(out.contains("5-7 func main()"), "{out}");
        assert!(out.contains("9-10 type T struct"), "{out}");
        assert!(out.contains("imports (1)"), "{out}");
    }

    #[test]
    fn file_outline_is_capped_with_a_note() {
        let text: String = (0..2000)
            .map(|i| format!("/// Does thing number {i}.\nfn function_number_{i}() {{}}\n"))
            .collect();
        let out = render_file("big.rs", &text, DEFAULT_DEPTH);
        assert!(out.len() <= FILE_CAP, "{} bytes", out.len());
        assert!(out.ends_with(&format!("{TRUNCATED_NOTE}\n")));
        assert!(out.starts_with("big.rs (4000 lines)\n"));
    }

    #[test]
    fn directory_outline_is_capped_with_a_note() {
        let file = render_file("a.rs", RUST, DEFAULT_DEPTH);
        let files = vec![file; 200];
        let out = render_dir(&files);
        assert!(out.len() <= DIR_CAP, "{} bytes", out.len());
        assert!(out.ends_with(&format!("{TRUNCATED_NOTE}\n")));
        assert_eq!(render_dir(&[]), "(no source files)");
    }

    #[test]
    fn small_outlines_are_not_truncated() {
        let out = render_file("sample.py", PYTHON, DEFAULT_DEPTH);
        assert!(!out.contains(TRUNCATED_NOTE));
    }

    #[test]
    fn shorten_cuts_on_characters() {
        assert_eq!(shorten("abc", 3), "abc");
        assert_eq!(shorten("abcdé", 4), "abc…");
    }
}
