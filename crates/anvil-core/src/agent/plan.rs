//! The steps of a plan file: reading their checkboxes and ticking them.
//!
//! A step is a list line that carries a checkbox, as the Plan prompt asks
//! for: `1. [ ] text`. Bulleted steps (`- [ ] text`) are accepted too. Lines
//! without a checkbox are never touched.

/// An open checkbox.
const OPEN: &str = "[ ]";

/// A ticked checkbox.
const DONE: &str = "[x]";

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
