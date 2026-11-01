//! The data boundary between anvil and the factory.
//!
//! Ticket text travels from a anvil run on someone's project to the factory,
//! which must never receive that project's code or paths. These checks are
//! purely textual so they give the same answer on any machine, with or
//! without the project present.

use crate::error::ValidationError;

/// Maximum number of code lines allowed in one ticket text field.
pub const MAX_CODE_LINES: usize = 40;

/// Checks one ticket text field against the data boundary.
///
/// # Arguments
///
/// * `field` - Name of the field, used in error messages.
/// * `text` - The field's contents.
///
/// # Returns
///
/// `Ok(())` when the text names no path outside the monorepo and contains at
/// most [`MAX_CODE_LINES`] lines of code. Code lines are the lines inside
/// ``` fences; when a field has no fences at all, every non-blank line
/// counts, so unfenced code cannot slip through.
///
/// # Errors
///
/// [`ValidationError::OutsidePath`] for the first offending token, or
/// [`ValidationError::TooMuchCode`] when the code-line limit is exceeded.
pub fn check_text(field: &'static str, text: &str) -> Result<(), ValidationError> {
    if let Some(path) = find_outside_path(text) {
        return Err(ValidationError::OutsidePath {
            field,
            path: path.to_owned(),
        });
    }
    let lines = count_code_lines(text);
    if lines > MAX_CODE_LINES {
        return Err(ValidationError::TooMuchCode {
            field,
            lines,
            max: MAX_CODE_LINES,
        });
    }
    Ok(())
}

/// Returns the first whitespace-delimited token that looks like a path outside the monorepo.
fn find_outside_path(text: &str) -> Option<&str> {
    text.split_whitespace()
        .map(strip_wrapping_punctuation)
        .find(|token| is_outside_path(token))
}

/// Removes quotes, backticks and brackets that commonly wrap a path in prose.
fn strip_wrapping_punctuation(token: &str) -> &str {
    token.trim_matches(|c: char| {
        matches!(
            c,
            '`' | '"' | '\'' | '(' | ')' | '[' | ']' | '<' | '>' | ',' | ';' | ':'
        )
    })
}

/// Decides whether one token is an absolute, home-relative, traversing or `file:` path.
fn is_outside_path(token: &str) -> bool {
    if token.to_ascii_lowercase().starts_with("file:") {
        return true;
    }
    // Any other URL (https://host/v1) is not a filesystem path.
    if token.contains("://") {
        return false;
    }
    let is_unix_absolute = token.starts_with('/') && looks_like_path_start(token.chars().nth(1));
    let is_home_relative = token.starts_with("~/") || token.starts_with("~\\");
    let is_unc = token.starts_with("\\\\");
    let traverses = token.contains("../") || token.contains("..\\");
    is_unix_absolute || is_home_relative || is_unc || has_drive_prefix(token) || traverses
}

/// Distinguishes `/etc` from `//` comments and `/*` in code samples.
fn looks_like_path_start(next: Option<char>) -> bool {
    next.is_some_and(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_')
}

/// True for Windows drive-letter paths such as `C:\Users` or `D:/Coding`.
fn has_drive_prefix(token: &str) -> bool {
    let mut chars = token.chars();
    let letter = chars.next().is_some_and(|c| c.is_ascii_alphabetic());
    let colon = chars.next() == Some(':');
    let separator = matches!(chars.next(), Some('\\') | Some('/'));
    letter && colon && separator
}

/// Counts lines inside ``` fences, or every non-blank line when there are no fences.
fn count_code_lines(text: &str) -> usize {
    let mut inside_fence = false;
    let mut saw_fence = false;
    let mut fenced_lines = 0;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            inside_fence = !inside_fence;
            saw_fence = true;
            continue;
        }
        if inside_fence {
            fenced_lines += 1;
        }
    }
    if saw_fence {
        fenced_lines
    } else {
        text.lines().filter(|line| !line.trim().is_empty()).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fenced(lines: usize) -> String {
        let body: Vec<String> = (0..lines).map(|i| format!("let x{i} = {i};")).collect();
        format!("Repro:\n```rust\n{}\n```\n", body.join("\n"))
    }

    fn unfenced(lines: usize) -> String {
        (0..lines)
            .map(|i| format!("step {i}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn rejects_paths_outside_the_monorepo() {
        let leaks = [
            "see /etc/passwd for details",
            "open `C:\\Users\\someone\\secret.rs`",
            "lives in D:/Coding/other-project/src",
            "keys are in ~/.ssh/id_rsa",
            "walk up with ../../other/Cargo.toml",
            "or ..\\sibling\\file.txt on Windows",
            "share at \\\\server\\share\\x",
            "url file:///etc/hosts",
            "wrapped (\"/var/log/syslog\")",
        ];
        for text in leaks {
            let err = check_text("spec", text).expect_err(text);
            assert!(
                matches!(err, ValidationError::OutsidePath { field: "spec", .. }),
                "{text}: {err}"
            );
        }
    }

    #[test]
    fn accepts_repo_relative_paths_urls_and_flags() {
        let fine = [
            "edit crates/anvil-core/src/lib.rs",
            "POST https://host.example/v1/chat/completions",
            "pass --dry-run and --json",
            "```rust\n// why: keep it simple\nlet a = 1 / 2; /* half */\n```",
            "a/b/c.toml and ./relative/path",
        ];
        for text in fine {
            check_text("spec", text).unwrap_or_else(|e| panic!("{text}: {e}"));
        }
    }

    #[test]
    fn allows_exactly_forty_fenced_lines() {
        check_text("repro", &fenced(MAX_CODE_LINES)).expect("40 lines is within the limit");
    }

    #[test]
    fn rejects_forty_one_fenced_lines() {
        let err = check_text("repro", &fenced(MAX_CODE_LINES + 1)).expect_err("41 lines");
        assert_eq!(
            err,
            ValidationError::TooMuchCode {
                field: "repro",
                lines: 41,
                max: 40
            }
        );
    }

    #[test]
    fn counts_every_non_blank_line_when_there_are_no_fences() {
        check_text("spec", &unfenced(MAX_CODE_LINES)).expect("40 unfenced lines");
        let err = check_text("spec", &unfenced(MAX_CODE_LINES + 1)).expect_err("41 unfenced lines");
        assert!(
            matches!(err, ValidationError::TooMuchCode { lines: 41, .. }),
            "{err}"
        );
    }

    #[test]
    fn sums_lines_across_fences_and_ignores_prose_between_them() {
        let prose: String = (0..100).map(|i| format!("prose line {i}\n")).collect();
        let text = format!("{}{prose}{}", fenced(30), fenced(10));
        check_text("spec", &text).expect("30 + 10 fenced lines, prose is free");
        let text = format!("{}{prose}{}", fenced(30), fenced(11));
        assert!(check_text("spec", &text).is_err(), "30 + 11 fenced lines");
    }

    #[test]
    fn unterminated_fence_counts_to_the_end() {
        let body = unfenced(MAX_CODE_LINES + 1);
        let text = format!("```\n{body}");
        assert!(check_text("spec", &text).is_err());
    }

    #[test]
    fn empty_text_passes() {
        check_text("repro", "").expect("nothing to leak");
    }
}
