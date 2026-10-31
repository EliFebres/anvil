//! Turns a session transcript into fact proposals and applies the user's
//! keep/drop decisions.
//!
//! The model call itself happens in the CLI, which owns the providers.
//! This module builds the request text, parses the reply, filters out
//! restatements, records the proposals in `proposed.md` and appends what
//! the user kept.

use std::path::PathBuf;

use super::{FactStore, Topic, normalize};
use crate::error::MemoryError;

/// The system prompt of the `memory-extract` completion.
pub const EXTRACT_PROMPT: &str = include_str!("../../prompts/memory-extract.md");

/// One fact the extractor proposed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proposal {
    /// Where it would go.
    pub topic: Topic,
    /// The fact, one sentence.
    pub fact: String,
}

/// What the user said about a proposal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Append it to its topic file.
    Keep,
    /// Forget it.
    Drop,
}

/// The user message of the `memory-extract` completion.
///
/// # Arguments
///
/// * `transcript` - The session rendered as text.
/// * `existing` - The memory already loaded for this repository, if any.
/// * `repo` - The repository name.
///
/// # Returns
///
/// The message text.
pub fn extract_user_message(transcript: &str, existing: Option<&str>, repo: &str) -> String {
    let known = existing
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .unwrap_or("(nothing yet)");
    format!(
        "# Repository\n\n{repo}\n\n# Already known\n\n{known}\n\n# Transcript\n\n{}",
        transcript.trim_end()
    )
}

/// Parses the extractor's reply.
///
/// # Arguments
///
/// * `text` - The reply.
/// * `repo` - The repository a `project:` line refers to.
///
/// # Returns
///
/// One proposal per `tooling:`, `workflow:` or `project:` line; every
/// other line, including `none`, is ignored.
pub fn parse_proposals(text: &str, repo: &str) -> Vec<Proposal> {
    text.lines()
        .map(|line| line.trim().trim_start_matches(['-', '*']).trim())
        .filter_map(|line| {
            let (label, fact) = line.split_once(':')?;
            let topic = Topic::parse(label, repo)?;
            let fact = fact.trim();
            (!fact.is_empty()).then(|| Proposal {
                topic,
                fact: fact.to_owned(),
            })
        })
        .collect()
}

/// Drops proposals that restate a stored fact or an earlier proposal.
///
/// # Arguments
///
/// * `store` - The facts store.
/// * `proposals` - The parsed proposals.
///
/// # Returns
///
/// The new proposals in order, and how many were dropped.
pub fn filter_new(store: &FactStore, proposals: Vec<Proposal>) -> (Vec<Proposal>, usize) {
    let mut seen = std::collections::BTreeSet::new();
    let mut fresh = Vec::new();
    let mut dropped = 0;
    for proposal in proposals {
        let key = (proposal.topic.label(), normalize(&proposal.fact));
        if store.contains(&proposal.topic, &proposal.fact) || !seen.insert(key) {
            dropped += 1;
            continue;
        }
        fresh.push(proposal);
    }
    (fresh, dropped)
}

/// Writes the proposals to `proposed.md`, replacing any earlier file.
///
/// # Arguments
///
/// * `store` - The facts store.
/// * `proposals` - The proposals, in order.
/// * `session_id` - The session they came from.
///
/// # Returns
///
/// The file's path.
///
/// # Errors
///
/// [`MemoryError::Io`] when the file cannot be written.
pub fn write_proposed(
    store: &FactStore,
    proposals: &[Proposal],
    session_id: &str,
) -> Result<PathBuf, MemoryError> {
    let path = store.proposed_path();
    std::fs::create_dir_all(store.root()).map_err(|error| MemoryError::io(store.root(), &error))?;
    let mut text = format!(
        "# Proposed facts from session {session_id}\n\nNothing here is remembered until it is kept. One line per proposal.\n\n"
    );
    for proposal in proposals {
        text.push_str(&format!(
            "- {}: {}\n",
            proposal.topic.label(),
            proposal.fact
        ));
    }
    std::fs::write(&path, text).map_err(|error| MemoryError::io(&path, &error))?;
    Ok(path)
}

