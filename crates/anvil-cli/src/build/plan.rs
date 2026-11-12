//! The build plan: what the planner is asked, how its reply is parsed and
//! validated, and how a work item becomes the task of one session.

use anvil_core::agent::verdict::parse_verdict;
use anvil_core::provider::{CompletionRequest, Message};
use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

/// Most work items a plan may have; a longer list is asked for again.
pub const MAX_ITEMS: usize = 30;

/// A specification up to this many characters goes to every session whole;
/// a longer one is cut down to the section the planner named for the item.
pub const SPEC_WHOLE_CAP: usize = 24_000;

/// Longest summary of the finished items a session gets, in characters.
pub const SUMMARY_CAP: usize = 1_500;

/// Attempts an item gets before the build stops and asks the owner.
pub const MAX_ATTEMPTS: u32 = 2;

/// Longest "one line" about a finished item in the summary, in characters.
const ONE_LINE_CAP: usize = 100;

/// Longest slug in an item's file name.
const SLUG_CAP: usize = 40;

/// The planner's standing instructions.
pub const PLANNER_SYSTEM: &str = "\
You are the planner of a long software build. You read a specification and split it into an \
ordered list of work items. A separate coding session with no memory of the others works on \
each item in turn, so every item must make sense on its own and may rely only on the items \
before it.

Rules:
- Order the items so each one builds on the earlier ones; the first item can start from the \
  repository as it is.
- Make each item small enough for one focused session and big enough to be worth a commit.
- Every item has an acceptance command: one shell command, runnable from the repository root on \
  Linux and Windows, that exits 0 only when the item is truly finished (a test run, a build, a \
  script). Never use a command that always succeeds.
- Do not write the code yourself and do not ask questions.";

/// One unit of a build: a task for one session and the command that proves it done.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkItem {
    /// A short imperative title; it becomes the commit message.
    pub title: String,
    /// What to build, in a few sentences.
    #[serde(default)]
    pub description: String,
    /// The files or area of the project the item concerns.
    #[serde(default)]
    pub area: String,
    /// A shell command that must exit 0 when the item is done.
    pub acceptance: String,
    /// The heading of the specification section that matters for the item;
    /// only used when the specification is longer than [`SPEC_WHOLE_CAP`].
    #[serde(default)]
    pub spec_section: String,
}

/// The shape of the planner's JSON reply.
#[derive(Debug, Deserialize)]
struct PlanReply {
    items: Vec<WorkItem>,
}

/// The first message to the planner.
///
/// # Arguments
///
/// * `spec` - The specification text.
///
/// # Returns
///
/// The prompt: the specification and the exact reply format.
pub fn planning_prompt(spec: &str) -> String {
    format!(
        "\
# Specification

{spec}

# Reply format

Reply with a short note and then one fenced JSON block, exactly this shape and nothing after it:

```json
{{
  \"items\": [
    {{
      \"title\": \"Add the parser\",
      \"description\": \"What to build, in a few sentences.\",
      \"area\": \"src/parser.rs, tests/parser.rs\",
      \"acceptance\": \"cargo test parser\",
      \"spec_section\": \"Heading of the specification section this item needs\"
    }}
  ]
}}
```

At least one item and at most {MAX_ITEMS}. Every item needs a title and an acceptance command."
    )
}

/// The conversation that asks for a plan.
///
/// # Arguments
///
/// * `spec` - The specification text.
/// * `earlier` - Replies and the user's answers so far, oldest first.
///
/// # Returns
///
/// The request.
pub fn planning_request(spec: &str, earlier: &[Message]) -> CompletionRequest {
    let mut messages = vec![Message::user(planning_prompt(spec))];
    messages.extend(earlier.iter().cloned());
    CompletionRequest {
        system: PLANNER_SYSTEM.to_owned(),
        messages,
        tools: Vec::new(),
        max_output_tokens: None,
    }
}

