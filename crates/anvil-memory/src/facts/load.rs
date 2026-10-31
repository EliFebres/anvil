//! Renders the facts for the system prompt under a token cap.
//!
//! Three topics are loaded: tooling, workflow and the current repository.
//! When they exceed the cap, the oldest project facts go first, then the
//! oldest workflow facts, then tooling, and the drop is logged.

use super::{FactStore, Topic};

/// Tokens the loaded memory may take in the system prompt.
pub const MEMORY_TOKEN_CAP: u64 = 2000;

/// Rough characters per token. The same estimate anvil-core uses; it is
/// repeated here because this crate must not depend on anvil-core.
const CHARS_PER_TOKEN: u64 = 4;

/// What the loader produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedMemory {
    /// The section body, with one `## ` heading per non-empty topic.
    pub text: String,
    /// Facts left out to fit the cap.
    pub dropped: usize,
}

/// Rough token count of a text.
///
/// # Arguments
///
/// * `text` - The text.
///
/// # Returns
///
/// `len / 4`, rounded up.
pub fn estimate_tokens(text: &str) -> u64 {
    (text.len() as u64).div_ceil(CHARS_PER_TOKEN)
}

/// Loads tooling, workflow and the repository's facts for the prompt.
///
/// # Arguments
///
/// * `store` - The facts store.
/// * `repo` - The repository name, from `paths::repo_name`.
/// * `cap_tokens` - The budget; [`MEMORY_TOKEN_CAP`] in anvil.
///
/// # Returns
///
/// The rendered section, or `None` when every topic is empty.
pub fn load_memory(store: &FactStore, repo: &str, cap_tokens: u64) -> Option<LoadedMemory> {
    let mut tooling = store.read(&Topic::Tooling);
    let mut workflow = store.read(&Topic::Workflow);
    let mut project = store.read(&Topic::Project(repo.to_owned()));
    if tooling.is_empty() && workflow.is_empty() && project.is_empty() {
        return None;
    }
    let mut dropped = 0;
    let mut text = render(&tooling, &workflow, &project, repo);
    while estimate_tokens(&text) > cap_tokens {
        let victim = [&mut project, &mut workflow, &mut tooling]
            .into_iter()
            .find(|facts| !facts.is_empty());
        let Some(victim) = victim else {
            break;
        };
        victim.remove(0);
        dropped += 1;
        text = render(&tooling, &workflow, &project, repo);
    }
    if dropped > 0 {
        tracing::warn!(
            dropped,
            cap_tokens,
            "memory exceeds the prompt cap; the oldest facts were left out"
        );
    }
    Some(LoadedMemory { text, dropped })
}

/// One `## ` section per non-empty topic, facts as bullets.
fn render(tooling: &[String], workflow: &[String], project: &[String], repo: &str) -> String {
    let mut sections = Vec::new();
    for (heading, facts) in [
        ("Tooling".to_owned(), tooling),
        ("Workflow".to_owned(), workflow),
        (format!("This repo ({repo})"), project),
    ] {
        if facts.is_empty() {
            continue;
        }
        let bullets = facts
            .iter()
            .map(|fact| format!("- {fact}"))
            .collect::<Vec<_>>()
            .join("\n");
        sections.push(format!("## {heading}\n\n{bullets}"));
    }
    sections.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with(
        dir: &std::path::Path,
        tooling: usize,
        workflow: usize,
        project: usize,
    ) -> FactStore {
        let store = FactStore::new(dir.join("memory"));
        let facts = |prefix: &str, count: usize| -> Vec<String> {
            (0..count)
                .map(|i| format!("{prefix} fact number {i} with some padding text"))
                .collect()
        };
        store
            .append(&Topic::Tooling, &facts("tooling", tooling))
            .expect("tooling");
        store
            .append(&Topic::Workflow, &facts("workflow", workflow))
            .expect("workflow");
        store
            .append(
                &Topic::Project("anvil".to_owned()),
                &facts("project", project),
            )
            .expect("project");
        store
    }

    #[test]
    fn under_the_cap_everything_loads_in_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_with(dir.path(), 2, 1, 2);
        let loaded = load_memory(&store, "anvil", MEMORY_TOKEN_CAP).expect("loaded");
        assert_eq!(loaded.dropped, 0);
        let tooling = loaded.text.find("## Tooling").expect("tooling");
        let workflow = loaded.text.find("## Workflow").expect("workflow");
        let project = loaded.text.find("## This repo (anvil)").expect("project");
        assert!(tooling < workflow && workflow < project, "{}", loaded.text);
        assert!(loaded.text.contains("- project fact number 1"));
        assert_eq!(
            load_memory(&store, "other", MEMORY_TOKEN_CAP).map(|m| m.dropped),
            Some(0)
        );
        let empty = FactStore::new(dir.path().join("nothing"));
        assert_eq!(load_memory(&empty, "anvil", MEMORY_TOKEN_CAP), None);
    }

    #[test]
    fn over_the_cap_drops_oldest_project_facts_before_tooling() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = store_with(dir.path(), 3, 2, 10);
        // Each fact is about 11 tokens; 60 tokens keeps roughly five lines.
        let loaded = load_memory(&store, "anvil", 60).expect("loaded");
        assert!(loaded.dropped >= 10, "dropped {}", loaded.dropped);
        assert!(
            loaded.text.contains("tooling fact number 0"),
            "{}",
            loaded.text
        );
        assert!(!loaded.text.contains("## This repo"), "{}", loaded.text);
        assert!(estimate_tokens(&loaded.text) <= 60);

        // A gentler cap keeps the newest project facts and all of tooling.
        let loaded = load_memory(&store, "anvil", 130).expect("loaded");
        assert!(
            loaded.dropped > 0 && loaded.dropped < 10,
            "dropped {}",
            loaded.dropped
        );
        assert!(
            loaded.text.contains("project fact number 9"),
            "{}",
            loaded.text
        );
        assert!(
            !loaded.text.contains("project fact number 0"),
            "{}",
            loaded.text
        );
        assert!(loaded.text.contains("tooling fact number 2"));
        assert!(loaded.text.contains("workflow fact number 1"));
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
    }
}
