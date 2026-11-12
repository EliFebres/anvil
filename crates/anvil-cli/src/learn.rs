//! Learning from the owner's corrections.
//!
//! A session that changes files records them with a hash and a copy of
//! their text ([`record_session_end`]). The next session in the project,
//! and `anvil memory learn`, look for recorded files the owner has edited
//! since, ask the summarizer which standing rules the edits suggest, and
//! put each proposal through the usual keep/drop review. Nothing is
//! written to `rules.toml` until a proposal is kept.
//!
//! The `[memory] always_summarize` setting does not apply: it decides
//! whether an uneventful session is worth a summarizer turn, while this step
//! only calls the model when the owner really edited something.

use std::path::Path;
use std::sync::Arc;

use anvil_core::agent::diff;
use anvil_core::context::{self, collect_text};
use anvil_core::provider::{CompletionRequest, Message, Provider};
use anvil_memory::corrections::{Record, Skip};
use anvil_memory::facts::extract::{Decision, Proposal, write_proposed};
use anvil_memory::rule_proposals::{
    LEARN_PROMPT, filter_new_rules, learn_user_message, parse_rule_proposals, review_rules,
};
use anvil_memory::{FactStore, paths};
use anyhow::Context;

use crate::console::Console;
use crate::frontend::Frontend;
use crate::memory;
use crate::telemetry::SessionTelemetry;
use crate::wiring::Wiring;

/// What one learning pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Learned {
    /// Recorded files the owner edited since anvil left them.
    pub edited: usize,
    /// Rules proposed after dropping restatements.
    pub proposed: usize,
    /// Rules the owner kept.
    pub kept: usize,
}

/// Finds the owner's edits and proposes rules from them.
///
/// The summarizer is built only when there is something to ask, so a
/// session where nothing was edited makes no model call and needs no key.
/// An edit leaves the record once its proposals were decided; when the
/// model call or a write fails, the record is untouched and the edit is
/// found again next time.
///
/// # Arguments
///
/// * `make_summarizer` - Builds the `summarizer` role's provider.
/// * `home` - The home directory.
/// * `cwd` - The project root.
/// * `decide` - Called once per proposed rule, in order.
/// * `console` - Where progress lines go.
///
/// # Returns
///
/// What the pass did.
///
/// # Errors
///
/// When the record or rules file is unreadable, the provider cannot be
/// built, the completion fails or a file cannot be written.
pub async fn propose_from_corrections(
    make_summarizer: &dyn Fn() -> anyhow::Result<Arc<dyn Provider>>,
    home: &Path,
    cwd: &Path,
    decide: &mut dyn FnMut(&Proposal) -> Decision,
    console: &Console,
) -> anyhow::Result<Learned> {
    let record_path = paths::corrections_path(home, cwd);
    let mut record = Record::load(&record_path)?;
    let corrections = record.take_corrections(cwd, &Skip::for_project(cwd));
    if corrections.is_empty() {
        record.save(&record_path)?;
        return Ok(Learned::default());
    }
    let mut learned = Learned {
        edited: corrections.len(),
        ..Learned::default()
    };
    let mut rules = memory::open_rules(home)?;
    let summarizer = make_summarizer()?;
    let request = CompletionRequest {
        system: LEARN_PROMPT.to_owned(),
        messages: vec![Message::user(learn_user_message(
            &corrections,
            &rules.numbered_list(),
        ))],
        tools: Vec::new(),
        max_output_tokens: None,
    };
    console.notice(format!(
        "memory: you edited {} file(s) anvil wrote; asking the summarizer for rules",
        corrections.len()
    ));
    let (reply, _) = collect_text(&*summarizer, request)
        .await
        .context("rule-learning completion")?;
    let (proposals, restated) = filter_new_rules(&rules, parse_rule_proposals(&reply));
    if restated > 0 {
        console.notice(format!(
            "memory: {restated} proposed rule(s) restate what is already a rule"
        ));
    }
    learned.proposed = proposals.len();
    if proposals.is_empty() {
        console.notice("memory: no new rules proposed from your edits");
        record.save(&record_path)?;
        return Ok(learned);
    }
    let store = FactStore::new(paths::memory_dir(home));
    let shown: Vec<Proposal> = proposals.iter().map(|p| p.to_proposal()).collect();
    let proposed_path = write_proposed(&store, &shown, "your edits")?;
    console.notice(format!(
        "memory: {} proposed rule(s), written to {}",
        proposals.len(),
        proposed_path.display()
    ));
    let kept = review_rules(&mut rules, &proposals, decide)?;
    if !kept.is_empty() {
        let rules_path = paths::rules_path(home);
        rules
            .save(&rules_path)
            .with_context(|| format!("writing {}", rules_path.display()))?;
        console.notice(format!(
            "memory: kept {} rule(s) in {}",
            kept.len(),
            rules_path.display()
        ));
    }
    learned.kept = kept.len();
    store.remove_proposed()?;
    record.save(&record_path)?;
    Ok(learned)
}