/// Reads and validates the planner's reply.
///
/// # Arguments
///
/// * `reply` - The model's text.
///
/// # Returns
///
/// The items, trimmed.
///
/// # Errors
///
/// When no JSON list is found, the list is empty or longer than
/// [`MAX_ITEMS`], or an item has no title or no acceptance command. The
/// message is written to be sent back to the model.
pub fn parse_plan(reply: &str) -> anyhow::Result<Vec<WorkItem>> {
    let parsed = parse_verdict::<PlanReply>(reply).context(
        "no fenced JSON block with an `items` list, each with a `title` and an `acceptance` string, was found",
    )?;
    let mut items = parsed.items;
    if items.is_empty() {
        bail!("the `items` list is empty");
    }
    if items.len() > MAX_ITEMS {
        bail!(
            "{} items is too many; the most allowed is {MAX_ITEMS}",
            items.len()
        );
    }
    for (index, item) in items.iter_mut().enumerate() {
        let number = index + 1;
        item.title = item.title.trim().to_owned();
        item.description = item.description.trim().to_owned();
        item.area = item.area.trim().to_owned();
        item.acceptance = item.acceptance.trim().to_owned();
        item.spec_section = item.spec_section.trim().to_owned();
        if item.title.is_empty() {
            bail!("item {number} has no title");
        }
        if item.acceptance.is_empty() {
            bail!("item {number} (`{}`) has no acceptance command", item.title);
        }
    }
    Ok(items)
}

/// The list as the owner reads it before approving.
///
/// # Arguments
///
/// * `items` - The work items.
///
/// # Returns
///
/// A numbered list with each item's area and acceptance command.
pub fn render_list(items: &[WorkItem]) -> String {
    let mut text = format!("Build plan: {} work item(s)\n", items.len());
    for (index, item) in items.iter().enumerate() {
        text.push_str(&format!("\n{}. {}\n", index + 1, item.title));
        if !item.area.is_empty() {
            text.push_str(&format!("   area: {}\n", item.area));
        }
        text.push_str(&format!("   done when: {}\n", item.acceptance));
    }
    text
}

/// The Markdown file kept for one item.
///
/// # Arguments
///
/// * `item` - The work item.
///
/// # Returns
///
/// The file's text.
pub fn render_item_file(item: &WorkItem) -> String {
    let area = if item.area.is_empty() {
        "(not named)"
    } else {
        &item.area
    };
    format!(
        "# {}\n\n{}\n\nArea: {area}\n\nAcceptance: `{}`\n",
        item.title, item.description, item.acceptance
    )
}

/// The file name of item `number` (1-based): `01-<slug>.md`.
///
/// # Arguments
///
/// * `number` - The item's position in the list.
/// * `title` - The item's title.
///
/// # Returns
///
/// The name.
pub fn item_file_name(number: usize, title: &str) -> String {
    let mut slug = String::new();
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug: String = slug.trim_matches('-').chars().take(SLUG_CAP).collect();
    let slug = slug.trim_end_matches('-');
    let slug = if slug.is_empty() { "item" } else { slug };
    format!("{number:02}-{slug}.md")
}

/// The part of the specification a session gets for an item.
///
/// # Arguments
///
/// * `spec` - The whole specification.
/// * `section` - The heading the planner named, possibly empty.
///
/// # Returns
///
/// The whole specification when it is at most [`SPEC_WHOLE_CAP`] characters;
/// otherwise the named section (up to the next heading of the same or a
/// higher level), or the start of the specification when no heading matches.
pub fn spec_for_item(spec: &str, section: &str) -> String {
    if spec.chars().count() <= SPEC_WHOLE_CAP {
        return spec.to_owned();
    }
    match named_section(spec, section) {
        Some(excerpt) => cut(&excerpt, SPEC_WHOLE_CAP),
        None => format!(
            "{}
[the specification continues; only its start is shown]",
            cut(spec, SPEC_WHOLE_CAP)
        ),
    }
}

