//! Text the worker writes: the task anvil receives, the pull request body
//! and the rejection notes.

use anvil_types::Ticket;

use crate::ports::GateCost;

/// The standing instruction every task ends with.
pub const INSTRUCTION: &str = "Add or update tests for every acceptance item; do not touch files outside the crates the spec names unless a shared type in anvil-types must change, and if so say why in the PR body.";

/// The line every task ends with: Gate A runs `just check`, so the
/// implementer should run it first.
pub const CHECK_INSTRUCTION: &str = "Before finishing, run `just check` and fix anything it reports; the factory rejects a branch that fails it.";

/// Lines of command output kept in a task or a note.
pub const TAIL_LINES: usize = 80;

/// The ticket rendered as the implementer's task.
///
/// # Arguments
///
/// * `ticket` - The ticket.
///
/// # Returns
///
/// Markdown: title, symptom, reproduction, spec, the acceptance checklist,
/// [`INSTRUCTION`] and, last, [`CHECK_INSTRUCTION`].
pub fn task(ticket: &Ticket) -> String {
    let mut text = format!(
        "# {}\n\n## Symptom\n\n{}\n",
        ticket.title,
        ticket.symptom.trim()
    );
    if let Some(repro) = ticket.repro.as_deref().filter(|r| !r.trim().is_empty()) {
        text.push_str(&format!("\n## Reproduction\n\n{}\n", repro.trim()));
    }
    text.push_str(&format!(
        "\n## Spec\n\n{}\n\n## Acceptance\n\n",
        ticket.spec.trim()
    ));
    for item in &ticket.acceptance {
        text.push_str(&format!("- [ ] {}\n", item.trim()));
    }
    text.push_str(&format!("\n{INSTRUCTION}\n\n{CHECK_INSTRUCTION}\n"));
    text
}

/// Appends a failed `just check` to the task for the retry.
///
/// # Arguments
///
/// * `task` - The task so far.
/// * `output` - The command output; only the last [`TAIL_LINES`] lines are kept.
pub fn append_check_failure(task: &str, output: &str) -> String {
    format!(
        "{task}\n## Previous attempt failed `just check`\n\nFix these before anything else; the same checks run again.\n\n```\n{}\n```\n",
        tail(output)
    )
}

/// Appends Gate B's regressions to the task for one more implementer run.
///
/// # Arguments
///
/// * `task` - The task so far.
/// * `summary` - Gate B's comparison and the regressed attempt's output, as Markdown.
pub fn append_eval_failure(task: &str, summary: &str) -> String {
    format!(
        "{task}\n## Previous attempt failed gate B: the eval tasks regressed\n\nThe eval harness ran the branch's release `anvil` on fixed coding tasks that pass on `main`. Find out from the output below why the branch's `anvil` failed them (a task that fails with 0 tokens usually means `anvil` exits before calling the model) and fix the branch; the same tasks run again.\n\n{}\n",
        summary.trim()
    )
}

/// Appends a note that the previous attempt ran out of budget while planning.
///
/// # Arguments
///
/// * `task` - The task so far.
pub fn append_plan_retry(task: &str) -> String {
    format!(
        "{task}\n## Previous attempt ran out of budget while planning\n\nIt read too much. Find the code with `grep`, read only the lines around each hit with `read_file` offset and limit, and write the plan as soon as you know which files change.\n"
    )
}

/// The task for a fix round: a fresh session that only addresses the review findings.
///
/// It leaves out the ticket's spec and acceptance list on purpose; the change
/// is already committed, and re-reading the spec invites a re-implementation.
///
/// # Arguments
///
/// * `ticket` - The ticket, for its title.
/// * `changed_files` - The files the branch changes against `main`.
/// * `findings` - The blocking findings as Markdown.
///
/// # Returns
///
/// Markdown that ends with [`CHECK_INSTRUCTION`].
pub fn fix_task(ticket: &Ticket, changed_files: &[String], findings: &str) -> String {
    let mut text = format!(
        "# Address review findings\n\nThe change for ticket \"{}\" is already committed on this branch. It touches:\n\n",
        ticket.title
    );
    for file in changed_files {
        text.push_str(&format!("- `{file}`\n"));
    }
    text.push_str(&format!(
        "\nThe review swarm asked for changes. Address each finding below and nothing else: do not re-plan or re-implement the ticket.\n\n## Review findings\n\n{}\n\n{CHECK_INSTRUCTION}\n",
        findings.trim()
    ));
    text
}

/// How a gate ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateResult {
    /// The gate passed.
    Pass,
    /// The gate failed.
    Fail,
    /// The gate did not run because it could not apply.
    Skipped,
}

impl GateResult {
    /// The word in the pull request body.
    ///
    /// # Returns
    ///
    /// `pass`, `fail` or `skipped`.
    pub fn word(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Skipped => "skipped",
        }
    }
}

