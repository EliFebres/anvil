//! Terminal implementations of the core's prompt hooks: tool permissions
//! and plan approval.

use std::io::{BufRead, Write};

use anvil_core::agent::{PlanDecision, PlanGate};
use anvil_core::tools::{PermissionPrompt, PermissionRequest, PromptAnswer};

/// Asks `once / session / no` (plus `always` where offered) on the terminal
/// for each `ask` decision.
#[derive(Debug, Clone, Copy, Default)]
pub struct TerminalPrompt;

impl PermissionPrompt for TerminalPrompt {
    fn ask(&self, request: &PermissionRequest) -> PromptAnswer {
        let detail = request
            .target
            .clone()
            .unwrap_or_else(|| request.arguments.to_string());
        let always = request.offers_always();
        let question = prompt_text(request, &detail);
        loop {
            let answer = read_line(&question);
            match parse_answer(&answer, always) {
                Some(answer) => return answer,
                None if always => eprintln!("  please answer y, s, n or a"),
                None => eprintln!("  please answer y, s or n"),
            }
        }
    }
}

/// The whole question for one call, one choice per line so the session
/// choice can say exactly what it allows.
fn prompt_text(request: &PermissionRequest, detail: &str) -> String {
    let tool = &request.tool;
    let mut text = format!(
        "\n? {tool} {detail}\n  [y] allow once\n  [s] allow {tool} for the rest of this session (within the current workspace root)\n  [n] deny\n"
    );
    if request.offers_always() {
        text.push_str("  [a] always allow, saved as a rule in .anvil/permissions.toml\n");
    }
    text.push_str("  choice: ");
    text
}

/// Reads one prompt answer; `None` asks again.
fn parse_answer(answer: &str, always: bool) -> Option<PromptAnswer> {
    match answer.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" | "once" => Some(PromptAnswer::Yes),
        "s" | "session" => Some(PromptAnswer::Session),
        "n" | "no" | "" => Some(PromptAnswer::No),
        "a" | "always" if always => Some(PromptAnswer::Always),
        _ => None,
    }
}

/// Shows the plan and asks whether to build it.
#[derive(Debug, Clone, Copy, Default)]
pub struct TerminalGate;

impl PlanGate for TerminalGate {
    fn review(&self, plan: &str) -> PlanDecision {
        eprintln!("\n{plan}");
        loop {
            let answer = read_line("build this plan? [y]es / [r]evise / [q]uit: ");
            match answer.trim().to_ascii_lowercase().as_str() {
                "y" | "yes" | "" => return PlanDecision::Approve,
                "r" | "revise" => {
                    let feedback = read_line("what should change? ");
                    return PlanDecision::Revise(feedback.trim().to_owned());
                }
                "q" | "quit" | "n" | "no" => return PlanDecision::Abort,
                _ => eprintln!("  please answer y, r or q"),
            }
        }
    }
}

/// Prints a prompt to stderr and reads one line from stdin.
///
/// # Arguments
///
/// * `prompt` - Text shown before the cursor.
///
/// # Returns
///
/// The line without its newline; empty at end of input.
pub fn read_line(prompt: &str) -> String {
    let mut stderr = std::io::stderr();
    let _ = write!(stderr, "{prompt}");
    let _ = stderr.flush();
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_offer_session_and_always_only_where_allowed() {
        assert_eq!(parse_answer("y\n", false), Some(PromptAnswer::Yes));
        assert_eq!(parse_answer("S", false), Some(PromptAnswer::Session));
        assert_eq!(parse_answer("session", true), Some(PromptAnswer::Session));
        assert_eq!(parse_answer("", false), Some(PromptAnswer::No));
        assert_eq!(parse_answer("a", false), None);
        assert_eq!(parse_answer("a", true), Some(PromptAnswer::Always));
        assert_eq!(parse_answer("maybe", true), None);
    }

    fn request(tool: &str, target: &str) -> PermissionRequest {
        PermissionRequest {
            tool: tool.to_owned(),
            target: Some(target.to_owned()),
            arguments: serde_json::json!({}),
        }
    }

    #[test]
    fn edit_file_prompt_lists_three_explicit_choices() {
        let text = prompt_text(&request("edit_file", "src/a.rs"), "src/a.rs");
        assert_eq!(
            text,
            "\n? edit_file src/a.rs\n  [y] allow once\n  [s] allow edit_file for the rest of this session (within the current workspace root)\n  [n] deny\n  choice: "
        );
    }

    #[test]
    fn shell_prompt_adds_the_saved_rule_choice() {
        let text = prompt_text(&request("shell", "cargo test"), "cargo test");
        assert!(
            text.contains(
                "[s] allow shell for the rest of this session (within the current workspace root)"
            ),
            "{text}"
        );
        assert!(
            text.ends_with(
                "  [a] always allow, saved as a rule in .anvil/permissions.toml\n  choice: "
            ),
            "{text}"
        );
    }
}
