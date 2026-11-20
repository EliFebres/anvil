//! System prompt assembly from the prompt files compiled into the binary.
//!
//! Order: anvil's base prompt, the active state's prompt, instruction files,
//! sections supplied from outside the loop, the tool list, then the user's
//! task. The prompt files live in `crates/anvil-core/prompts/` and are
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
        State::Verify => VERIFY,
        State::Done => "",
    }
}

/// The name of the tool that hands a task to a subagent.
pub const DELEGATE_TOOL: &str = "delegate";

/// The Plan state's advice on delegating: planning only reads.
const DELEGATE_PLAN: &str = "`delegate` hands a self-contained task to a subagent that starts with a fresh context and returns only its final message. It pays for itself on a broad search across many files: ask `explore` the question instead of reading the files yourself. It does not pay for a single file read or a quick grep: do those yourself.";

/// The Build state's advice on delegating.
const DELEGATE_BUILD: &str = "`delegate` hands a self-contained task to a subagent that starts with a fresh context and returns only its final message. It pays for itself on a broad search across many files (`explore`) and on an independent, self-contained step of the plan (`implement`); several `explore` calls in one turn run at the same time. It does not pay for a single file read or a one-line edit: do those yourself.";

/// The section that says when delegating pays off.
///
/// # Arguments
///
/// * `state` - The state the prompt is for.
/// * `tools` - The tools on offer in it.
/// * `enabled` - `[prompt] delegate_guidance`.
///
/// # Returns
///
/// The section, or `None` outside Plan and Build, when `enabled` is false,
/// or when the `delegate` tool is not among `tools`.
pub fn delegate_guidance(state: State, tools: &[ToolSpec], enabled: bool) -> Option<PromptSection> {
    if !enabled || !tools.iter().any(|tool| tool.name == DELEGATE_TOOL) {
        return None;
    }
    let text = match state {
        State::Plan => DELEGATE_PLAN,
        State::Build => DELEGATE_BUILD,
        _ => return None,
    };
    Some(PromptSection {
        heading: "Delegation".to_owned(),
        text: text.to_owned(),
    })
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
/// The CLI uses it for things anvil-core does not know about, such as the
/// user's standing rules and what anvil remembers about the repository.
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

/// The heading of the section that says where the agent runs.
pub const ENVIRONMENT_HEADING: &str = "Environment";

/// The section that tells the model where it runs: the operating system
/// family, the path separator and, when the `shell` tool is on offer, how
/// it starts commands.
///
/// It holds no user name, host name or path, and it depends only on the
/// session's configuration, so it stays byte for byte the same within a
/// session.
///
/// # Arguments
///
/// * `os` - The operating system, as `std::env::consts::OS` names it.
/// * `shell` - What [`crate::tools::shell::shell_fact`] says, or `None`
///   when the `shell` tool is not registered.
///
/// # Returns
///
/// The section.
pub fn environment_section(os: &str, shell: Option<&str>) -> PromptSection {
    let family = match os {
        "windows" => "Windows",
        "macos" => "macOS",
        "linux" => "Linux",
        other => other,
    };
    let separator = if os == "windows" {
        "Paths use `\\` as the separator; the file tools also accept `/`."
    } else {
        "Paths use `/` as the separator."
    };
    let mut text = format!("Operating system: {family}. {separator}");
    if let Some(shell) = shell {
        text.push('\n');
        text.push_str(shell);
    }
    PromptSection {
        heading: ENVIRONMENT_HEADING.to_owned(),
        text,
    }
}

/// Everything that goes into one system prompt.
#[derive(Debug, Clone)]
pub struct PromptInputs<'a> {
    /// The active state.
    pub state: State,
    /// Where the agent runs ([`environment_section`]), placed right after
    /// the base prompt so the prefix every state shares stays the same.
    pub environment: Option<&'a PromptSection>,
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
    let mut parts = vec![BASE.trim_end().to_owned()];
    parts.extend(inputs.environment.map(PromptSection::render));
    parts.push(state_prompt(inputs.state).trim_end().to_owned());
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
    use crate::tools::shell::{ShellTool, shell_fact};
    use crate::tools::{Container, Runner, Shell, Tool};

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

    /// The environment of a Windows session with the default shell, so the
    /// state snapshots show where the section goes.
    fn windows_environment() -> PromptSection {
        let fact = shell_fact(&Shell::from_config(Some("powershell")), &Runner::default());
        environment_section("windows", Some(&fact))
    }

    fn inputs(state: State, tools: &[ToolSpec], instructions: &[Instruction]) -> String {
        system_prompt(&PromptInputs {
            state,
            environment: Some(&windows_environment()),
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

    fn tool_named(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.to_owned(),
            description: "Hands off a task.".to_owned(),
            parameters: serde_json::json!({"type": "object"}),
        }
    }

    #[test]
    fn delegate_guidance_snapshots() {
        let tools = [tool_named("delegate")];
        for state in [State::Plan, State::Build] {
            let section = delegate_guidance(state, &tools, true).expect("guidance");
            insta::assert_snapshot!(format!("delegate_guidance_{state}"), section.render());
        }
    }

    #[test]
    fn delegate_guidance_needs_the_tool_the_switch_and_a_coding_state() {
        let tools = [tool_named("delegate")];
        assert!(delegate_guidance(State::Build, &tools, false).is_none());
        assert!(delegate_guidance(State::Build, &[tool_named("shell")], true).is_none());
        assert!(delegate_guidance(State::Review, &tools, true).is_none());
        assert!(delegate_guidance(State::Verify, &tools, true).is_none());
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
        let base = text.find("You are anvil").expect("base");
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
            environment: None,
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
    /// A container executor with any image; the fact does not name it.
    fn container() -> Option<Container> {
        let settings =
            serde_json::from_value(serde_json::json!({"image": "registry.local/build@sha256:00"}))
                .expect("settings");
        Some(Container::new(settings))
    }

    /// The environment section for one shell setting, as the model sees it.
    fn environment(os: &str, shell: Option<&str>, runner: &Runner) -> String {
        let fact = shell.map(|name| shell_fact(&Shell::from_config(Some(name)), runner));
        environment_section(os, fact.as_deref()).render()
    }

    #[test]
    fn environment_section_for_powershell_names_its_commands() {
        let text = environment("windows", Some("powershell"), &Runner::default());
        assert!(
            text.starts_with("# Environment\n\nOperating system: Windows."),
            "{text}"
        );
        assert!(text.contains("`\\` as the separator"), "{text}");
        assert!(
            text.contains("`powershell -NoProfile -NonInteractive -Command`"),
            "{text}"
        );
        assert!(text.contains("`Get-Content`, `Select-String`"), "{text}");
        assert!(text.contains("`cat -A`, `head`"), "{text}");
        assert!(text.contains("no `&&`"), "{text}");
        let pwsh = environment("windows", Some("pwsh"), &Runner::default());
        assert!(pwsh.contains("`pwsh -NoProfile"), "{pwsh}");
        assert!(!pwsh.contains("no `&&`"), "PowerShell 7 has &&: {pwsh}");
        insta::assert_snapshot!("environment_powershell", text);
    }

    #[test]
    fn environment_section_for_sh_is_posix() {
        let text = environment("linux", Some("sh"), &Runner::default());
        assert!(
            text.contains("Operating system: Linux. Paths use `/`"),
            "{text}"
        );
        assert!(text.contains("`sh -c`, a POSIX shell"), "{text}");
        assert!(!text.contains("PowerShell"), "{text}");
        insta::assert_snapshot!("environment_sh", text);
        let mac = environment("macos", Some("zsh"), &Runner::default());
        assert!(mac.contains("Operating system: macOS."), "{mac}");
        assert!(mac.contains("`zsh -c`, a POSIX shell"), "{mac}");
    }

    #[test]
    fn environment_section_names_no_user_host_or_home() {
        for (os, configured) in [
            ("windows", r"C:\Users\someone\tools\nu.exe"),
            ("linux", "/home/someone/bin/nu"),
        ] {
            let text = environment(os, Some(configured), &Runner::default());
            assert!(
                text.contains("`nu -c`, the shell the configuration names"),
                "{text}"
            );
            assert!(!text.contains("someone"), "no user name: {text}");
            assert!(
                !text.contains("Users") && !text.contains("/home"),
                "no home path: {text}"
            );
        }
        let cmd = environment(
            "windows",
            Some(r"C:\Windows\System32\cmd.exe"),
            &Runner::default(),
        );
        assert!(cmd.contains("`cmd /C` (Command Prompt)"), "{cmd}");
        assert!(!cmd.contains("System32"), "{cmd}");
        insta::assert_snapshot!(
            "environment_custom",
            environment("linux", Some("/home/someone/bin/nu"), &Runner::default())
        );
    }

    #[test]
    fn environment_section_follows_each_hardening_shell_mode() {
        // ask: the configured shell on the host, as above.
        let ask = environment("windows", Some("powershell"), &Runner::default());
        assert!(ask.contains("(PowerShell)"), "{ask}");

        // allowlist: no shell interpreter, so no pipes or redirection.
        let direct = Runner {
            direct: true,
            container: None,
        };
        let allowlist = environment("windows", Some("powershell"), &direct);
        assert!(allowlist.contains("no shell interpreter"), "{allowlist}");
        assert!(
            allowlist.contains("pipes (`|`), redirection"),
            "{allowlist}"
        );
        assert!(!allowlist.contains("PowerShell"), "{allowlist}");
        insta::assert_snapshot!("environment_allowlist", allowlist);

        // off: the section says nothing about a shell.
        let off = environment_section("windows", None).render();
        assert_eq!(
            off,
            "# Environment\n\nOperating system: Windows. Paths use `\\` as the separator; the file tools also accept `/`."
        );
        assert!(!off.to_lowercase().contains("shell"), "{off}");

        // The container executor runs `sh -c` in Linux whatever the host.
        let contained = Runner {
            direct: false,
            container: container(),
        };
        let text = environment("windows", Some("powershell"), &contained);
        assert!(text.contains("`sh -c` inside a Linux container"), "{text}");
        assert!(!text.contains("(PowerShell)"), "{text}");
        assert!(
            !text.contains("registry.local"),
            "the image is not told: {text}"
        );
        insta::assert_snapshot!("environment_container", text);
        let both = Runner {
            direct: true,
            container: container(),
        };
        let text = environment("linux", Some("sh"), &both);
        assert!(text.contains("no shell interpreter"), "{text}");
        assert!(text.contains("Linux container"), "{text}");
    }

    #[test]
    fn the_shell_tool_says_what_the_environment_section_says() {
        let shell = Shell::from_config(Some("powershell"));
        let runner = Runner::default();
        let fact = shell_fact(&shell, &runner);
        let tool = ShellTool::for_context(&shell, &runner);
        assert!(tool.description().contains(&fact), "{}", tool.description());
        assert_eq!(
            shell_fact(&shell, &runner),
            fact,
            "the same text every time"
        );
        insta::assert_snapshot!("shell_tool_description_powershell", tool.description());
    }
}
