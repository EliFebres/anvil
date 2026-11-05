//! The parts of a plan file anvil acts on: its steps and its check command.
//!
//! A step is a list line that carries a checkbox, as the Plan prompt asks
//! for: `1. [ ] text`. Bulleted steps (`- [ ] text`) are accepted too. Lines
//! without a checkbox are never touched. The check command is the plan's
//! `Check:` line.

/// An open checkbox.
const OPEN: &str = "[ ]";

/// A ticked checkbox.
const DONE: &str = "[x]";

/// The label that starts the plan's check line.
const CHECK_LABEL: &str = "Check:";

/// The value that defers to the check command anvil detected.
const AUTO: &str = "auto";

/// What a plan's `Check:` line asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanCheck {
    /// `Check: auto`: the detected check command is enough.
    Auto,
    /// A command line to run from the project root.
    Command(String),
    /// The plan has no `Check:` line, or the line gives no command.
    Missing,
}

/// Reads the plan's check line.
///
/// The line starts with `Check:` at the left margin; when a plan has
/// several, the last one counts. Backticks around the command are dropped.
///
/// # Arguments
///
/// * `plan` - The text of the plan file.
///
/// # Returns
///
/// What the line asks for.
pub fn check_line(plan: &str) -> PlanCheck {
    let Some(value) = plan
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix(CHECK_LABEL))
    else {
        return PlanCheck::Missing;
    };
    let value = value.trim();
    let value = value
        .strip_prefix('`')
        .and_then(|inner| inner.strip_suffix('`'))
        .unwrap_or(value)
        .trim();
    if value.is_empty() || value.starts_with("```") {
        PlanCheck::Missing
    } else if value.eq_ignore_ascii_case(AUTO) {
        PlanCheck::Auto
    } else {
        PlanCheck::Command(value.to_owned())
    }
}

/// What ticking one step did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tick {
    /// The step was open; this is the plan with its box ticked.
    Ticked(String),
    /// The step was ticked already.
    AlreadyDone,
    /// The plan has no step with that number.
    NoSuchStep,
}

/// Ticks the checkbox of one numbered step.
///
/// # Arguments
///
/// * `plan` - The text of the plan file.
/// * `step` - The step's number, as written in the plan.
///
/// # Returns
///
/// What happened, with the new text when the step was open.
pub fn tick(plan: &str, step: u32) -> Tick {
    let marker = format!("{step}. ");
    let mut offset = 0;
    for line in plan.split_inclusive('\n') {
        let indent = line.len() - line.trim_start().len();
        if let Some(rest) = line[indent..].strip_prefix(&marker) {
            let start = offset + indent + marker.len();
            if rest.starts_with(OPEN) {
                let mut ticked = plan.to_owned();
                ticked.replace_range(start..start + OPEN.len(), DONE);
                return Tick::Ticked(ticked);
            }
            if rest.starts_with(DONE) || rest.starts_with("[X]") {
                return Tick::AlreadyDone;
            }
        }
        offset += line.len();
    }
    Tick::NoSuchStep
}

/// Ticks every open step.
///
/// # Arguments
///
/// * `plan` - The text of the plan file.
///
/// # Returns
///
/// The plan with every step's box ticked; equal to `plan` when none was open.
pub fn tick_all(plan: &str) -> String {
    plan.split_inclusive('\n')
        .map(|line| match open_box(line) {
            Some(start) => format!("{}{DONE}{}", &line[..start], &line[start + OPEN.len()..]),
            None => line.to_owned(),
        })
        .collect()
}

