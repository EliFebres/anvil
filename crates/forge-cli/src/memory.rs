//! Standing rules and remembered facts: how `forge-memory` plugs into the loop.
//!
//! forge-core knows nothing about either store. This module turns them
//! into prompt sections and a [`ChangeChecks`] implementation, saves
//! `rule:` messages, and runs the end-of-session `memory-extract`
//! completion with the keep/drop review.

use std::path::{Path, PathBuf};

use anyhow::Context;
use forge_core::agent::{ChangeCheckFailure, ChangeChecks, PromptSection};
use forge_core::context::{collect_text, render_transcript};
use forge_core::provider::{CompletionRequest, Message, Provider};
use forge_memory::facts::extract::{
    EXTRACT_PROMPT, extract_user_message, filter_new, parse_proposals, review, write_proposed,
};
use forge_memory::rules::{ensure_check_scripts, strip_rule_prefix};
use forge_memory::{
    Decision, FactStore, MEMORY_TOKEN_CAP, Proposal, Rule, RuleSet, load_memory, paths, run_checks,
};

use crate::terminal::read_line;
use crate::wiring::Wiring;

/// Heading of the rules section in every system prompt.
pub const RULES_HEADING: &str = "Standing rules";

/// Heading of the facts section in the coder's system prompt.
pub const MEMORY_HEADING: &str = "What you know about this user and repo";

/// The sections built from the two stores.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PromptSections {
    /// For every coder-side state: the rules, then the facts.
    pub coder: Vec<PromptSection>,
    /// For the reviewer: the rules with the citation requirement.
    pub review: Vec<PromptSection>,
}

/// Loads `~/.forge/rules.toml`, seeding it on first use, and makes sure
/// the bundled check scripts are installed.
///
/// # Arguments
///
/// * `home` - The home directory.
///
/// # Returns
///
/// The rules.
///
/// # Errors
///
/// When the file is malformed or cannot be read or written.
pub fn open_rules(home: &Path) -> anyhow::Result<RuleSet> {
    let path = paths::rules_path(home);
    let (set, seeded) =
        RuleSet::open(&path).with_context(|| format!("loading {}", path.display()))?;
    if seeded {
        eprintln!(
            "seeded {} with {} standing rules",
            path.display(),
            set.rules.len()
        );
    }
    let checks = paths::checks_dir(home);
    ensure_check_scripts(&checks)
        .with_context(|| format!("installing check scripts under {}", checks.display()))?;
    Ok(set)
}

/// The prompt sections for a session in `cwd`.
///
/// # Arguments
///
/// * `home` - The home directory; without one there are no sections.
/// * `cwd` - The project root, which names the project's facts file.
///
/// # Returns
///
/// The sections, possibly empty.
///
/// # Errors
///
/// When the rules file is malformed.
pub fn prompt_sections(home: Option<&Path>, cwd: &Path) -> anyhow::Result<PromptSections> {
    let Some(home) = home else {
        return Ok(PromptSections::default());
    };
    let rules = open_rules(home)?;
    let mut sections = PromptSections::default();
    if let Some(text) = rules.prompt_section() {
        sections.coder.push(PromptSection {
            heading: RULES_HEADING.to_owned(),
            text,
        });
    }
    if let Some(text) = rules.review_section() {
        sections.review.push(PromptSection {
            heading: RULES_HEADING.to_owned(),
            text,
        });
    }
    let store = FactStore::new(paths::memory_dir(home));
    if let Some(loaded) = load_memory(&store, &paths::repo_name(cwd), MEMORY_TOKEN_CAP) {
        sections.coder.push(PromptSection {
            heading: MEMORY_HEADING.to_owned(),
            text: loaded.text,
        });
    }
    Ok(sections)
}

/// The rule checks, as the loop's Verify state runs them.
#[derive(Debug)]
pub struct RuleChecks {
    rules: RuleSet,
    home: PathBuf,
}

impl RuleChecks {
    /// Loads the rules from the home directory.
    ///
    /// # Arguments
    ///
    /// * `home` - The home directory.
    ///
    /// # Returns
    ///
    /// The checks.
    ///
    /// # Errors
    ///
    /// When the rules file is malformed.
    pub fn load(home: &Path) -> anyhow::Result<Self> {
        Ok(Self {
            rules: open_rules(home)?,
            home: home.to_path_buf(),
        })
    }
}