/// One gate's result for the pull request body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateRecord {
    /// `A: just check`, `B: eval`, `C: review`.
    pub name: String,
    /// How it ended.
    pub result: GateResult,
    /// One line of detail.
    pub detail: String,
}

/// Everything the pull request body shows.
#[derive(Debug, Clone, PartialEq)]
pub struct PullBody<'a> {
    /// The ticket.
    pub ticket: &'a Ticket,
    /// The gates, in the order they ran.
    pub gates: &'a [GateRecord],
    /// What each step cost, in the order they ran.
    pub costs: &'a [GateCost],
    /// The eval compare table and gate line, as Markdown.
    pub eval_summary: Option<&'a str>,
    /// Every reviewer and its verdict word.
    pub verdicts: &'a [(String, String)],
    /// Protected paths the change touches, if any.
    pub held: &'a [String],
}

/// Renders the pull request body.
///
/// The acceptance list keeps the `- [ ]` form so `factory review` can read it
/// back with `TicketSummary::from_pull`.
///
/// # Arguments
///
/// * `body` - The parts.
///
/// # Returns
///
/// Markdown.
pub fn pull_body(body: &PullBody<'_>) -> String {
    let ticket = body.ticket;
    let mut text = format!(
        "Ticket `{}` ({:?}, {:?}), opened by the factory.\n\n## Symptom\n\n{}\n",
        ticket.id,
        ticket.kind,
        ticket.priority,
        ticket.symptom.trim()
    );
    if let Some(repro) = ticket.repro.as_deref().filter(|r| !r.trim().is_empty()) {
        text.push_str(&format!("\n## Reproduction\n\n{}\n", repro.trim()));
    }
    text.push_str(&format!(
        "\n## Spec\n\n{}\n\n## Acceptance\n\n",
        ticket.spec.trim()
    ));
    for item in &ticket.acceptance {
        text.push_str(&format!("- [ ] {}\n", item.trim()));
    }
    text.push_str("\n## Gates\n\n| gate | result | detail |\n|---|---|---|\n");
    for gate in body.gates {
        text.push_str(&format!(
            "| {} | {} | {} |\n",
            gate.name,
            gate.result.word(),
            gate.detail.replace('|', "\\|")
        ));
    }
    if !body.costs.is_empty() {
        text.push_str("\n## Cost\n\n| step | cost | weighted tokens |\n|---|---|---|\n");
        for cost in body.costs {
            text.push_str(&format!(
                "| {} | {} | {} |\n",
                cost.step,
                cost.cost_usd
                    .map_or_else(|| "unknown".to_owned(), |usd| format!("${usd:.2}")),
                cost.weighted_tokens
                    .map_or_else(|| "-".to_owned(), |tokens| tokens.to_string())
            ));
        }
    }
    if let Some(summary) = body.eval_summary {
        text.push_str(&format!("\n## Eval against main\n\n{}\n", summary.trim()));
    }
    if !body.verdicts.is_empty() {
        text.push_str("\n## Reviewers\n\n| reviewer | verdict |\n|---|---|\n");
        for (reviewer, verdict) in body.verdicts {
            text.push_str(&format!("| {reviewer} | {verdict} |\n"));
        }
    }
    if !body.held.is_empty() {
        text.push_str(
            "\n## Needs a human\n\nThis change touches protected paths and is not merged automatically:\n\n",
        );
        for path in body.held {
            text.push_str(&format!("- `{path}`\n"));
        }
    }
    text
}

/// The rejection notes, built section by section as the gates run.
#[derive(Debug, Default, Clone)]
pub struct Notes {
    text: String,
}

impl Notes {
    /// Starts the notes for a ticket.
    pub fn new(ticket: &Ticket) -> Self {
        Self {
            text: format!("# {}: {}\n", ticket.id, ticket.title),
        }
    }

    /// Adds a section with a heading and preformatted output.
    ///
    /// # Arguments
    ///
    /// * `heading` - The section heading.
    /// * `output` - The output; the last [`TAIL_LINES`] lines are kept.
    pub fn section(&mut self, heading: &str, output: &str) {
        self.text
            .push_str(&format!("\n## {heading}\n\n```\n{}\n```\n", tail(output)));
    }

    /// Adds a section with Markdown text, kept whole.
    pub fn markdown(&mut self, heading: &str, text: &str) {
        self.text
            .push_str(&format!("\n## {heading}\n\n{}\n", text.trim()));
    }

    /// Adds one line.
    pub fn line(&mut self, text: &str) {
        self.text.push_str(&format!("\n{text}\n"));
    }

    /// The notes so far.
    pub fn text(&self) -> &str {
        &self.text
    }
}

