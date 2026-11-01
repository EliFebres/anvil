//! Terminal implementations of the core's prompt hooks: tool permissions
//! and plan approval.

use std::io::{BufRead, Write};

use anvil_core::agent::{PlanDecision, PlanGate};
use anvil_core::tools::{PermissionPrompt, PermissionRequest, PromptAnswer};

/// Asks `y / n / always` on the terminal for each `ask` decision.
#[derive(Debug, Clone, Copy, Default)]
pub struct TerminalPrompt;

impl PermissionPrompt for TerminalPrompt {
    fn ask(&self, request: &PermissionRequest) -> PromptAnswer {
        let detail = request
            .target
            .clone()
            .unwrap_or_else(|| request.arguments.to_string());
        loop {
            let answer = read_line(&format!(
                "\n? {} {}\n  allow? [y]es / [n]o / [a]lways: ",
                request.tool, detail
            ));
            match answer.trim().to_ascii_lowercase().as_str() {
                "y" | "yes" => return PromptAnswer::Yes,
                "n" | "no" | "" => return PromptAnswer::No,
                "a" | "always" => return PromptAnswer::Always,
                _ => eprintln!("  please answer y, n or a"),
            }
        }
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