impl ChangeChecks for RuleChecks {
    fn run(&self, cwd: &Path, changed: &[PathBuf]) -> Vec<ChangeCheckFailure> {
        run_checks(&self.rules, cwd, Some(&self.home), changed)
            .into_iter()
            .map(|failure| ChangeCheckFailure {
                name: failure.rule_id,
                output: failure.output,
            })
            .collect()
    }
}

/// Saves a `rule:` message as a new standing rule.
///
/// # Arguments
///
/// * `line` - The user's message.
/// * `home` - The home directory.
///
/// # Returns
///
/// The saved rule, or `None` when the message is not a `rule:` message.
///
/// # Errors
///
/// When there is no home directory or the rules file cannot be updated.
pub fn add_rule_from_message(line: &str, home: Option<&Path>) -> anyhow::Result<Option<Rule>> {
    let Some(text) = strip_rule_prefix(line) else {
        return Ok(None);
    };
    let home = home.context("cannot determine the home directory, so the rule cannot be saved")?;
    let mut set = open_rules(home)?;
    let id = set.add_unique(Rule::from_message(text))?;
    let path = paths::rules_path(home);
    set.save(&path)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(set.get(&id).cloned())
}

/// Asks the summarizer for facts worth keeping and reviews them.
///
/// Nothing happens when there is no home directory or the session had no
/// model turn. Proposals that restate a stored fact are dropped before the
/// review; the rest are written to `proposed.md`, decided one by one, and
/// the kept ones appended to their topic files.
///
/// # Arguments
///
/// * `summarizer` - The `summarizer` role's provider.
/// * `messages` - The session's conversation.
/// * `home` - The home directory.
/// * `cwd` - The project root.
/// * `session_id` - For the `proposed.md` header.
/// * `context_window` - The coder's window, which caps the transcript.
/// * `decide` - Called once per proposal, in order.
///
/// # Errors
///
/// When the completion fails or a memory file cannot be written.
pub async fn extract_after_session(
    summarizer: &dyn Provider,
    messages: &[Message],
    home: Option<&Path>,
    cwd: &Path,
    session_id: &str,
    context_window: u32,
    decide: &mut dyn FnMut(&Proposal) -> Decision,
) -> anyhow::Result<()> {
    let Some(home) = home else {
        return Ok(());
    };
    if !messages
        .iter()
        .any(|message| matches!(message, Message::Assistant { .. }))
    {
        return Ok(());
    }
    let repo = paths::repo_name(cwd);
    let store = FactStore::new(paths::memory_dir(home));
    let existing = load_memory(&store, &repo, MEMORY_TOKEN_CAP).map(|loaded| loaded.text);
    let max_chars = usize::try_from(context_window)
        .unwrap_or(usize::MAX)
        .saturating_mul(2);
    let transcript = render_transcript(messages, max_chars);
    let request = CompletionRequest {
        system: EXTRACT_PROMPT.to_owned(),
        messages: vec![Message::user(extract_user_message(
            &transcript,
            existing.as_deref(),
            &repo,
        ))],
        tools: Vec::new(),
        max_output_tokens: None,
    };
    eprintln!("memory: asking the summarizer for facts worth keeping");
    let (reply, _) = collect_text(summarizer, request)
        .await
        .context("memory-extract completion")?;
    let (proposals, restated) = filter_new(&store, parse_proposals(&reply, &repo));
    if restated > 0 {
        eprintln!("memory: {restated} proposal(s) restate what is already known");
    }
    if proposals.is_empty() {
        eprintln!("memory: no new facts proposed");
        return Ok(());
    }
    let path = write_proposed(&store, &proposals, session_id)?;
    eprintln!(
        "memory: {} proposed fact(s), written to {}",
        proposals.len(),
        path.display()
    );
    let kept = review(&store, &proposals, decide)?;
    store.remove_proposed()?;
    if kept.is_empty() {
        eprintln!("memory: nothing kept");
    } else {
        eprintln!(
            "memory: kept {} fact(s) under {}",
            kept.len(),
            store.root().display()
        );
    }
    Ok(())
}

