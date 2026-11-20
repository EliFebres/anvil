//! Line endings for the file tools that write.
//!
//! `read_file` shows lines without their endings, so a model copies text
//! with LF breaks even from a CRLF file. `edit_file` matches through a copy
//! of the file with every CRLF turned into LF and maps each match back to
//! the original bytes, so nothing outside a replaced span changes.
//! `write_file` keeps a CRLF file CRLF.

/// The UTF-8 byte order mark, as it appears at the start of a decoded file.
pub(crate) const BOM: char = '\u{feff}';

/// A line ending a text file uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ending {
    /// `\n`.
    Lf,
    /// `\r\n`.
    CrLf,
}

/// A text with every CRLF turned into LF, and where those CRLFs were.
#[derive(Debug)]
pub(crate) struct Normalized {
    /// The text with `\r\n` replaced by `\n`. A lone `\r` is kept.
    pub(crate) text: String,
    /// Offsets in `text` of each `\n` that was a `\r\n`, ascending.
    crlf_at: Vec<usize>,
}

impl Normalized {
    /// Builds the normalized copy of `original`.
    pub(crate) fn new(original: &str) -> Self {
        let mut text = String::with_capacity(original.len());
        let mut crlf_at = Vec::new();
        let mut rest = original;
        while let Some(at) = rest.find("\r\n") {
            text.push_str(&rest[..at]);
            crlf_at.push(text.len());
            text.push('\n');
            rest = &rest[at + 2..];
        }
        text.push_str(rest);
        Self { text, crlf_at }
    }

    /// Maps an offset in the normalized text to the original text.
    ///
    /// An offset that points at a `\n` which was a `\r\n` maps to the `\r`,
    /// so a span never splits a CRLF pair.
    pub(crate) fn to_original(&self, offset: usize) -> usize {
        offset + self.crlf_at.partition_point(|&at| at < offset)
    }
}

/// Turns every `\r\n` in `text` into `\n`.
pub(crate) fn to_lf(text: &str) -> String {
    text.replace("\r\n", "\n")
}

/// Writes `text` with every line break as `ending`.
pub(crate) fn with_ending(text: &str, ending: Ending) -> String {
    let lf = to_lf(text);
    match ending {
        Ending::Lf => lf,
        Ending::CrLf => lf.replace('\n', "\r\n"),
    }
}

/// The ending of the line break at byte `at` of `text`, if one is there.
fn ending_at(text: &str, at: usize) -> Ending {
    if at > 0 && text.as_bytes()[at - 1] == b'\r' {
        Ending::CrLf
    } else {
        Ending::Lf
    }
}

/// The ending new text takes when it replaces `text[start..end]`.
///
/// That is the ending of the first line the span touches: the first line
/// break at or after `start`. When the span is on a last line with no break,
/// the break before it decides, and a file with no break at all is LF.
pub(crate) fn ending_for_span(text: &str, start: usize) -> Ending {
    debug_assert!(start <= text.len(), "span start past the end of the text");
    if let Some(found) = text[start..].find('\n') {
        return ending_at(text, start + found);
    }
    match text[..start].rfind('\n') {
        Some(found) => ending_at(text, found),
        None => Ending::Lf,
    }
}

/// Whether every line break in `text` is CRLF, and there is at least one.
pub(crate) fn is_all_crlf(text: &str) -> bool {
    let breaks = text.matches('\n').count();
    breaks > 0 && text.matches("\r\n").count() == breaks
}

/// Whether `text` has a line break and none of them is CRLF.
pub(crate) fn is_lf_only(text: &str) -> bool {
    text.contains('\n') && !text.contains("\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalized_offsets_map_back_without_splitting_a_crlf() {
        let original = "a\r\nb\nc\r\n";
        let normalized = Normalized::new(original);
        assert_eq!(normalized.text, "a\nb\nc\n");
        assert_eq!(normalized.to_original(0), 0);
        assert_eq!(
            normalized.to_original(1),
            1,
            "the \\n of a CRLF maps to its \\r"
        );
        assert_eq!(normalized.to_original(2), 3);
        assert_eq!(normalized.to_original(4), 5);
        assert_eq!(normalized.to_original(5), 6);
        assert_eq!(normalized.to_original(6), original.len());
    }

    #[test]
    fn a_lone_carriage_return_is_ordinary_text() {
        let normalized = Normalized::new("a\rb\r\r\n");
        assert_eq!(normalized.text, "a\rb\r\n");
        assert_eq!(normalized.to_original(4), 4);
        assert_eq!(normalized.to_original(5), 6);
    }

    #[test]
    fn the_first_line_the_span_touches_decides_the_ending() {
        let text = "a\nb\r\nc";
        assert_eq!(ending_for_span(text, 0), Ending::Lf);
        assert_eq!(ending_for_span(text, 2), Ending::CrLf);
        assert_eq!(
            ending_for_span(text, 5),
            Ending::CrLf,
            "last line, no break"
        );
        assert_eq!(ending_for_span("abc", 1), Ending::Lf);
        assert_eq!(with_ending("x\ny\r\n", Ending::CrLf), "x\r\ny\r\n");
        assert_eq!(with_ending("x\r\ny\n", Ending::Lf), "x\ny\n");
    }

    #[test]
    fn all_crlf_and_lf_only_are_told_apart() {
        assert!(is_all_crlf("a\r\nb\r\n"));
        assert!(!is_all_crlf("a\r\nb\n"));
        assert!(!is_all_crlf("no break"));
        assert!(is_lf_only("a\nb"));
        assert!(!is_lf_only("a\r\nb\n"));
        assert!(!is_lf_only("no break"));
    }
}
