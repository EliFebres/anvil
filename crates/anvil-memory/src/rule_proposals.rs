//! Rule proposals drawn from the owner's corrections.
//!
//! The model call happens in the CLI. This module builds the request text,
//! parses the reply into proposals, drops the ones that restate an existing
//! rule and applies the owner's keep/drop decisions to a rule set. A
//! proposal is shown through the same [`Proposal`] and [`Decision`] types
//! the fact review uses, so every front end already knows how to ask.

use std::collections::BTreeSet;

use crate::MemoryError;
use crate::corrections::{Correction, cap_diffs, render_corrections};
use crate::facts::Topic;
use crate::facts::extract::{Decision, Proposal};
use crate::facts::normalize;
use crate::rules::{ANY_LANGUAGE, Rule, RuleSet, slug};

/// The system prompt of the rule-learning completion.
pub const LEARN_PROMPT: &str = include_str!("../prompts/rule-learn.md");

/// Most proposals taken from one reply.
pub const MAX_PROPOSALS: usize = 5;

/// Longest language tag accepted in a reply, in characters.
const MAX_LANGUAGE_LEN: usize = 20;

/// One rule the model proposed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleProposal {
    /// The one language the rule applies to, lowercase, or `*`.
    pub language: String,
    /// The rule, one sentence; it may end with a note on a mechanical check.
    pub rule: String,
}

impl RuleProposal {
    /// The proposal as the keep/drop prompts show it.
    ///
    /// # Returns
    ///
    /// A [`Proposal`] on the rule topic whose fact reads `(language) rule`.
    pub fn to_proposal(&self) -> Proposal {
        Proposal {
            topic: Topic::Rule(self.language.clone()),
            fact: format!("({}) {}", self.language, self.rule),
        }
    }
}

/// The user message of the rule-learning completion.
///
/// # Arguments
///
/// * `corrections` - What the owner changed, in order. Their diffs are cut
///   to the shared cap here.
/// * `existing_rules` - The rules already in force, as a numbered list.
///
/// # Returns
///
/// The message text.
pub fn learn_user_message(corrections: &[Correction], existing_rules: &str) -> String {
    let known = existing_rules.trim();
    let known = if known.is_empty() { "(none)" } else { known };
    format!(
        "# Existing rules\n\n{known}\n\n# Owner's edits\n\n{}",
        render_corrections(&cap_diffs(corrections))
    )
}

/// Parses the model's reply.
///
/// # Arguments
///
/// * `text` - The reply.
///
/// # Returns
///
/// One proposal per `rule: <language> | <sentence>` line, at most
/// [`MAX_PROPOSALS`]. Lines with another shape, an empty sentence or a
/// language that is not one lowercase word (or `*`) are ignored.
pub fn parse_rule_proposals(text: &str) -> Vec<RuleProposal> {
    text.lines()
        .map(|line| line.trim().trim_start_matches(['-', '*']).trim())
        .filter_map(|line| {
            let (label, rest) = line.split_once(':')?;
            if !label.trim().eq_ignore_ascii_case("rule") {
                return None;
            }
            let (language, rule) = rest.split_once('|')?;
            let language = language.trim().to_ascii_lowercase();
            let rule = rule.trim();
            let is_word = language == ANY_LANGUAGE
                || (!language.is_empty()
                    && language.chars().count() <= MAX_LANGUAGE_LEN
                    && language.chars().all(|c| c.is_ascii_lowercase()));
            (is_word && !rule.is_empty()).then(|| RuleProposal {
                language,
                rule: rule.to_owned(),
            })
        })
        .take(MAX_PROPOSALS)
        .collect()
}

/// Drops proposals that restate an existing rule or an earlier proposal.
///
/// # Arguments
///
/// * `rules` - The rules in force.
/// * `proposals` - The parsed proposals.
///
/// # Returns
///
/// The new proposals in order, and how many were dropped.
pub fn filter_new_rules(
    rules: &RuleSet,
    proposals: Vec<RuleProposal>,
) -> (Vec<RuleProposal>, usize) {
    let mut seen: BTreeSet<String> = rules
        .rules
        .iter()
        .map(|rule| normalize(&rule.rule))
        .collect();
    let mut fresh = Vec::new();
    let mut dropped = 0;
    for proposal in proposals {
        if seen.insert(normalize(&proposal.rule)) {
            fresh.push(proposal);
        } else {
            dropped += 1;
        }
    }
    (fresh, dropped)
}