/// The keep/drop decider for a terminal session.
///
/// # Arguments
///
/// * `yes` - Whether `--yes` was given, which keeps everything.
///
/// # Returns
///
/// A decider. Without `--yes` it prints each proposal and asks; Enter
/// keeps, and once input ends every remaining proposal is dropped.
pub fn terminal_decider(yes: bool) -> Box<dyn FnMut(&Proposal) -> Decision> {
    if yes {
        return Box::new(|_| Decision::Keep);
    }
    let mut input_ended = false;
    Box::new(move |proposal| {
        if input_ended {
            return Decision::Drop;
        }
        loop {
            let answer = read_line(&format!(
                "[{}] {}\n  keep? [k]eep / [d]rop: ",
                proposal.topic, proposal.fact
            ));
            if answer.is_empty() {
                input_ended = true;
                return Decision::Drop;
            }
            match answer.trim().to_ascii_lowercase().as_str() {
                "k" | "keep" | "" => return Decision::Keep,
                "d" | "drop" => return Decision::Drop,
                _ => eprintln!("  please answer k or d"),
            }
        }
    })
}

/// Runs the end-of-session extraction for a finished run or chat and
/// reports, rather than fails, when it cannot.
///
/// # Arguments
///
/// * `wiring` - Providers and paths.
/// * `session_id` - The session.
/// * `messages` - The session's conversation.
/// * `yes` - Whether `--yes` was given.
pub async fn finish_session(wiring: &Wiring, session_id: &str, messages: &[Message], yes: bool) {
    let outcome = async {
        let summarizer = wiring.provider(&wiring.config.roles.summarizer)?;
        let mut decide = terminal_decider(yes);
        extract_after_session(
            &*summarizer,
            messages,
            wiring.home.as_deref(),
            &wiring.cwd,
            session_id,
            wiring.context_window(),
            &mut *decide,
        )
        .await
    }
    .await;
    if let Err(error) = outcome {
        eprintln!("warning: memory extraction skipped: {error:#}");
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use forge_core::agent::prompts::{PromptInputs, system_prompt};
    use forge_core::agent::{
        AgentLoop, AutoApprove, CheckCommand, EventSink, LoopConfig, LoopProviders, State, Status,
    };
    use forge_core::context::load_session;
    use forge_core::provider::FakeProvider;
    use forge_core::tools::{Permissions, PromptAnswer, StaticPrompt, ToolContext, ToolRegistry};
    use forge_memory::Topic;
    use forge_types::LimitsConfig;

    use super::*;

    #[test]
    fn rule_message_writes_a_valid_entry_and_the_next_prompt_contains_it() {
        let home = tempfile::tempdir().expect("home");
        let cwd = tempfile::tempdir().expect("cwd");
        assert_eq!(
            add_rule_from_message("hello there", Some(home.path())).expect("not a rule"),
            None
        );
        let rule =
            add_rule_from_message("rule: never call unwrap outside tests", Some(home.path()))
                .expect("saves")
                .expect("is a rule");
        assert_eq!(rule.id, "never-call-unwrap-outside");
        assert_eq!(rule.languages, vec!["*"]);
        assert_eq!(rule.check, None);

        let set = RuleSet::load(&paths::rules_path(home.path())).expect("reloads");
        set.validate().expect("valid");
        assert_eq!(set.rules.len(), 5, "four seeded rules plus the new one");
        assert!(
            paths::checks_dir(home.path())
                .join("docstrings.py")
                .is_file()
        );

        let sections = prompt_sections(Some(home.path()), cwd.path()).expect("sections");
        assert_eq!(sections.coder.len(), 1, "no facts yet, so only the rules");
        assert_eq!(sections.review.len(), 1);
        let prompt = system_prompt(&PromptInputs {
            state: State::Build,
            instructions: &[],
            sections: &sections.coder,
            tools: &[],
            task: "add a --dry-run flag",
        });
        insta::assert_snapshot!(prompt);
        assert!(add_rule_from_message("rule: x", None).is_err());
    }

    #[tokio::test]
    async fn keep_drop_flow_writes_exactly_the_kept_lines() {
        let home = tempfile::tempdir().expect("home");
        let cwd = tempfile::tempdir().expect("cwd");
        let repo = paths::repo_name(cwd.path());
        let store = FactStore::new(paths::memory_dir(home.path()));
        store
            .append(
                &Topic::Project(repo.clone()),
                &["Config lives in .forge/config.toml.".to_owned()],
            )
            .expect("seed a known fact");

        let plan = "# Plan\n\n1. [ ] Write hello.txt\n";
        let coder = Arc::new(FakeProvider::new(vec![
            FakeProvider::text_turn(plan),
            FakeProvider::text_turn("built"),
            FakeProvider::text_turn("nothing to simplify"),
        ]));
        let reviewer = Arc::new(FakeProvider::new(vec![FakeProvider::text_turn(
            "```json\n{\"verdict\": \"approve\"}\n```",
        )]));
        let session_dir = cwd.path().join(".forge").join("sessions").join("t1");
        let config = LoopConfig {
            task: "add a --dry-run flag".to_owned(),
            cwd: cwd.path().to_path_buf(),
            session_dir,
            limits: LimitsConfig::default(),
            check: CheckCommand::Command("exit 0".to_owned()),
            context_window: 100_000,
            home_dir: None,
            extra_instructions: Vec::new(),
            sections: Vec::new(),
            review_sections: Vec::new(),
        };
        let agent = AgentLoop::new(
            config,
            LoopProviders {
                coder: coder as Arc<dyn Provider>,
                reviewer: reviewer as Arc<dyn Provider>,
                summarizer: Arc::new(FakeProvider::new(vec![])) as Arc<dyn Provider>,
            },
            ToolRegistry::with_builtins(
                Permissions::empty(),
                Box::new(StaticPrompt(PromptAnswer::No)),
            ),
            ToolContext::new(cwd.path()),
            Box::new(AutoApprove),
            EventSink::discard(),
        );
        assert_eq!(agent.run().await.status, Status::Done);
        let messages = load_session(cwd.path(), "t1").expect("session").messages;

        let summarizer = FakeProvider::new(vec![FakeProvider::text_turn(
            "tooling: The user runs tests with just check.\n\
             workflow: The user commits on dev.\n\
             project: Tests live next to the code.\n\
             project: Config lives in .forge/config.toml.\n",
        )]);
        let mut decisions = vec![Decision::Keep, Decision::Drop, Decision::Keep].into_iter();
        let mut decide = |_: &Proposal| decisions.next().expect("one decision per proposal");
        extract_after_session(
            &summarizer,
            &messages,
            Some(home.path()),
            cwd.path(),
            "t1",
            100_000,
            &mut decide,
        )
        .await
        .expect("extracts");

        assert_eq!(
            store.read(&Topic::Tooling),
            vec!["The user runs tests with just check."]
        );
        assert!(store.read(&Topic::Workflow).is_empty());
        assert_eq!(
            store.read(&Topic::Project(repo)),
            vec![
                "Config lives in .forge/config.toml.",
                "Tests live next to the code."
            ]
        );
        assert!(!store.proposed_path().exists());
        let request = &summarizer.requests()[0];
        assert_eq!(request.system, EXTRACT_PROMPT);
        let Message::User { text } = &request.messages[0] else {
            panic!("user message");
        };
        assert!(text.contains("add a --dry-run flag"), "{text}");
        assert!(text.contains("# Already known\n\n## This repo"), "{text}");
        assert!(
            text.contains("Config lives in .forge/config.toml."),
            "{text}"
        );
    }

    #[tokio::test]
    async fn sessions_without_a_model_turn_or_a_home_are_skipped() {
        let cwd = tempfile::tempdir().expect("cwd");
        let summarizer = FakeProvider::new(vec![]);
        let mut decide = |_: &Proposal| Decision::Keep;
        extract_after_session(
            &summarizer,
            &[Message::user("hi")],
            Some(cwd.path()),
            cwd.path(),
            "t",
            1000,
            &mut decide,
        )
        .await
        .expect("skips");
        assert_eq!(summarizer.calls(), 0);
    }
}