/// Asks about each proposal and appends the kept ones to their topic files.
///
/// # Arguments
///
/// * `store` - The facts store.
/// * `proposals` - The proposals, in order.
/// * `decide` - Called once per proposal, in order.
///
/// # Returns
///
/// The kept proposals.
///
/// # Errors
///
/// [`MemoryError::Io`] when a topic file cannot be written; proposals
/// decided before the failure are already on disk.
pub fn review(
    store: &FactStore,
    proposals: &[Proposal],
    decide: &mut dyn FnMut(&Proposal) -> Decision,
) -> Result<Vec<Proposal>, MemoryError> {
    let mut kept = Vec::new();
    for proposal in proposals {
        if decide(proposal) == Decision::Keep {
            store.append(&proposal.topic, std::slice::from_ref(&proposal.fact))?;
            kept.push(proposal.clone());
        }
    }
    Ok(kept)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(fact: &str) -> Proposal {
        Proposal {
            topic: Topic::Project("anvil".to_owned()),
            fact: fact.to_owned(),
        }
    }

    #[test]
    fn prompt_snapshot() {
        insta::assert_snapshot!(EXTRACT_PROMPT);
        let message = extract_user_message("## User\nhi", None, "anvil");
        assert!(message.starts_with("# Repository\n\nanvil\n\n# Already known\n\n(nothing yet)"));
        assert!(message.ends_with("# Transcript\n\n## User\nhi"));
        let message = extract_user_message("t", Some("## Tooling\n\n- x"), "anvil");
        assert!(message.contains("# Already known\n\n## Tooling\n\n- x"));
    }

    #[test]
    fn parses_only_labelled_lines() {
        let reply = "Here you go:\n- tooling: Uses PowerShell 7.\nworkflow: Commits on dev.\nProject: Tests live next to the code.\nnone\nrandom: ignored\nproject:   \n";
        let proposals = parse_proposals(reply, "anvil");
        assert_eq!(
            proposals,
            vec![
                Proposal {
                    topic: Topic::Tooling,
                    fact: "Uses PowerShell 7.".to_owned()
                },
                Proposal {
                    topic: Topic::Workflow,
                    fact: "Commits on dev.".to_owned()
                },
                project("Tests live next to the code."),
            ]
        );
        assert!(parse_proposals("none", "anvil").is_empty());
    }

    #[test]
    fn restatements_and_duplicates_are_dropped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FactStore::new(dir.path());
        store
            .append(&Topic::Tooling, &["Uses PowerShell 7.".to_owned()])
            .expect("seed");
        let (fresh, dropped) = filter_new(
            &store,
            vec![
                Proposal {
                    topic: Topic::Tooling,
                    fact: "uses powershell 7".to_owned(),
                },
                project("Tests live next to the code."),
                project("Tests live next to the code"),
                Proposal {
                    topic: Topic::Workflow,
                    fact: "Tests live next to the code.".to_owned(),
                },
            ],
        );
        assert_eq!(dropped, 2);
        assert_eq!(fresh.len(), 2);
        assert_eq!(fresh[0], project("Tests live next to the code."));
        assert_eq!(fresh[1].topic, Topic::Workflow);
    }

    #[test]
    fn review_writes_exactly_the_kept_lines() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FactStore::new(dir.path().join("memory"));
        let proposals = vec![
            Proposal {
                topic: Topic::Tooling,
                fact: "Uses PowerShell 7.".to_owned(),
            },
            Proposal {
                topic: Topic::Workflow,
                fact: "Commits on dev.".to_owned(),
            },
            project("Tests live next to the code."),
        ];
        let path = write_proposed(&store, &proposals, "01S").expect("writes");
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(
            text.starts_with("# Proposed facts from session 01S"),
            "{text}"
        );
        assert!(text.ends_with("- tooling: Uses PowerShell 7.\n- workflow: Commits on dev.\n- project: Tests live next to the code.\n"), "{text}");
        assert_eq!(parse_proposals(&text, "anvil"), proposals);

        let mut decisions = vec![Decision::Keep, Decision::Drop, Decision::Keep].into_iter();
        let mut decide = |_: &Proposal| decisions.next().expect("one per proposal");
        let kept = review(&store, &proposals, &mut decide).expect("reviews");
        assert_eq!(kept.len(), 2);
        assert_eq!(store.read(&Topic::Tooling), vec!["Uses PowerShell 7."]);
        assert!(store.read(&Topic::Workflow).is_empty());
        assert!(!store.path(&Topic::Workflow).exists());
        assert_eq!(
            store.read(&Topic::Project("anvil".to_owned())),
            vec!["Tests live next to the code."]
        );
    }
}