/// `anvil memory learn`: runs one learning pass with the terminal prompt.
///
/// # Arguments
///
/// * `wiring` - Providers and paths.
///
/// # Returns
///
/// Zero.
///
/// # Errors
///
/// When there is no home directory or the pass fails.
pub async fn run(wiring: &Wiring) -> anyhow::Result<i32> {
    let home = wiring
        .home
        .as_deref()
        .context("cannot determine the home directory")?;
    let name = wiring.config.roles.summarizer.clone();
    let mut decide = memory::terminal_decider();
    let learned = propose_from_corrections(
        &|| wiring.provider(&name),
        home,
        &wiring.cwd,
        &mut *decide,
        &Console::Stderr,
    )
    .await?;
    if learned.edited == 0 {
        println!("no files anvil wrote have been edited since; nothing to learn");
    }
    Ok(0)
}

/// The start-of-session pass: proposes rules from edits made since the
/// last session. It never fails the session; a problem is a warning.
///
/// Skipped under `--yes`, where nobody is there to keep or drop a
/// proposal; `anvil memory learn` picks the edits up later.
///
/// # Arguments
///
/// * `wiring` - Providers and paths.
/// * `frontend` - The session's front end, which asks keep or drop.
/// * `yes` - Whether `--yes` was given.
/// * `telemetry` - Where the summarizer's completion is recorded.
pub async fn at_session_start(
    wiring: &Wiring,
    frontend: &Frontend,
    yes: bool,
    telemetry: &SessionTelemetry,
) {
    let Some(home) = wiring.home.as_deref() else {
        return;
    };
    if yes || !paths::corrections_path(home, &wiring.cwd).is_file() {
        return;
    }
    let console = frontend.console();
    let name = wiring.config.roles.summarizer.clone();
    let make = || {
        let provider = wiring.provider(&name)?;
        Ok(telemetry.recorded(&name, provider, "memory"))
    };
    let mut decide = frontend.decider(false);
    if let Err(error) =
        propose_from_corrections(&make, home, &wiring.cwd, &mut *decide, &console).await
    {
        console.warn(format!(
            "warning: learning from your edits skipped: {error:#}"
        ));
    }
}

/// Records the files a finished run changed.
///
/// # Arguments
///
/// * `wiring` - Providers and paths.
/// * `id` - The session id.
/// * `console` - Where a warning goes.
pub async fn record_run(wiring: &Wiring, id: &str, console: &Console) {
    match context::load_session(&wiring.cwd, id) {
        Ok(snapshot) => {
            record_session_end(wiring, &snapshot.dir, snapshot.baseline.as_deref(), console).await;
        }
        Err(error) => tracing::warn!(%error, "cannot record the files anvil changed"),
    }
}

