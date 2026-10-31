//! System prompt assembly from the prompt files compiled into the binary.
//!
//! Order: forge's base prompt, the active state's prompt, instruction files,
//! sections supplied from outside the loop, the tool list, then the user's
//! task. The prompt files live in `crates/forge-core/prompts/` and are
//! snapshot-tested here.

use super::state::State;
use crate::provider::ToolSpec;

/// The prompt every state shares.
pub const BASE: &str = include_str!("../../prompts/base.md");

/// The Plan state prompt.
pub const PLAN: &str = include_str!("../../prompts/plan.md");

/// The Build state prompt.
pub const BUILD: &str = include_str!("../../prompts/build.md");

/// The reviewer's prompt. It stands alone; the reviewer does not get `BASE`.
pub const REVIEW: &str = include_str!("../../prompts/review.md");

/// The Optimize state prompt.
pub const OPTIMIZE: &str = include_str!("../../prompts/optimize.md");

/// The Verify state prompt, used only when no check command is detected.
pub const VERIFY: &str = include_str!("../../prompts/verify.md");

/// The prompt section for a state.
///
/// # Arguments
///
/// * `state` - The state.
///
/// # Returns
///
/// The section, empty for `Done`.
pub fn state_prompt(state: State) -> &'static str {
    match state {
        State::Plan => PLAN,
        State::Build => BUILD,
        State::Review => REVIEW,
        State::Optimize => OPTIMIZE,
        State::Verify => VERIFY,
        State::Done => "",
    }
}

/// An instruction file found on disk, such as a `CLAUDE.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instruction {
    /// Where it came from, shown as a heading.
    pub source: String,
    /// Its contents.
    pub text: String,
}

/// A section supplied from outside the loop, rendered under its own heading.
///
/// The CLI uses it for things forge-core does not know about, such as the
/// user's standing rules and what forge remembers about the repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptSection {
    /// The heading, without the `# `.
    pub heading: String,
    /// The section body.
    pub text: String,
}

impl PromptSection {
    /// `# heading` followed by the body.
    fn render(&self) -> String {
        format!("# {}\n\n{}", self.heading.trim(), self.text.trim_end())
    }
}

/// Everything that goes into one system prompt.
#[derive(Debug, Clone)]
pub struct PromptInputs<'a> {
    /// The active state.
    pub state: State,
    /// Instruction files, in the order they apply.
    pub instructions: &'a [Instruction],
    /// Sections from outside the loop, after the instruction files.
    pub sections: &'a [PromptSection],
    /// Tools on offer in this state.
    pub tools: &'a [ToolSpec],
    /// The user's task.
    pub task: &'a str,
}

/// Assembles the system prompt for a coder-side state.
///
/// # Arguments
///
/// * `inputs` - The parts.
///
/// # Returns
///
/// The prompt text.
pub fn system_prompt(inputs: &PromptInputs<'_>) -> String {
    let mut parts = vec![
        BASE.trim_end().to_owned(),
        state_prompt(inputs.state).trim_end().to_owned(),
    ];
    for instruction in inputs.instructions {
        parts.push(format!(
            "# Instructions from {}\n\n{}",
            instruction.source,
            instruction.text.trim_end()
        ));
    }
    parts.extend(inputs.sections.iter().map(PromptSection::render));
    parts.push(tool_section(inputs.tools));
    parts.push(format!("# Task\n\n{}", inputs.task.trim_end()));
    parts.join("\n\n")
}

/// Assembles the reviewer's system prompt: [`REVIEW`] plus any sections.
///
/// # Arguments
///
/// * `sections` - Sections from outside the loop, such as standing rules.
///
/// # Returns
///
/// The prompt text.
pub fn review_prompt(sections: &[PromptSection]) -> String {
    let mut parts = vec![REVIEW.trim_end().to_owned()];
    parts.extend(sections.iter().map(PromptSection::render));
    parts.join("\n\n")
}

/// Lists the tools by name and description.
fn tool_section(tools: &[ToolSpec]) -> String {
    if tools.is_empty() {
        return "# Tools\n\nNo tools are available in this state.".to_owned();
    }
    let mut text = String::from("# Tools\n\nYou can call these tools:\n");
    for tool in tools {
        text.push_str(&format!("- `{}`: {}\n", tool.name, tool.description));
    }
    text.trim_end().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tools() -> Vec<ToolSpec> {
        vec![
            ToolSpec {
                name: "read_file".to_owned(),
                description: "Reads a file.".to_owned(),
                parameters: serde_json::json!({"type": "object"}),
            },
            ToolSpec {
                name: "shell".to_owned(),
                description: "Runs a command.".to_owned(),
                parameters: serde_json::json!({"type": "object"}),
            },
        ]
    }

    fn inputs(state: State, tools: &[ToolSpec], instructions: &[Instruction]) -> String {
        system_prompt(&PromptInputs {
            state,
            instructions,
            sections: &[],
            tools,
            task: "add a --dry-run flag to the export command",
        })
    }

    #[test]
    fn plan_prompt_snapshot() {
        insta::assert_snapshot!(inputs(State::Plan, &tools()[..1], &[]));
    }

    #[test]
    fn build_prompt_snapshot() {
        let instructions = [Instruction {
            source: "CLAUDE.md".to_owned(),
            text: "Run just check before committing.\n".to_owned(),
        }];
        insta::assert_snapshot!(inputs(State::Build, &tools(), &instructions));
    }

    #[test]
    fn optimize_prompt_snapshot() {
        insta::assert_snapshot!(inputs(State::Optimize, &tools(), &[]));
    }

    #[test]
    fn verify_prompt_snapshot() {
        insta::assert_snapshot!(inputs(State::Verify, &tools(), &[]));
    }

    #[test]
    fn review_prompt_snapshot() {
        insta::assert_snapshot!(REVIEW);
    }

    #[test]
    fn sections_appear_in_order() {
        let text = inputs(State::Build, &[], &[]);
        let base = text.find("You are forge").expect("base");
        let state = text.find("# State: BUILD").expect("state");
        let tools = text.find("# Tools").expect("tools");
        let task = text.find("# Task").expect("task");
        assert!(base < state && state < tools && tools < task);
        assert!(text.contains("No tools are available"));
        assert_eq!(state_prompt(State::Done), "");
    }

    #[test]
    fn extra_sections_follow_the_instruction_files() {
        let instructions = [Instruction {
            source: "CLAUDE.md".to_owned(),
            text: "repo rules".to_owned(),
        }];
        let sections = [PromptSection {
            heading: "Standing rules".to_owned(),
            text: "1. [x] be kind\n".to_owned(),
        }];
        let text = system_prompt(&PromptInputs {
            state: State::Build,
            instructions: &instructions,
            sections: &sections,
            tools: &[],
            task: "t",
        });
        let repo = text.find("# Instructions from CLAUDE.md").expect("repo");
        let rules = text
            .find("# Standing rules\n\n1. [x] be kind\n\n# Tools")
            .expect("rules");
        assert!(repo < rules, "{text}");

        let review = review_prompt(&sections);
        assert!(review.starts_with(REVIEW.trim_end()));
        assert!(
            review.ends_with("# Standing rules\n\n1. [x] be kind"),
            "{review}"
        );
        assert_eq!(review_prompt(&[]), REVIEW.trim_end());
    }
}