/// The Markdown section whose heading contains `section`, case-insensitively.
fn named_section(spec: &str, section: &str) -> Option<String> {
    let wanted = section.trim().trim_start_matches('#').trim().to_lowercase();
    if wanted.is_empty() {
        return None;
    }
    let lines: Vec<&str> = spec.lines().collect();
    let start = lines.iter().position(|line| {
        heading_level(line).is_some()
            && line
                .trim_start_matches('#')
                .trim()
                .to_lowercase()
                .contains(&wanted)
    })?;
    let level = heading_level(lines[start])?;
    let end = lines[start + 1..]
        .iter()
        .position(|line| heading_level(line).is_some_and(|other| other <= level))
        .map_or(lines.len(), |offset| start + 1 + offset);
    Some(lines[start..end].join("\n"))
}

/// The level of a Markdown heading line, or `None` for any other line.
fn heading_level(line: &str) -> Option<usize> {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    (hashes > 0 && line[hashes..].starts_with(' ')).then_some(hashes)
}

/// The first `max` characters of `text`.
fn cut(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

/// A finished item as the next sessions hear of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finished<'a> {
    /// The item's title.
    pub title: &'a str,
    /// The item's description; its first line is quoted.
    pub description: &'a str,
}

/// The short summary of finished items, oldest dropped first to fit [`SUMMARY_CAP`].
///
/// # Arguments
///
/// * `finished` - The finished items, oldest first.
///
/// # Returns
///
/// One `- title: first line` per item, or an empty string for no items.
pub fn finished_summary(finished: &[Finished<'_>]) -> String {
    let lines: Vec<String> = finished
        .iter()
        .map(|item| {
            let first = item.description.lines().next().unwrap_or("").trim();
            if first.is_empty() {
                format!("- {}", item.title)
            } else {
                format!("- {}: {}", item.title, cut(first, ONE_LINE_CAP))
            }
        })
        .collect();
    let mut start = 0;
    let mut total: usize = lines.iter().map(|line| line.chars().count() + 1).sum();
    while total > SUMMARY_CAP && start + 1 < lines.len() {
        total -= lines[start].chars().count() + 1;
        start += 1;
    }
    lines[start..].join("\n")
}

/// Everything the task of one item is made from.
#[derive(Debug, Clone, Copy)]
pub struct TaskInputs<'a> {
    /// The item's position, 1-based.
    pub number: usize,
    /// How many items the build has.
    pub total: usize,
    /// The item.
    pub item: &'a WorkItem,
    /// The whole specification.
    pub spec: &'a str,
    /// The summary of the finished items.
    pub finished: &'a str,
    /// Why the previous attempt at this item failed, on a retry.
    pub previous_failure: Option<&'a str>,
}

/// The task text of one item's session.
///
/// # Arguments
///
/// * `inputs` - The item and what surrounds it.
///
/// # Returns
///
/// The task: the item, the finished summary and the specification.
pub fn item_task(inputs: TaskInputs<'_>) -> String {
    let TaskInputs {
        number,
        total,
        item,
        spec,
        finished,
        previous_failure,
    } = inputs;
    let mut task = format!("# Work item {number} of {total}: {}\n\n", item.title);
    if !item.description.is_empty() {
        task.push_str(&format!("{}\n\n", item.description));
    }
    if !item.area.is_empty() {
        task.push_str(&format!("Area: {}\n\n", item.area));
    }
    task.push_str(&format!(
        "This item is done when this command exits 0: `{}`\nMake changes only for this item; the other items are handled in their own sessions.\n\n",
        item.acceptance
    ));
    if let Some(failure) = previous_failure {
        task.push_str(&format!(
            "A previous attempt at this item failed:\n{failure}\n\n"
        ));
    }
    if !finished.is_empty() {
        task.push_str(&format!("# Already finished\n\n{finished}\n\n"));
    }
    task.push_str(&format!(
        "# Specification\n\n{}",
        spec_for_item(spec, &item.spec_section)
    ));
    task
}