/// The last [`TAIL_LINES`] lines of `text`, trimmed.
fn tail(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(TAIL_LINES);
    lines[start..].join("\n").trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use anvil_types::{Priority, TicketKind, TicketOrigin, TicketStatus};
    use ulid::Ulid;

    fn sample() -> Ticket {
        Ticket {
            id: Ulid::from_string("01J9Z6Q4K8XW2R3T5V7Y9B1D3F").expect("ulid"),
            created_at: "2026-10-30T10:00:00Z".parse().expect("rfc3339"),
            kind: TicketKind::Tool,
            title: "Add a `grep` tool with a max-results flag".to_owned(),
            symptom: "The agent reads whole files to find one symbol.".to_owned(),
            repro: Some("```\nanvil run \"find callers of foo\"\n```".to_owned()),
            spec: "Tool `grep` with `pattern`, `path`, `max_results` (default 50) in anvil-core."
                .to_owned(),
            acceptance: vec![
                "cargo test -p anvil-core passes".to_owned(),
                "the tool schema snapshot is updated".to_owned(),
            ],
            evidence: Vec::new(),
            priority: Priority::P1,
            status: TicketStatus::Claimed,
            origin: TicketOrigin::Cli,
            claimed_at: Some("2026-10-31T10:00:00Z".parse().expect("rfc3339")),
            claimed_by: Some("nuc".to_owned()),
            pr: None,
            note: None,
        }
    }

    #[test]
    fn task_ends_with_the_check_instruction() {
        let text = task(&sample());
        assert!(text.ends_with(&format!("{CHECK_INSTRUCTION}\n")), "{text}");
    }

    #[test]
    fn task_is_stable() {
        insta::assert_snapshot!(task(&sample()));
    }

    #[test]
    fn fix_task_with_a_check_failure_is_stable_and_narrow() {
        let findings =
            "- **major** (openai/correctness) `src/lib.rs:3`: `max_results` is never applied\n";
        let text = fix_task(
            &sample(),
            &["crates/anvil-core/src/lib.rs".to_owned()],
            findings,
        );
        assert!(text.ends_with(&format!("{CHECK_INSTRUCTION}\n")), "{text}");
        for left_out in [
            INSTRUCTION,
            "## Acceptance",
            "## Spec",
            "max_results` (default 50)",
        ] {
            assert!(!text.contains(left_out), "{left_out}: {text}");
        }
        let text = append_check_failure(
            &text,
            "error[E0425]: cannot find value `x`\n --> src/lib.rs:3:5\n",
        );
        insta::assert_snapshot!(text);
    }

    #[test]
    fn pull_body_is_stable_and_keeps_the_checklist() {
        let gates = [
            GateRecord {
                name: "A: just check".to_owned(),
                result: GateResult::Pass,
                detail: "passed on attempt 1".to_owned(),
            },
            GateRecord {
                name: "C: review".to_owned(),
                result: GateResult::Pass,
                detail: "approve: no blocking findings".to_owned(),
            },
            GateRecord {
                name: "B: eval".to_owned(),
                result: GateResult::Skipped,
                detail: "no changed path matches [eval_gate] paths".to_owned(),
            },
        ];
        let costs = [
            GateCost {
                step: "implementer run 1".to_owned(),
                cost_usd: Some(0.01),
                weighted_tokens: Some(1000),
            },
            GateCost {
                step: "C: review round 1".to_owned(),
                cost_usd: None,
                weighted_tokens: None,
            },
        ];
        let verdicts = [
            ("openai/correctness".to_owned(), "approve".to_owned()),
            ("google/safety".to_owned(), "approve".to_owned()),
        ];
        let held = ["deploy/nuc/Containerfile".to_owned()];
        let body = pull_body(&PullBody {
            ticket: &sample(),
            gates: &gates,
            costs: &costs,
            eval_summary: Some(
                "```\ntask          main  branch\nrs-grep       PASS  PASS\ngate: PASS (0 regressions)\n```",
            ),
            verdicts: &verdicts,
            held: &held,
        });
        insta::assert_snapshot!(body);
        assert!(body.contains("- [ ] cargo test -p anvil-core passes"));
        assert!(body.contains("| B: eval | skipped | no changed path matches [eval_gate] paths |"));
        assert!(body.contains("| implementer run 1 | $0.01 | 1000 |"));
        assert!(body.contains("| C: review round 1 | unknown | - |"));
    }

    #[test]
    fn notes_keep_the_tail_of_long_output() {
        let mut notes = Notes::new(&sample());
        let long: String = (0..200).map(|i| format!("line {i}\n")).collect();
        notes.section("implementer attempt 1", &long);
        notes.markdown("review", "- finding");
        notes.line("tokens: 10");
        let text = notes.text();
        assert!(text.starts_with("# 01J9Z6Q4K8XW2R3T5V7Y9B1D3F: Add a `grep` tool"));
        assert!(!text.contains("line 119\n"));
        assert!(text.contains("line 120\n"));
        assert!(text.contains("line 199"));
        assert!(text.contains("## review\n\n- finding"));
        assert!(text.ends_with("tokens: 10\n"));
    }
}
