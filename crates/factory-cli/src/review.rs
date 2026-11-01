//! `factory review <pr>`: run the review swarm on a pull request.
//!
//! The aggregate verdict is printed. Minor findings are posted as one
//! `COMMENT` review, inline where the diff shows the line and in the review
//! body otherwise. Blocker and major findings are printed only, so a human
//! decides what to do with them.

use std::path::PathBuf;

use anvil_types::config;
use anyhow::Context;
use factory_core::{GitHub, InlineComment};
use factory_review::{
    AttributedFinding, Decision, DiffLines, ModelReviewer, ReviewRequest, TicketSummary, Verdict,
    load_rules,
};

use factory_cli::wiring::{read_factory, repo_root};

/// Exit code when the swarm asks for changes.
pub const CHANGES_REQUESTED: i32 = 2;

/// The repository file whose text the style reviewer receives.
const GUIDELINES_FILE: &str = "CLAUDE.md";

/// What `factory review` needs.
pub struct Options {
    /// The pull request number.
    pub number: u64,
    /// The current directory; the repository root is found from it.
    pub cwd: PathBuf,
    /// The home directory holding `.anvil/config.toml` and `.anvil/rules.toml`.
    pub home: Option<PathBuf>,
}

/// Runs the swarm on the pull request and posts its minor findings.
///
/// # Arguments
///
/// * `options` - The pull request and where to find the configuration.
///
/// # Returns
///
/// `0` when the swarm approves, [`CHANGES_REQUESTED`] when it asks for changes.
///
/// # Errors
///
/// When `factory.toml` or the anvil configuration is missing or invalid, the
/// reviewers are not independent, a key or the GitHub token is unset, or
/// GitHub cannot be reached.
pub async fn run(options: Options) -> anyhow::Result<i32> {
    let root = repo_root(&options.cwd).await;
    let factory = read_factory(&root)?;
    let loaded = config::load(options.home.as_deref(), &root)?;
    factory
        .check_independence(&loaded.config)
        .context("the reviewers in factory.toml are not independent")?;
    let rules = load_rules(options.home.as_deref())?;
    let reviewers = ModelReviewer::from_config(&factory, &loaded.config, &rules)?;
    let github = GitHub::from_env(&factory.github.repo)?;

    let number = options.number;
    let (pull, diff, changed_files) = tokio::try_join!(
        github.pull(number),
        github.diff(number),
        github.changed_files(number)
    )?;
    let request = ReviewRequest {
        ticket: TicketSummary::from_pull(&pull.title, &pull.body),
        diff,
        changed_files,
        guidelines: std::fs::read_to_string(root.join(GUIDELINES_FILE)).ok(),
    };
    eprintln!(
        "reviewing {}#{number} with {} reviewer(s)",
        factory.github.repo,
        reviewers.len()
    );
    let verdict = factory_review::review(&request, &reviewers).await;
    print!("{verdict}");

    if !verdict.minors.is_empty() {
        let (comments, body) = split_minors(&verdict, &DiffLines::parse(&request.diff));
        github
            .post_review(number, &pull.head_sha, &body, &comments)
            .await?;
        println!(
            "posted {} inline comment(s) and {} in the review body",
            comments.len(),
            verdict.minors.len() - comments.len()
        );
    }
    Ok(match verdict.decision {
        Decision::Approve => 0,
        Decision::Changes => CHANGES_REQUESTED,
    })
}

/// Splits the minors into inline comments on lines the diff shows and a
/// review body listing the rest.
fn split_minors(verdict: &Verdict, lines: &DiffLines) -> (Vec<InlineComment>, String) {
    let mut comments = Vec::new();
    let mut unanchored = Vec::new();
    for minor in &verdict.minors {
        let anchored = match (&minor.finding.file, minor.finding.line) {
            (Some(file), Some(line)) if lines.contains(file, line) => Some((file.clone(), line)),
            _ => None,
        };
        match anchored {
            Some((path, line)) => comments.push(InlineComment {
                path,
                line,
                body: comment_text(minor),
                side: "RIGHT",
            }),
            None => unanchored.push(minor),
        }
    }
    let mut body = String::from("factory review: minor findings only; nothing blocks the merge.");
    if !unanchored.is_empty() {
        body.push_str("\n\nOutside the diff:\n");
        for minor in unanchored {
            body.push_str(&format!(
                "\n- `{}`: {}",
                minor.finding.location(),
                comment_text(minor)
            ));
        }
    }
    (comments, body)
}

/// `**minor** (openai/style): message` plus the suggested fix when there is one.
fn comment_text(minor: &AttributedFinding) -> String {
    let mut text = format!(
        "**{}** ({}): {}",
        minor.finding.severity,
        minor.reviewer,
        minor.finding.message.trim()
    );
    if let Some(fix) = &minor.finding.suggested_fix
        && !fix.trim().is_empty()
    {
        text.push_str(&format!("\n\nSuggested fix: {}", fix.trim()));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use anvil_types::Stance;
    use factory_review::{Finding, Outcome, RawVerdict, Review, ReviewerId, Severity, Usage};

    fn minor(file: Option<&str>, line: Option<u32>, message: &str) -> Finding {
        Finding {
            severity: Severity::Minor,
            file: file.map(str::to_owned),
            line,
            message: message.to_owned(),
            suggested_fix: Some("do x".to_owned()),
        }
    }

    #[test]
    fn minors_on_diff_lines_go_inline_and_the_rest_into_the_body() {
        let raw = vec![RawVerdict {
            reviewer: ReviewerId {
                provider: "openai".to_owned(),
                stance: Stance::Style,
                model: "m".to_owned(),
            },
            outcome: Outcome::Reviewed {
                review: Review {
                    verdict: Decision::Approve,
                    findings: vec![
                        minor(Some("a.rs"), Some(2), "inline"),
                        minor(Some("a.rs"), Some(40), "line not in diff"),
                        minor(None, None, "no location"),
                    ],
                },
            },
            usage: Usage::default(),
        }];
        let verdict = Verdict::aggregate(raw);
        let lines = DiffLines::parse(
            "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1,2 @@\n x\n+y\n",
        );
        let (comments, body) = split_minors(&verdict, &lines);
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].path, "a.rs");
        assert_eq!(comments[0].line, 2);
        assert_eq!(comments[0].side, "RIGHT");
        assert_eq!(
            comments[0].body,
            "**minor** (openai/style): inline\n\nSuggested fix: do x"
        );
        assert!(body.contains("- `a.rs:40`: **minor** (openai/style): line not in diff"));
        assert!(body.contains("- `-`: **minor** (openai/style): no location"));
        assert!(!body.contains("inline\n"));
    }
}