/// Records the files a session changed since its baseline, with their
/// hashes and text, so a later session can see which the owner edited.
/// Outside a git repository there is no baseline and nothing is recorded.
///
/// # Arguments
///
/// * `wiring` - Providers and paths.
/// * `session_dir` - The session folder, which holds the private git index.
/// * `baseline` - The tree hash taken when the session started.
/// * `console` - Where a warning goes.
pub async fn record_session_end(
    wiring: &Wiring,
    session_dir: &Path,
    baseline: Option<&str>,
    console: &Console,
) {
    let (Some(home), Some(baseline)) = (wiring.home.as_deref(), baseline) else {
        return;
    };
    let Some(changed) = diff::changed_files(&wiring.cwd, session_dir, baseline).await else {
        return;
    };
    let record_path = paths::corrections_path(home, &wiring.cwd);
    let mut record = Record::load(&record_path).unwrap_or_else(|error| {
        tracing::warn!(%error, "starting a fresh corrections record");
        Record::default()
    });
    if record.remember(&wiring.cwd, &changed, &Skip::for_project(&wiring.cwd)) == 0 {
        return;
    }
    if let Err(error) = record.save(&record_path) {
        console.warn(format!(
            "warning: cannot record the files anvil changed: {error}"
        ));
    }
}

#[cfg(test)]
mod tests {
    use anvil_core::provider::{FakeProvider, ProviderError};
    use anvil_memory::RuleSet;
    use anvil_memory::corrections::{MAX_DIFF_BYTES, MAX_FILE_BYTES};

    use super::*;

    struct Fixture {
        home: tempfile::TempDir,
        cwd: tempfile::TempDir,
    }

    impl Fixture {
        /// A project where anvil "left" `a.py` and the record knows it.
        fn new() -> Self {
            let fixture = Self {
                home: tempfile::tempdir().expect("home"),
                cwd: tempfile::tempdir().expect("cwd"),
            };
            fixture.write("a.py", "def f(x):\n    return x\n");
            fixture.write("Cargo.lock", "# lock\n");
            fixture.record(&["a.py", "Cargo.lock"]);
            fixture
        }

        fn write(&self, relative: &str, text: &str) {
            std::fs::write(self.cwd.path().join(relative), text).expect("write");
        }

        fn record(&self, files: &[&str]) {
            let changed: Vec<_> = files.iter().map(|f| self.cwd.path().join(f)).collect();
            let skip = Skip::for_project(self.cwd.path());
            let path = paths::corrections_path(self.home.path(), self.cwd.path());
            let mut record = Record::load(&path).expect("loads");
            record.remember(self.cwd.path(), &changed, &skip);
            record.save(&path).expect("saves");
        }

        fn rules(&self) -> Option<RuleSet> {
            RuleSet::load(&paths::rules_path(self.home.path())).ok()
        }

        async fn learn(
            &self,
            reply: &str,
            decision: Decision,
        ) -> (Learned, Arc<FakeProvider>, Vec<String>) {
            let fake = Arc::new(FakeProvider::new(vec![FakeProvider::text_turn(reply)]));
            let provider = Arc::clone(&fake);
            let mut shown = Vec::new();
            let mut decide = |proposal: &Proposal| {
                shown.push(proposal.fact.clone());
                decision
            };
            let learned = propose_from_corrections(
                &move || Ok(Arc::clone(&provider) as Arc<dyn Provider>),
                self.home.path(),
                self.cwd.path(),
                &mut decide,
                &Console::Stderr,
            )
            .await
            .expect("learns");
            (learned, fake, shown)
        }
    }

    #[tokio::test]
    async fn an_edited_file_produces_a_proposal_that_is_written_only_when_kept() {
        let fixture = Fixture::new();
        fixture.write("a.py", "def f(x: int) -> int:\n    return x\n");
        fixture.write("Cargo.lock", "# lock changed\n");
        let reply = "rule: python | Annotate every parameter and return type.";

        let (learned, fake, shown) = fixture.learn(reply, Decision::Drop).await;
        assert_eq!(
            learned,
            Learned {
                edited: 1,
                proposed: 1,
                kept: 0
            }
        );
        assert_eq!(
            shown,
            vec!["(python) Annotate every parameter and return type."]
        );
        assert_eq!(
            fixture.rules(),
            Some(RuleSet::seed()),
            "dropping leaves rules.toml as it was seeded"
        );
        let request = &fake.requests()[0];
        assert_eq!(request.system, LEARN_PROMPT);
        let Message::User { text } = &request.messages[0] else {
            panic!("user message");
        };
        assert!(text.contains("+def f(x: int) -> int:"), "{text}");
        assert!(!text.contains("Cargo.lock"), "{text}");

        let again = Fixture::new();
        again.write("a.py", "def f(x: int) -> int:\n    return x\n");
        let (learned, _, _) = again.learn(reply, Decision::Keep).await;
        assert_eq!(learned.kept, 1);
        let rules = again.rules().expect("rules.toml written on keep");
        let kept = rules.rules.last().expect("the new rule");
        assert_eq!(kept.languages, vec!["python"]);
        assert!(kept.rule.starts_with("Annotate every parameter"));
    }