/// Asks about each proposal and adds the kept ones to the rule set.
///
/// The caller saves the set; nothing reaches `rules.toml` here.
///
/// # Arguments
///
/// * `rules` - The rule set to add to.
/// * `proposals` - The proposals, in order.
/// * `decide` - Called once per proposal, in order.
///
/// # Returns
///
/// The ids of the rules that were added.
///
/// # Errors
///
/// A [`MemoryError`] when a kept rule is invalid, which proposals parsed by
/// [`parse_rule_proposals`] never are.
pub fn review_rules(
    rules: &mut RuleSet,
    proposals: &[RuleProposal],
    decide: &mut dyn FnMut(&Proposal) -> Decision,
) -> Result<Vec<String>, MemoryError> {
    let mut kept = Vec::new();
    for proposal in proposals {
        if decide(&proposal.to_proposal()) == Decision::Keep {
            let rule = Rule {
                id: slug(&proposal.rule),
                languages: vec![proposal.language.clone()],
                rule: proposal.rule.clone(),
                check: None,
            };
            kept.push(rules.add_unique(rule)?);
        }
    }
    Ok(kept)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_and_message_snapshots() {
        insta::assert_snapshot!("learn_prompt", LEARN_PROMPT);
        let corrections = [Correction {
            path: "src/a.py".to_owned(),
            diff: "--- anvil/src/a.py\n+++ owner/src/a.py\n@@ -1 +1 @@\n-x = 1\n+X: int = 1\n"
                .to_owned(),
        }];
        insta::assert_snapshot!(
            "learn_message",
            learn_user_message(&corrections, "1. [no-todo] (all languages) Leave no TODO.")
        );
        assert!(learn_user_message(&corrections, " ").contains("# Existing rules\n\n(none)"));
    }

    #[test]
    fn replies_parse_into_language_tagged_rules() {
        let reply = "Sure:\n- rule: Python | Annotate every parameter.\nrule: * | Keep functions short.\nrule: two words | no\nrule: rust |\nworkflow: ignored\nrule: rust | Never unwrap. Mechanical check: regex `unwrap\\(\\)`.\n";
        let proposals = parse_rule_proposals(reply);
        assert_eq!(
            proposals
                .iter()
                .map(|p| (p.language.as_str(), p.rule.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("python", "Annotate every parameter."),
                ("*", "Keep functions short."),
                (
                    "rust",
                    "Never unwrap. Mechanical check: regex `unwrap\\(\\)`."
                ),
            ]
        );
        assert!(parse_rule_proposals("none").is_empty());
        let many = "rule: go | a\n".repeat(MAX_PROPOSALS + 3);
        assert_eq!(parse_rule_proposals(&many).len(), MAX_PROPOSALS);
    }

    #[test]
    fn restatements_are_dropped_and_kept_rules_are_added() {
        let mut rules = RuleSet::default();
        rules
            .add(Rule::from_message("Annotate every parameter."))
            .expect("seed");
        let (fresh, dropped) = filter_new_rules(
            &rules,
            parse_rule_proposals(
                "rule: python | annotate every parameter\nrule: python | Prefer dataclasses.\nrule: go | Prefer dataclasses.",
            ),
        );
        assert_eq!(dropped, 2);
        assert_eq!(fresh.len(), 1);
        assert_eq!(fresh[0].to_proposal().fact, "(python) Prefer dataclasses.");

        let before = rules.clone();
        let mut drop_all = |_: &Proposal| Decision::Drop;
        let ids = review_rules(&mut rules, &fresh, &mut drop_all).expect("reviews");
        assert!(ids.is_empty());
        assert_eq!(rules, before);
        let mut keep_all = |_: &Proposal| Decision::Keep;
        let ids = review_rules(&mut rules, &fresh, &mut keep_all).expect("reviews");
        assert_eq!(ids, vec!["prefer-dataclasses"]);
        assert_eq!(rules.rules[1].languages, vec!["python"]);
        rules.validate().expect("valid");
    }
}
