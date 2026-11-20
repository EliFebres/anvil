//! The message `edit_file` returns when its old text is not in the file.
//!
//! The message names what is closest and how it differs, so a model can
//! correct the call instead of guessing: a white space difference, the
//! file's real lines where the old text's first line occurs, or nothing
//! similar. It shows at most the lines of that one region, and never more
//! than [`MAX_BYTES`].

/// Most file lines one message shows.
pub(crate) const MAX_LINES: usize = 20;

/// Most characters shown of one line; the rest is cut.
pub(crate) const MAX_LINE_CHARS: usize = 160;

/// Most bytes in one message.
pub(crate) const MAX_BYTES: usize = 4096;

/// Most further line numbers listed where the same line also occurs.
const MAX_OTHER_HITS: usize = 5;

/// What ends a message cut at [`MAX_BYTES`].
const CUT_NOTE: &str = "\n[message cut]";

/// Builds the not-found message.
///
/// `file` is the file's text with LF line breaks, no byte order mark, and
/// already redacted; `old` is the old text with LF line breaks.
pub(crate) fn not_found(shown: &str, file: &str, old: &str) -> String {
    let file_lines: Vec<&str> = file.lines().collect();
    let old_lines: Vec<&str> = old.lines().collect();
    let message = match old_lines.iter().position(|line| !line.trim().is_empty()) {
        None => format!(
            "old text not found in {shown}. It holds only white space and line breaks, which \
             must match exactly; read the file again with read_file and include a line with \
             text in it."
        ),
        Some(first) => whitespace_hint(shown, &file_lines, &old_lines)
            .or_else(|| first_line_hint(shown, &file_lines, &old_lines, first))
            .unwrap_or_else(|| nothing_similar(shown, old_lines[first])),
    };
    cap(message)
}

/// The message when the old text matches once leading and trailing white
/// space on each line is ignored.
fn whitespace_hint(shown: &str, file_lines: &[&str], old_lines: &[&str]) -> Option<String> {
    let count = old_lines.len();
    if count == 0 || count > file_lines.len() {
        return None;
    }
    for start in 0..=file_lines.len() - count {
        let window = &file_lines[start..start + count];
        let loose = (0..count).all(|i| fits(window[i], old_lines[i], i, count, true));
        if !loose {
            continue;
        }
        let differs = (0..count).find(|&i| !fits(window[i], old_lines[i], i, count, false))?;
        return Some(format!(
            "old text not found in {shown}, but it matches at line {} when leading and trailing \
             white space is ignored, so the indentation or trailing white space differs. At line \
             {}:\n  file: |{}|\n  old:  |{}|\n(· is a space, → is a tab.) Copy the white space \
             exactly; edit_file ignores only the difference between LF and CRLF line endings.",
            start + 1,
            start + differs + 1,
            visible(window[differs]),
            visible(old_lines[differs]),
        ));
    }
    None
}

/// Whether one file line fits one line of the old text.
///
/// The old text's first line may start mid-line and its last line may end
/// mid-line, so those are compared as a suffix and a prefix. With `loose`,
/// leading and trailing white space is ignored on both sides.
fn fits(file: &str, old: &str, index: usize, count: usize, loose: bool) -> bool {
    let (file_whole, old) = if loose {
        (file.trim(), old.trim())
    } else {
        (file, old)
    };
    if count == 1 {
        !old.is_empty() && file_whole.contains(old)
    } else if index == 0 {
        file_whole.ends_with(old)
    } else if index == count - 1 {
        file_whole.starts_with(old)
    } else {
        file_whole == old
    }
}

/// The message when the old text's first non-blank line occurs in the file:
/// the file's real lines for the region the old text would cover.
fn first_line_hint(
    shown: &str,
    file_lines: &[&str],
    old_lines: &[&str],
    first: usize,
) -> Option<String> {
    let key = old_lines[first].trim();
    let hits: Vec<usize> = file_lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.contains(key))
        .map(|(index, _)| index)
        .collect();
    let &hit = hits.first()?;
    let start = hit.saturating_sub(first);
    let end = (start + old_lines.len())
        .min(file_lines.len())
        .min(start + MAX_LINES);
    let mut out = format!(
        "old text not found in {shown}. Its line `{}` occurs at line {}",
        clip(key),
        hit + 1
    );
    if hits.len() > 1 {
        let others: Vec<String> = hits[1..]
            .iter()
            .take(MAX_OTHER_HITS)
            .map(|index| (index + 1).to_string())
            .collect();
        out.push_str(&format!(" (also at {}", others.join(", ")));
        if hits.len() - 1 > MAX_OTHER_HITS {
            out.push_str(&format!(" and {} more", hits.len() - 1 - MAX_OTHER_HITS));
        }
        out.push(')');
    }
    out.push_str(&format!(
        ". The file's lines {}-{} are below; the old text differs from them. Copy the text from \
         them exactly, without the line numbers.\n",
        start + 1,
        end
    ));
    if old_lines.len() > MAX_LINES {
        out.push_str(&format!(
            "(Showing {MAX_LINES} of the {} lines the old text spans.)\n",
            old_lines.len()
        ));
    }
    for (index, line) in file_lines.iter().enumerate().take(end).skip(start) {
        out.push_str(&format!("{:>6}\t{}\n", index + 1, clip(line)));
    }
    Some(out)
}

/// The message when nothing in the file resembles the old text.
fn nothing_similar(shown: &str, first_line: &str) -> String {
    format!(
        "old text not found in {shown}, and no line of the file contains `{}`, the first line \
         of the old text. The file may have changed since it was read, or the path may be \
         wrong: read it again with read_file and copy the text exactly.",
        clip(first_line.trim())
    )
}

/// Shows a line with its spaces, tabs and carriage returns made visible.
fn visible(line: &str) -> String {
    clip(line)
        .chars()
        .map(|c| match c {
            ' ' => '·',
            '\t' => '→',
            '\r' => '␍',
            other => other,
        })
        .collect()
}

/// Cuts a line to [`MAX_LINE_CHARS`] characters.
fn clip(line: &str) -> String {
    match line.char_indices().nth(MAX_LINE_CHARS) {
        Some((at, _)) => format!("{}...", &line[..at]),
        None => line.to_owned(),
    }
}

/// Cuts a message to [`MAX_BYTES`] on a character boundary.
fn cap(mut message: String) -> String {
    if message.len() <= MAX_BYTES {
        return message;
    }
    let mut at = MAX_BYTES - CUT_NOTE.len();
    while !message.is_char_boundary(at) {
        at -= 1;
    }
    message.truncate(at);
    message.push_str(CUT_NOTE);
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_keeps_a_character_whole() {
        let long = "é".repeat(MAX_BYTES);
        let capped = cap(long);
        assert!(capped.len() <= MAX_BYTES);
        assert!(capped.ends_with(CUT_NOTE));
    }

    #[test]
    fn visible_marks_spaces_tabs_and_carriage_returns() {
        assert_eq!(visible(" \tx\r"), "·→x␍");
    }
}
