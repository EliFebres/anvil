//! The keyword outline for languages without a tree-sitter grammar here.

use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

use super::Item;

/// Extensions a directory outline includes through this fallback.
const EXTENSIONS: &[&str] = &[
    "go", "java", "kt", "kts", "scala", "c", "h", "cc", "cpp", "hpp", "cs", "rb", "swift", "php",
    "lua", "sh", "bash", "ex", "exs", "zig", "dart", "hs", "ml",
];

/// A line that starts a definition, after optional modifiers.
static DEFINITION: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(
        r"^\s*(?:(?:pub(?:\([^)]*\))?|export|public|private|protected|internal|static|async|abstract|final|override|open|sealed|data|inline)\s+)*(?:fn|def|defp|defmodule|class|struct|enum|trait|interface|func|function|type|module|impl|object|namespace|protocol|extension)\b",
    )
    .ok()
});

/// A line that imports something.
static IMPORT: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(r"^\s*(?:import|use|using|require|include|#include|from\s+\S+\s+import)\b").ok()
});

/// Whether the fallback covers files with this path's extension.
pub(super) fn knows(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| EXTENSIONS.contains(&ext))
}

/// A flat outline of definition lines; each ends where the next begins.
pub(super) fn outline(text: &str) -> Vec<Item> {
    let (Some(definition), Some(import)) = (DEFINITION.as_ref(), IMPORT.as_ref()) else {
        return Vec::new();
    };
    let lines: Vec<&str> = text.lines().collect();
    let mut items: Vec<Item> = Vec::new();
    let mut imports: Option<(usize, usize, usize)> = None;
    for (index, line) in lines.iter().enumerate() {
        let number = index + 1;
        if import.is_match(line) {
            let (start, _, count) = imports.unwrap_or((number, number, 0));
            imports = Some((start, number, count + 1));
            continue;
        }
        if definition.is_match(line) {
            items.push(Item::new(line, number, number, None));
        }
    }
    let starts: Vec<usize> = items.iter().map(|item| item.start).skip(1).collect();
    for (item, next) in items
        .iter_mut()
        .zip(starts.into_iter().map(Some).chain([None]))
    {
        item.end = last_content_line(&lines, next.map_or(lines.len(), |n| n - 1)).max(item.start);
    }
    if let Some((start, end, count)) = imports {
        items.insert(0, Item::imports(start, end, count));
    }
    items
}

/// The last non-blank line at or before `line` (1-based).
fn last_content_line(lines: &[&str], line: usize) -> usize {
    (1..=line)
        .rev()
        .find(|n| !lines[n - 1].trim().is_empty())
        .unwrap_or(line)
}