/// Where the open checkbox of a step line starts, if the line is one.
fn open_box(line: &str) -> Option<usize> {
    let indent = line.len() - line.trim_start().len();
    let body = &line[indent..];
    let digits = body.chars().take_while(char::is_ascii_digit).count();
    let marker = if digits > 0 && body[digits..].starts_with(". ") {
        digits + 2
    } else if body.starts_with("- ") || body.starts_with("* ") {
        2
    } else {
        return None;
    };
    body[marker..].starts_with(OPEN).then_some(indent + marker)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAN: &str = "# Plan\n\nAssumption: [ ] is not a step.\n\n1. [ ] First\n2. [x] Second\n10. [ ] Tenth\n   - [ ] a sub-step\n   - a plain bullet [ ]\n";

    #[test]
    fn check_line_reads_a_command() {
        let plan = "# Plan\n\n1. [ ] Edit tool.sh\n\nCheck: bash -n tool.sh && test \"$(./tool.sh a)\" = \"a\"\n";
        assert_eq!(
            check_line(plan),
            PlanCheck::Command("bash -n tool.sh && test \"$(./tool.sh a)\" = \"a\"".to_owned())
        );
        assert_eq!(
            check_line("# Plan\n\nCheck: `grep -q '^## Usage' README.md`"),
            PlanCheck::Command("grep -q '^## Usage' README.md".to_owned())
        );
        assert_eq!(
            check_line("Check: echo `date`\n"),
            PlanCheck::Command("echo `date`".to_owned()),
            "backticks inside a command stay"
        );
        assert_eq!(
            check_line("Check: first\n\n1. [ ] step\nCheck: second\n\ntrailing prose\n"),
            PlanCheck::Command("second".to_owned()),
            "the last check line counts"
        );
    }

    #[test]
    fn check_line_reads_auto() {
        assert_eq!(
            check_line("# Plan\n\n1. [ ] a\n\nCheck: auto\n"),
            PlanCheck::Auto
        );
        assert_eq!(check_line("Check:   `Auto`  "), PlanCheck::Auto);
    }

    #[test]
    fn a_plan_without_a_check_line_has_none() {
        assert_eq!(check_line(PLAN), PlanCheck::Missing);
        assert_eq!(check_line(""), PlanCheck::Missing);
        assert_eq!(
            check_line("1. [ ] Check: run the tests\n   Check: indented\n"),
            PlanCheck::Missing,
            "a step that mentions a check is not the check line"
        );
    }

    #[test]
    fn a_malformed_check_line_counts_as_missing() {
        assert_eq!(check_line("Check:\n"), PlanCheck::Missing);
        assert_eq!(check_line("Check: ``\n"), PlanCheck::Missing);
        assert_eq!(
            check_line("Check:\n```\ncargo test\n```\n"),
            PlanCheck::Missing
        );
        assert_eq!(check_line("Check: ```sh\n"), PlanCheck::Missing);
        assert_eq!(check_line("check: cargo test\n"), PlanCheck::Missing);
    }

    #[test]
    fn tick_marks_one_numbered_step() {
        let Tick::Ticked(plan) = tick(PLAN, 1) else {
            panic!("step 1 is open");
        };
        assert!(
            plan.contains("1. [x] First\n2. [x] Second\n10. [ ] Tenth"),
            "{plan}"
        );
        assert_eq!(plan.len(), PLAN.len());
        let Tick::Ticked(plan) = tick(PLAN, 10) else {
            panic!("step 10 is open");
        };
        assert!(
            plan.contains("1. [ ] First"),
            "step 1 is not step 10: {plan}"
        );
        assert!(plan.contains("10. [x] Tenth"), "{plan}");
    }

    #[test]
    fn tick_reports_done_and_missing_steps() {
        assert_eq!(tick(PLAN, 2), Tick::AlreadyDone);
        assert_eq!(tick(PLAN, 3), Tick::NoSuchStep);
        assert_eq!(tick(PLAN, 0), Tick::NoSuchStep);
        assert_eq!(tick("", 1), Tick::NoSuchStep);
        assert_eq!(tick("1. no box here\n", 1), Tick::NoSuchStep);
    }

    #[test]
    fn tick_all_marks_every_step_and_nothing_else() {
        let plan = tick_all(PLAN);
        assert_eq!(
            plan,
            "# Plan\n\nAssumption: [ ] is not a step.\n\n1. [x] First\n2. [x] Second\n10. [x] Tenth\n   - [x] a sub-step\n   - a plain bullet [ ]\n"
        );
        assert_eq!(tick_all(&plan), plan);
        assert_eq!(
            tick_all("1. [ ] no newline at the end"),
            "1. [x] no newline at the end"
        );
    }
}
