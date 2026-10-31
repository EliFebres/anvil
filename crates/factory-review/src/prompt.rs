//! The prompts: one system prompt per stance, one user message per request.

use std::fmt::Write as _;

use anvil_types::Stance;

use crate::reviewer::ReviewRequest;
use crate::rules::Rule;

/// Bytes of diff sent to a reviewer before the rest is cut.
pub const MAX_DIFF_BYTES: usize = 300 * 1024;

/// The JSON contract every stance ends with.
const OUTPUT: &str = include_str!("../prompts/output.md");

/// The stance's own text.
fn stance_text(stance: Stance) -> &'static str {
    match stance {
        Stance::Correctness => include_str!("../prompts/correctness.md"),
        Stance::Safety => include_str!("../prompts/safety.md"),
        Stance::Style => include_str!("../prompts/style.md"),
        Stance::Simplicity => include_str!("../prompts/simplicity.md"),
    }
}

/// The system prompt for a stance.
///
/// # Arguments
///
/// * `stance` - Which stance.
/// * `rules` - The standing rules; only the style stance receives them.
///
/// # Returns
///
/// The stance text, the rules section when it applies, and the output contract.
pub fn system_prompt(stance: Stance, rules: &[Rule]) -> String {
    let mut text = stance_text(stance).trim_end().to_owned();
    if stance == Stance::Style {
        text.push_str("\n\n# Standing rules\n\n");
        if rules.is_empty() {
            text.push_str("(no rules.toml on this machine; apply CLAUDE.md only)\n");
        }
        for rule in rules {
            let _ = writeln!(text, "- `{}`: {}", rule.id, rule.rule.trim());
        }
    }
    text.push_str("\n\n");
    text.push_str(OUTPUT.trim_end());
    text.push('\n');
    text
}

/// The user message: ticket, guidelines, changed files and the diff.
///
/// # Arguments
///
/// * `request` - What to review.
///
/// # Returns
///
/// The message, with the diff cut at [`MAX_DIFF_BYTES`] and a marker saying so.
pub fn user_message(request: &ReviewRequest) -> String {
    let mut text = String::new();
    let _ = writeln!(text, "# Ticket\n\n## {}\n", request.ticket.title.trim());
    let _ = writeln!(text, "{}\n", request.ticket.description.trim());
    text.push_str("## Acceptance\n\n");
    for item in &request.ticket.acceptance {
        let _ = writeln!(text, "- {}", item.trim());
    }
    if let Some(guidelines) = &request.guidelines {
        let _ = writeln!(
            text,
            "\n# Repository guidelines (CLAUDE.md)\n\n{}",
            guidelines.trim()
        );
    }
    text.push_str("\n# Changed files\n\n");
    for file in &request.changed_files {
        let _ = writeln!(text, "- {file}");
    }
    text.push_str("\n# Diff\n\n```diff\n");
    text.push_str(&truncated(&request.diff));
    text.push_str("\n```\n");
    text
}

/// The diff, cut at a character boundary with a marker when it is too long.
fn truncated(diff: &str) -> String {
    if diff.len() <= MAX_DIFF_BYTES {
        return diff.trim_end().to_owned();
    }
    let mut cut = MAX_DIFF_BYTES;
    while !diff.is_char_boundary(cut) {
        cut -= 1;
    }
    format!(
        "{}\n[diff truncated at {} KB; {} more bytes not shown]",
        &diff[..cut],
        MAX_DIFF_BYTES / 1024,
        diff.len() - cut
    )
}

/// The message sent after a reply that was not a review.
///
/// # Arguments
///
/// * `detail` - What was wrong with the reply.
///
/// # Returns
///
/// The message.
pub fn retry_message(detail: &str) -> String {
    format!(
        "Your reply was not a well-formed review: {detail}\n\nReply again with exactly one JSON object matching the contract: a `verdict` of \"approve\" or \"changes\" and a `findings` array. No prose, no code fences."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reviewer::TicketSummary;

    fn request() -> ReviewRequest {
        ReviewRequest {
            ticket: TicketSummary {
                title: "Add --count to grep clone".to_owned(),
                description: "Symptom:\nNo way to count matches.\n\nSpec:\nAdd a --count flag."
                    .to_owned(),
                acceptance: vec![
                    "`--count` prints only the number of matching lines".to_owned(),
                    "existing output unchanged without the flag".to_owned(),
                ],
            },
            diff: "diff --git a/src/main.rs b/src/main.rs\n--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1 +1,2 @@\n fn main() {}\n+fn count() {}\n".to_owned(),
            changed_files: vec!["src/main.rs".to_owned()],
            guidelines: Some("# CLAUDE.md\n\nNo unwrap outside tests.\n".to_owned()),
        }
    }

    #[test]
    fn system_prompts_are_stable() {
        let rules = vec![
            Rule {
                id: "no-unwrap".to_owned(),
                rule: "No unwrap() outside tests.".to_owned(),
            },
            Rule {
                id: "google-docstrings".to_owned(),
                rule: "Every public item has a Google-shape doc comment.".to_owned(),
            },
        ];
        for stance in Stance::ALL {
            insta::assert_snapshot!(format!("system_{stance}"), system_prompt(stance, &rules));
        }
    }

    #[test]
    fn style_without_rules_says_so() {
        let text = system_prompt(Stance::Style, &[]);
        assert!(text.contains("no rules.toml"));
        assert!(!system_prompt(Stance::Safety, &[]).contains("Standing rules"));
    }

    #[test]
    fn user_message_is_stable() {
        insta::assert_snapshot!(user_message(&request()));
        let mut without = request();
        without.guidelines = None;
        assert!(!user_message(&without).contains("CLAUDE.md"));
    }

    #[test]
    fn long_diffs_are_cut_with_a_marker() {
        let mut long = request();
        long.diff = "é".repeat(MAX_DIFF_BYTES);
        let text = user_message(&long);
        assert!(text.contains("[diff truncated at 300 KB; "));
        assert!(text.len() < MAX_DIFF_BYTES + 1024);
    }

    #[test]
    fn retry_message_quotes_the_problem() {
        let text = retry_message("invalid review JSON: expected `,` at line 3");
        assert!(text.contains("expected `,` at line 3"));
        insta::assert_snapshot!(text);
    }
}