    #[tokio::test]
    async fn a_processed_correction_is_not_proposed_again_and_no_change_means_no_call() {
        let fixture = Fixture::new();
        let none = |text: &str| FakeProvider::text_turn(text);
        let fake = Arc::new(FakeProvider::new(vec![none("none")]));
        let provider = Arc::clone(&fake);
        let make = move || Ok(Arc::clone(&provider) as Arc<dyn Provider>);
        let mut decide = |_: &Proposal| Decision::Keep;

        let learned = propose_from_corrections(
            &make,
            fixture.home.path(),
            fixture.cwd.path(),
            &mut decide,
            &Console::Stderr,
        )
        .await
        .expect("learns");
        assert_eq!(learned, Learned::default());
        assert_eq!(fake.calls(), 0, "nothing was edited");

        fixture.write("a.py", "def f(x):\n    return x + 1\n");
        let learned = propose_from_corrections(
            &make,
            fixture.home.path(),
            fixture.cwd.path(),
            &mut decide,
            &Console::Stderr,
        )
        .await
        .expect("learns");
        assert_eq!((learned.edited, learned.proposed), (1, 0));
        assert_eq!(fake.calls(), 1);
        let learned = propose_from_corrections(
            &make,
            fixture.home.path(),
            fixture.cwd.path(),
            &mut decide,
            &Console::Stderr,
        )
        .await
        .expect("learns");
        assert_eq!(learned.edited, 0, "the same edit is not learned twice");
        assert_eq!(fake.calls(), 1);
    }

    #[tokio::test]
    async fn a_failed_completion_keeps_the_correction_for_next_time() {
        let fixture = Fixture::new();
        fixture.write("a.py", "def f(x):\n    return 2\n");
        let failing = vec![Err(ProviderError::Timeout { after_s: 1 })];
        let fake = Arc::new(FakeProvider::new(vec![failing]));
        let provider = Arc::clone(&fake);
        let mut decide = |_: &Proposal| Decision::Keep;
        let result = propose_from_corrections(
            &move || Ok(Arc::clone(&provider) as Arc<dyn Provider>),
            fixture.home.path(),
            fixture.cwd.path(),
            &mut decide,
            &Console::Stderr,
        )
        .await;
        assert!(result.is_err());
        let (learned, _, _) = fixture.learn("none", Decision::Keep).await;
        assert_eq!(learned.edited, 1, "found again after the failure");
    }

    #[tokio::test]
    async fn the_diff_sent_to_the_model_is_capped() {
        let fixture = Fixture::new();
        let big = (0..2_000)
            .map(|index| format!("line {index} of an edit\n"))
            .collect::<String>();
        assert!(big.len() > MAX_DIFF_BYTES && big.len() < MAX_FILE_BYTES * 4);
        fixture.write("big.txt", "start\n");
        fixture.record(&["big.txt"]);
        fixture.write("big.txt", &big);
        let (learned, fake, _) = fixture.learn("none", Decision::Keep).await;
        assert_eq!(learned.edited, 1);
        let Message::User { text } = &fake.requests()[0].messages[0] else {
            panic!("user message");
        };
        let sent = text.split("# Owner's edits").nth(1).expect("edits section");
        assert!(sent.len() <= MAX_DIFF_BYTES + 100, "{} bytes", sent.len());
        assert!(sent.contains("(diff cut)"));
    }
}
