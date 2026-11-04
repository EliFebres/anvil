//! Line diffs for proposed edits: the changed lines with a little context.

use similar::{ChangeTag, TextDiff};

/// Unchanged lines kept on each side of a change.
pub const CONTEXT: usize = 3;

/// What one diff row is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    /// An unchanged line near a change.
    Context,
    /// A line the edit removes.
    Removed,
    /// A line the edit adds.
    Added,
}

/// One line of a diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffRow {
    /// What it is.
    pub kind: RowKind,
    /// The line without its line ending.
    pub text: String,
}

/// The diff of one edit: its rows and where in the file it starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diff {
    /// The first shown line's number in the old file, from one.
    pub start_line: usize,
    /// The last shown line's number in the old file.
    pub end_line: usize,
    /// The rows, changes with up to [`CONTEXT`] unchanged lines around them.
    pub rows: Vec<DiffRow>,
}

/// Diffs two versions of a file by line.
///
/// Line endings are normalized first, so a CRLF file diffs exactly like
/// its LF twin. Unchanged lines more than [`CONTEXT`] away from any change
/// are left out; separate hunks are joined without a marker.
///
/// # Arguments
///
/// * `old` - The file before.
/// * `new` - The file after.
///
/// # Returns
///
/// The diff; `rows` is empty when nothing changed.
pub fn diff(old: &str, new: &str) -> Diff {
    let old = normalize(old);
    let new = normalize(new);
    let text_diff = TextDiff::from_lines(old.as_str(), new.as_str());
    let mut rows = Vec::new();
    let mut start_line = None;
    let mut end_line = 0;
    for group in text_diff.grouped_ops(CONTEXT) {
        for op in group {
            for change in text_diff.iter_changes(&op) {
                let kind = match change.tag() {
                    ChangeTag::Equal => RowKind::Context,
                    ChangeTag::Delete => RowKind::Removed,
                    ChangeTag::Insert => RowKind::Added,
                };
                if let Some(index) = change.old_index() {
                    let line = index + 1;
                    start_line.get_or_insert(line);
                    end_line = end_line.max(line);
                }
                rows.push(DiffRow {
                    kind,
                    text: change.value().trim_end_matches('\n').to_owned(),
                });
            }
        }
    }
    let start_line = start_line.unwrap_or(1);
    Diff {
        start_line,
        end_line: end_line.max(start_line),
        rows,
    }
}

/// Turns CRLF line endings into LF.
fn normalize(text: &str) -> String {
    text.replace("\r\n", "\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(diff: &Diff) -> Vec<(RowKind, &str)> {
        diff.rows
            .iter()
            .map(|row| (row.kind, row.text.as_str()))
            .collect()
    }

    #[test]
    fn changes_carry_three_lines_of_context_and_line_numbers() {
        let old = (1..=20)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let new = old.replace("line 10", "LINE TEN");
        let diff = diff(&old, &new);
        assert_eq!(diff.start_line, 7);
        assert_eq!(diff.end_line, 13);
        assert_eq!(
            kinds(&diff),
            vec![
                (RowKind::Context, "line 7"),
                (RowKind::Context, "line 8"),
                (RowKind::Context, "line 9"),
                (RowKind::Removed, "line 10"),
                (RowKind::Added, "LINE TEN"),
                (RowKind::Context, "line 11"),
                (RowKind::Context, "line 12"),
                (RowKind::Context, "line 13"),
            ]
        );
    }

    #[test]
    fn crlf_and_lf_edits_diff_the_same() {
        let old_lf = "fn a() {}\nfn b() {}\nfn c() {}\n";
        let new_lf = "fn a() {}\nfn beta() {}\nfn c() {}\n";
        let old_crlf = old_lf.replace('\n', "\r\n");
        let new_crlf = new_lf.replace('\n', "\r\n");
        let lf = diff(old_lf, new_lf);
        assert_eq!(diff(&old_crlf, &new_crlf), lf);
        assert_eq!(diff(&old_crlf, new_lf), lf);
        assert_eq!(
            kinds(&lf),
            vec![
                (RowKind::Context, "fn a() {}"),
                (RowKind::Removed, "fn b() {}"),
                (RowKind::Added, "fn beta() {}"),
                (RowKind::Context, "fn c() {}"),
            ]
        );
        assert!(!lf.rows.iter().any(|row| row.text.contains('\r')));
    }

    #[test]
    fn a_new_file_is_all_additions_and_no_change_is_empty() {
        let fresh = diff("", "a\nb\n");
        assert_eq!(
            kinds(&fresh),
            vec![(RowKind::Added, "a"), (RowKind::Added, "b")]
        );
        assert_eq!(fresh.start_line, 1);
        assert!(diff("same\n", "same\n").rows.is_empty());
    }
}
