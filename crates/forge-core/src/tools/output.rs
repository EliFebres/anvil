//! Truncating tool output and keeping the full text on disk.

use std::path::{Path, PathBuf};

/// Folder under the session directory that holds full tool outputs.
const OUTPUT_DIR: &str = "tool-output";

/// Cuts `content` to at most `cap` bytes on a character boundary.
///
/// # Arguments
///
/// * `content` - The full output.
/// * `cap` - Byte cap.
///
/// # Returns
///
/// The truncated prefix, or `None` when the content already fits.
pub(crate) fn truncate(content: &str, cap: usize) -> Option<String> {
    if content.len() <= cap {
        return None;
    }
    let mut cut = cap;
    while !content.is_char_boundary(cut) {
        cut -= 1;
    }
    Some(content[..cut].to_owned())
}

/// Appends the truncation note.
///
/// # Arguments
///
/// * `truncated` - The truncated prefix.
/// * `total` - Byte length of the full output.
/// * `full_output` - Where the full output was written, if anywhere.
///
/// # Returns
///
/// The text for the model.
pub(crate) fn with_note(mut truncated: String, total: usize, full_output: Option<&Path>) -> String {
    let shown = truncated.len();
    truncated.push_str(&format!(
        "\n\n[output truncated: {shown} of {total} bytes shown"
    ));
    if let Some(path) = full_output {
        truncated.push_str(&format!("; full output at {}", path.display()));
    }
    truncated.push(']');
    truncated
}

/// Writes the full output to `<session_dir>/tool-output/<call id>.txt`.
///
/// # Arguments
///
/// * `session_dir` - The session folder.
/// * `call_id` - The tool call id; unsafe characters become `_`.
/// * `content` - The full output.
///
/// # Returns
///
/// The file's path.
///
/// # Errors
///
/// When the folder or file cannot be written.
pub(crate) fn write_full(
    session_dir: &Path,
    call_id: &str,
    content: &str,
) -> std::io::Result<PathBuf> {
    let dir = session_dir.join(OUTPUT_DIR);
    std::fs::create_dir_all(&dir)?;
    let safe: String = call_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let path = dir.join(format!("{safe}.txt"));
    std::fs::write(&path, content)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_respects_char_boundaries() {
        assert_eq!(truncate("abc", 3), None);
        assert_eq!(truncate("abcd", 3), Some("abc".to_owned()));
        assert_eq!(truncate("éé", 3), Some("é".to_owned()));
    }

    #[test]
    fn note_mentions_path_only_when_present() {
        let text = with_note("ab".to_owned(), 10, None);
        assert_eq!(text, "ab\n\n[output truncated: 2 of 10 bytes shown]");
        let text = with_note("ab".to_owned(), 10, Some(Path::new("out.txt")));
        assert!(text.ends_with("; full output at out.txt]"));
    }

    #[test]
    fn write_full_sanitizes_the_file_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_full(dir.path(), "call/..\\1", "data").expect("writes");
        assert_eq!(
            path.file_name().and_then(|n| n.to_str()),
            Some("call____1.txt")
        );
        assert_eq!(std::fs::read_to_string(path).expect("reads"), "data");
    }
}
