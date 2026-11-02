//! Word wrapping by display width, shared by the transcript entries.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Wraps `text` into lines no wider than `width` columns.
///
/// Existing line breaks are kept and words longer than the width are
/// split at a character boundary.
///
/// # Arguments
///
/// * `text` - The text.
/// * `width` - Columns available; zero counts as one.
///
/// # Returns
///
/// At least one line, each at most `width` columns wide.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for raw in text.split('\n') {
        wrap_line(raw, width, &mut out);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// Wraps one line without newlines, appending to `out`.
fn wrap_line(line: &str, width: usize, out: &mut Vec<String>) {
    let mut current = String::new();
    let mut current_width = 0;
    let mut has_word = false;
    for word in line.split(' ') {
        let word_width = word.width();
        if !has_word {
            push_word(word, width, &mut current, &mut current_width, out);
            has_word = true;
        } else if current_width + 1 + word_width <= width {
            current.push(' ');
            current.push_str(word);
            current_width += 1 + word_width;
        } else {
            out.push(std::mem::take(&mut current));
            current_width = 0;
            push_word(word, width, &mut current, &mut current_width, out);
        }
    }
    out.push(current);
}

/// Appends one word to the current line, splitting it when it is wider
/// than a whole line.
fn push_word(
    word: &str,
    width: usize,
    current: &mut String,
    current_width: &mut usize,
    out: &mut Vec<String>,
) {
    if word.width() <= width {
        current.push_str(word);
        *current_width = word.width();
        return;
    }
    for ch in word.chars() {
        let ch_width = ch.width().unwrap_or(0);
        if *current_width + ch_width > width && *current_width > 0 {
            out.push(std::mem::take(current));
            *current_width = 0;
        }
        current.push(ch);
        *current_width += ch_width;
    }
}

/// Splits one line into display segments no wider than `width`, keeping
/// the character offset where each segment starts.
///
/// # Arguments
///
/// * `line` - Text without newlines.
/// * `width` - Columns per segment; zero counts as one.
///
/// # Returns
///
/// At least one segment.
pub fn segments(line: &str, width: usize) -> Vec<(usize, String)> {
    let width = width.max(1);
    let mut out = Vec::new();
    let mut current = String::new();
    let mut current_width = 0;
    let mut start = 0;
    for (index, ch) in line.chars().enumerate() {
        let ch_width = ch.width().unwrap_or(0);
        if current_width + ch_width > width && !current.is_empty() {
            out.push((start, std::mem::take(&mut current)));
            current_width = 0;
            start = index;
        }
        current.push(ch);
        current_width += ch_width;
    }
    out.push((start, current));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_at_word_boundaries_and_keeps_newlines() {
        assert_eq!(
            wrap("one two three four", 9),
            vec!["one two", "three", "four"]
        );
        assert_eq!(wrap("a\n\nb", 10), vec!["a", "", "b"]);
        assert_eq!(wrap("", 10), vec![""]);
    }

    #[test]
    fn splits_words_wider_than_a_line() {
        assert_eq!(wrap("abcdefgh", 3), vec!["abc", "def", "gh"]);
        assert_eq!(wrap("x abcdefgh", 3), vec!["x", "abc", "def", "gh"]);
    }

    #[test]
    fn segments_keep_offsets() {
        assert_eq!(
            segments("abcde", 2),
            vec![
                (0, "ab".to_owned()),
                (2, "cd".to_owned()),
                (4, "e".to_owned())
            ]
        );
        assert_eq!(segments("", 2), vec![(0, String::new())]);
    }
}
