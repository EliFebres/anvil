//! Which lines of a unified diff exist on its right side.
//!
//! GitHub only accepts an inline review comment on a line that the diff
//! shows, so a minor finding is posted inline when its file and line are in
//! here and in the review body otherwise.

use std::collections::{BTreeMap, BTreeSet};

/// The right-side (new file) line numbers a unified diff shows, per path.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiffLines {
    lines: BTreeMap<String, BTreeSet<u32>>,
}

impl DiffLines {
    /// Reads the `+++ b/<path>` headers and `@@` hunks of a unified diff.
    ///
    /// Added and context lines count; removed lines do not exist on the right
    /// side. A deleted file (`+++ /dev/null`) has no lines.
    ///
    /// # Arguments
    ///
    /// * `diff` - The diff as `git diff` or the GitHub API prints it.
    ///
    /// # Returns
    ///
    /// The lines per path.
    pub fn parse(diff: &str) -> Self {
        let mut lines: BTreeMap<String, BTreeSet<u32>> = BTreeMap::new();
        let mut path: Option<String> = None;
        let mut after_minus_header = false;
        let mut next_line: Option<u32> = None;
        for raw in diff.lines() {
            if raw.starts_with("diff --git ") {
                path = None;
                next_line = None;
                after_minus_header = false;
                continue;
            }
            if after_minus_header && let Some(rest) = raw.strip_prefix("+++ ") {
                let name = rest.split('\t').next().unwrap_or(rest);
                path = name.strip_prefix("b/").map(str::to_owned);
                next_line = None;
                after_minus_header = false;
                continue;
            }
            after_minus_header = raw.starts_with("--- ") && next_line.is_none();
            if let Some(start) = hunk_start(raw) {
                next_line = Some(start);
                continue;
            }
            let (Some(path), Some(line)) = (path.as_ref(), next_line.as_mut()) else {
                continue;
            };
            match raw.chars().next() {
                Some('+') | Some(' ') => {
                    lines.entry(path.clone()).or_default().insert(*line);
                    *line += 1;
                }
                None => {
                    // A blank context line, which some tools emit without the leading space.
                    lines.entry(path.clone()).or_default().insert(*line);
                    *line += 1;
                }
                _ => {}
            }
        }
        Self { lines }
    }

    /// Whether `line` of `path` is visible on the right side of the diff.
    ///
    /// # Arguments
    ///
    /// * `path` - Repository-relative path as the diff names it.
    /// * `line` - Line number in the new version of the file.
    ///
    /// # Returns
    ///
    /// `true` when an inline comment there would be accepted.
    pub fn contains(&self, path: &str, line: u32) -> bool {
        self.lines.get(path).is_some_and(|set| set.contains(&line))
    }

    /// The paths the diff touches on its right side.
    ///
    /// # Returns
    ///
    /// Sorted paths; deleted files are absent.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.lines.keys().map(String::as_str)
    }
}

/// The new-file start line of a `@@ -a,b +c,d @@` header.
fn hunk_start(line: &str) -> Option<u32> {
    let rest = line.strip_prefix("@@ ")?;
    let plus = rest.split(' ').find_map(|part| part.strip_prefix('+'))?;
    plus.split(',').next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIFF: &str = "diff --git a/src/lib.rs b/src/lib.rs
index 1..2 100644
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,3 +1,4 @@
 fn a() {}
-fn b() {}
+fn b() { todo!() }
+fn c() {}
 fn d() {}
@@ -20,2 +21,3 @@
 x
+y
 z
diff --git a/gone.txt b/gone.txt
deleted file mode 100644
--- a/gone.txt
+++ /dev/null
@@ -1,2 +0,0 @@
-one
-two
diff --git a/new.txt b/new.txt
new file mode 100644
--- /dev/null
+++ b/new.txt
@@ -0,0 +1 @@
+hello
";

    #[test]
    fn added_and_context_lines_are_on_the_right_side() {
        let lines = DiffLines::parse(DIFF);
        for line in 1..=4 {
            assert!(lines.contains("src/lib.rs", line), "line {line}");
        }
        assert!(!lines.contains("src/lib.rs", 5));
        assert!(lines.contains("src/lib.rs", 21));
        assert!(lines.contains("src/lib.rs", 22));
        assert!(lines.contains("src/lib.rs", 23));
        assert!(!lines.contains("src/lib.rs", 24));
        assert!(lines.contains("new.txt", 1));
        assert!(!lines.contains("gone.txt", 1));
        assert_eq!(
            lines.paths().collect::<Vec<_>>(),
            vec!["new.txt", "src/lib.rs"]
        );
    }

    #[test]
    fn hunk_header_without_a_count_is_a_single_line() {
        assert_eq!(hunk_start("@@ -0,0 +1 @@"), Some(1));
        assert_eq!(hunk_start("@@ -3,4 +7,8 @@ fn x()"), Some(7));
        assert_eq!(hunk_start("not a hunk"), None);
    }

    #[test]
    fn empty_diff_has_no_lines() {
        let lines = DiffLines::parse("");
        assert!(!lines.contains("a", 1));
        assert_eq!(lines.paths().count(), 0);
    }
}
