//! The GitHub calls `factory review` makes; the only module that knows octocrab.

use anyhow::Context;
use octocrab::Octocrab;
use serde::{Deserialize, Serialize};

/// The environment variable that holds the token.
pub const TOKEN_ENV: &str = "GITHUB_TOKEN";

/// Overrides the API base URL; tests point it at a mock server.
pub const API_ENV: &str = "FACTORY_GITHUB_API";

/// Where the real API lives.
const DEFAULT_API: &str = "https://api.github.com";

/// Files fetched per page; GitHub's maximum.
const FILES_PER_PAGE: usize = 100;

/// One repository on GitHub, reached with one token.
pub struct GitHub {
    octocrab: Octocrab,
    owner: String,
    repo: String,
}

/// What a review needs to know about a pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pull {
    /// The title.
    pub title: String,
    /// The body; empty when GitHub has none.
    pub body: String,
    /// The head commit, which a review must name.
    pub head_sha: String,
}

/// One inline comment on the right side of the diff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InlineComment {
    /// Repository-relative path.
    pub path: String,
    /// Line in the new version of the file.
    pub line: u32,
    /// The comment text.
    pub body: String,
    /// Always `RIGHT`: comments go on the new version.
    pub side: &'static str,
}

/// The fields of `GET /repos/{owner}/{repo}/pulls/{n}` a review reads.
#[derive(Deserialize)]
struct PullJson {
    title: String,
    body: Option<String>,
    head: HeadJson,
}

#[derive(Deserialize)]
struct HeadJson {
    sha: String,
}

/// One entry of `GET /repos/{owner}/{repo}/pulls/{n}/files`.
#[derive(Deserialize)]
struct FileJson {
    filename: String,
}

impl GitHub {
    /// Builds a client from `GITHUB_TOKEN` and, when set, `FACTORY_GITHUB_API`.
    ///
    /// # Arguments
    ///
    /// * `repo` - The repository as `owner/repo`.
    ///
    /// # Returns
    ///
    /// The client.
    ///
    /// # Errors
    ///
    /// When the token is unset, the repository is not `owner/repo`, or the
    /// API URL does not parse.
    pub fn from_env(repo: &str) -> anyhow::Result<Self> {
        let token = std::env::var(TOKEN_ENV)
            .ok()
            .filter(|token| !token.trim().is_empty())
            .with_context(|| {
                format!("{TOKEN_ENV} is not set; a token with pull request access is needed")
            })?;
        let api = std::env::var(API_ENV).unwrap_or_else(|_| DEFAULT_API.to_owned());
        let (owner, name) = repo
            .split_once('/')
            .filter(|(owner, name)| !owner.is_empty() && !name.is_empty())
            .with_context(|| format!("github.repo `{repo}` must be `owner/repo`"))?;
        let octocrab = Octocrab::builder()
            .base_uri(api.as_str())
            .with_context(|| format!("{API_ENV} `{api}` is not a URL"))?
            .personal_token(token)
            .build()
            .context("building the GitHub client")?;
        Ok(Self {
            octocrab,
            owner: owner.to_owned(),
            repo: name.to_owned(),
        })
    }

    /// The pull request's title, body and head commit.
    ///
    /// # Arguments
    ///
    /// * `number` - The pull request number.
    ///
    /// # Returns
    ///
    /// The pull.
    ///
    /// # Errors
    ///
    /// When GitHub refuses or the response is not a pull request.
    pub async fn pull(&self, number: u64) -> anyhow::Result<Pull> {
        let json: PullJson = self
            .octocrab
            .get(self.route(number, ""), None::<&()>)
            .await
            .with_context(|| format!("fetching pull request #{number}"))?;
        Ok(Pull {
            title: json.title,
            body: json.body.unwrap_or_default(),
            head_sha: json.head.sha,
        })
    }

    /// The pull request's unified diff.
    ///
    /// # Arguments
    ///
    /// * `number` - The pull request number.
    ///
    /// # Returns
    ///
    /// The diff text.
    ///
    /// # Errors
    ///
    /// When GitHub refuses.
    pub async fn diff(&self, number: u64) -> anyhow::Result<String> {
        self.octocrab
            .pulls(&self.owner, &self.repo)
            .get_diff(number)
            .await
            .with_context(|| format!("fetching the diff of pull request #{number}"))
    }

    /// Every file the pull request touches, across all pages.
    ///
    /// # Arguments
    ///
    /// * `number` - The pull request number.
    ///
    /// # Returns
    ///
    /// Repository-relative paths in GitHub's order.
    ///
    /// # Errors
    ///
    /// When GitHub refuses.
    pub async fn changed_files(&self, number: u64) -> anyhow::Result<Vec<String>> {
        let mut files = Vec::new();
        for page in 1.. {
            let batch: Vec<FileJson> = self
                .octocrab
                .get(
                    self.route(number, "/files"),
                    Some(&[
                        ("per_page", FILES_PER_PAGE.to_string()),
                        ("page", page.to_string()),
                    ]),
                )
                .await
                .with_context(|| format!("fetching the files of pull request #{number}"))?;
            let count = batch.len();
            files.extend(batch.into_iter().map(|file| file.filename));
            if count < FILES_PER_PAGE {
                break;
            }
        }
        Ok(files)
    }

    /// Posts one review with `COMMENT` as its event, so it never approves or blocks.
    ///
    /// # Arguments
    ///
    /// * `number` - The pull request number.
    /// * `head_sha` - The commit the review is attached to.
    /// * `body` - The review body.
    /// * `comments` - Inline comments on lines the diff shows.
    ///
    /// # Returns
    ///
    /// Nothing; the review's own id is not needed.
    ///
    /// # Errors
    ///
    /// When GitHub refuses, for example when a comment names a line outside the diff.
    pub async fn post_review(
        &self,
        number: u64,
        head_sha: &str,
        body: &str,
        comments: &[InlineComment],
    ) -> anyhow::Result<()> {
        let payload = serde_json::json!({
            "commit_id": head_sha,
            "event": "COMMENT",
            "body": body,
            "comments": comments,
        });
        let _: serde_json::Value = self
            .octocrab
            .post(self.route(number, "/reviews"), Some(&payload))
            .await
            .with_context(|| format!("posting the review on pull request #{number}"))?;
        Ok(())
    }

    /// `/repos/{owner}/{repo}/pulls/{number}{suffix}`.
    fn route(&self, number: u64, suffix: &str) -> String {
        format!("/repos/{}/{}/pulls/{number}{suffix}", self.owner, self.repo)
    }
}
