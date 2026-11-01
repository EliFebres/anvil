//! `factory review <pr>`: run the review swarm on a pull request.
//!
//! The aggregate verdict is printed. Minor findings are posted as one
//! `COMMENT` review, inline where the diff shows the line and in the review
//! body otherwise. Blocker and major findings are printed only, so a human
//! decides what to do with them.

use std::path::PathBuf;

use anvil_types::config;
use anyhow::Context;
use factory_cli::adapters::review_swarm::Swarm;
use factory_cli::wiring::{read_factory, repo_root};
use factory_core::GitHub;
use factory_core::worker::split_minors;
use factory_review::{Decision, ModelReviewer, ReviewRequest, TicketSummary, load_rules};

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

    let report = Swarm::report(&verdict, &request.diff);
    if !report.minors.is_empty() {
        let (comments, body) = split_minors(&report.minors);
        github
            .post_review(number, &pull.head_sha, &body, &comments)
            .await?;
        println!(
            "posted {} inline comment(s) and {} in the review body",
            comments.len(),
            report.minors.len() - comments.len()
        );
    }
    Ok(match verdict.decision {
        Decision::Approve => 0,
        Decision::Changes => CHANGES_REQUESTED,
    })
}
