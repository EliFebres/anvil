//! GitHub through octocrab: the only module that knows the REST routes.
//!
//! One token, one repository. `factory review` reads pull requests and posts
//! comments with it; the service reads the CI status of `main` through the
//! [`CodeHost`] port.

use std::time::Duration;

use octocrab::Octocrab;
use serde::{Deserialize, Serialize};

use crate::error::{FactoryError, Result};
use crate::ports::{BoxFuture, CiStatus, CodeHost};

/// The environment variable that holds the factory's own token.
pub const TOKEN_ENV: &str = "FACTORY_GITHUB_TOKEN";

/// The personal token, used when the factory token is unset (`factory review`
/// on a laptop).
pub const FALLBACK_TOKEN_ENV: &str = "GITHUB_TOKEN";

/// Overrides the API base URL; tests point it at a mock server.
pub const API_ENV: &str = "FACTORY_GITHUB_API";

/// The workflow whose run on `main` decides whether the base is green.
pub const CI_WORKFLOW: &str = "ci.yml";

/// Where the real API lives.
const DEFAULT_API: &str = "https://api.github.com";

/// Files fetched per page; GitHub's maximum.
const FILES_PER_PAGE: usize = 100;

/// Wall clock for one request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// One repository on GitHub, reached with one token.
pub struct GitHub {
    octocrab: Octocrab,
    owner: String,
    repo: String,
}

/// What a review needs to know about a pull request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pull {
    /// The number.
    pub number: u64,
    /// The title.
    pub title: String,
    /// The body; empty when GitHub has none.
    pub body: String,
    /// The head commit, which a review must name.
    pub head_sha: String,
    /// The web page.
    pub url: String,
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
    #[serde(default)]
    number: Option<u64>,
    title: String,
    body: Option<String>,
    head: HeadJson,
    #[serde(default)]
    html_url: Option<String>,
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

/// `GET /repos/{owner}/{repo}/actions/workflows/{id}/runs`.
#[derive(Deserialize)]
struct RunsJson {
    workflow_runs: Vec<RunJson>,
}

#[derive(Deserialize)]
struct RunJson {
    head_sha: String,
    status: Option<String>,
    conclusion: Option<String>,
    html_url: String,
}

impl GitHub {
    /// Builds a client from `FACTORY_GITHUB_TOKEN` (else `GITHUB_TOKEN`) and,
    /// when set, `FACTORY_GITHUB_API`.
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
    /// [`FactoryError::Config`] when no token is set, the repository is not
    /// `owner/repo`, or the API URL does not parse.
    pub fn from_env(repo: &str) -> Result<Self> {
        let token = [TOKEN_ENV, FALLBACK_TOKEN_ENV]
            .iter()
            .find_map(|name| std::env::var(name).ok())
            .filter(|token| !token.trim().is_empty())
            .ok_or_else(|| {
                FactoryError::Config(format!(
                    "neither {TOKEN_ENV} nor {FALLBACK_TOKEN_ENV} is set; a token with pull request access is needed"
                ))
            })?;
        let api = std::env::var(API_ENV).unwrap_or_else(|_| DEFAULT_API.to_owned());
        Self::new(repo, &token, &api)
    }

    /// Builds a client for a token and an API base.
    ///
    /// # Arguments
    ///
    /// * `repo` - The repository as `owner/repo`.
    /// * `token` - A personal or fine-grained token.
    /// * `api` - The API base URL.
    ///
    /// # Returns
    ///
    /// The client.
    ///
    /// # Errors
    ///
    /// [`FactoryError::Config`] when the repository is not `owner/repo` or
    /// the URL does not parse.
    pub fn new(repo: &str, token: &str, api: &str) -> Result<Self> {
        let (owner, name) = repo
            .split_once('/')
            .filter(|(owner, name)| !owner.is_empty() && !name.is_empty())
            .ok_or_else(|| {
                FactoryError::Config(format!("github.repo `{repo}` must be `owner/repo`"))
            })?;
        let octocrab = Octocrab::builder()
            .base_uri(api)
            .map_err(|error| {
                FactoryError::Config(format!("{API_ENV} `{api}` is not a URL: {error}"))
            })?
            .personal_token(token.to_owned())
            .set_connect_timeout(Some(REQUEST_TIMEOUT))
            .set_read_timeout(Some(REQUEST_TIMEOUT))
            .build()
            .map_err(|error| {
                FactoryError::Config(format!("building the GitHub client: {error}"))
            })?;
        Ok(Self {
            octocrab,
            owner: owner.to_owned(),
            repo: name.to_owned(),
        })
    }

    /// The repository as `owner/repo`.
    pub fn repo(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
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
    pub async fn pull(&self, number: u64) -> Result<Pull> {
        let json: PullJson = self
            .octocrab
            .get(self.pulls_route(number, ""), None::<&()>)
            .await
            .map_err(host_error)?;
        Ok(Pull {
            number: json.number.unwrap_or(number),
            title: json.title,
            body: json.body.unwrap_or_default(),
            head_sha: json.head.sha,
            url: json.html_url.unwrap_or_default(),
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
    pub async fn diff(&self, number: u64) -> Result<String> {
        self.octocrab
            .pulls(&self.owner, &self.repo)
            .get_diff(number)
            .await
            .map_err(host_error)
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
    pub async fn changed_files(&self, number: u64) -> Result<Vec<String>> {
        let mut files = Vec::new();
        for page in 1.. {
            let batch: Vec<FileJson> = self
                .octocrab
                .get(
                    self.pulls_route(number, "/files"),
                    Some(&[
                        ("per_page", FILES_PER_PAGE.to_string()),
                        ("page", page.to_string()),
                    ]),
                )
                .await
                .map_err(host_error)?;
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
    /// # Errors
    ///
    /// When GitHub refuses, for example when a comment names a line outside the diff.
    pub async fn post_review(
        &self,
        number: u64,
        head_sha: &str,
        body: &str,
        comments: &[InlineComment],
    ) -> Result<()> {
        let payload = serde_json::json!({
            "commit_id": head_sha,
            "event": "COMMENT",
            "body": body,
            "comments": comments,
        });
        let _: serde_json::Value = self
            .octocrab
            .post(self.pulls_route(number, "/reviews"), Some(&payload))
            .await
            .map_err(host_error)?;
        Ok(())
    }

    /// The latest run of the CI workflow on `main`, as a [`CiStatus`].
    async fn ci_of_main(&self) -> Result<CiStatus> {
        let route = format!(
            "/repos/{}/{}/actions/workflows/{CI_WORKFLOW}/runs",
            self.owner, self.repo
        );
        let runs: RunsJson = match self
            .octocrab
            .get(route, Some(&[("branch", "main"), ("per_page", "1")]))
            .await
        {
            Ok(runs) => runs,
            Err(error) if status_of(&error) == Some(404) => return Ok(CiStatus::Unknown),
            Err(error) => return Err(host_error(error)),
        };
        let Some(run) = runs.workflow_runs.into_iter().next() else {
            return Ok(CiStatus::Unknown);
        };
        Ok(match (run.status.as_deref(), run.conclusion.as_deref()) {
            (Some("completed"), Some("success")) => CiStatus::Green,
            (Some("completed"), Some("skipped" | "neutral")) => CiStatus::Green,
            (Some("completed"), Some(_)) => CiStatus::Red {
                sha: run.head_sha,
                url: run.html_url,
            },
            (Some("completed"), None) => CiStatus::Unknown,
            _ => CiStatus::Pending,
        })
    }

    /// `/repos/{owner}/{repo}/pulls/{number}{suffix}`.
    fn pulls_route(&self, number: u64, suffix: &str) -> String {
        format!("/repos/{}/{}/pulls/{number}{suffix}", self.owner, self.repo)
    }
}

impl CodeHost for GitHub {
    fn main_ci(&self) -> BoxFuture<'_, Result<CiStatus>> {
        Box::pin(self.ci_of_main())
    }
}

/// The HTTP status inside an octocrab error, when it carries one.
fn status_of(error: &octocrab::Error) -> Option<u16> {
    match error {
        octocrab::Error::GitHub { source, .. } => Some(source.status_code.as_u16()),
        _ => None,
    }
}

/// Maps an octocrab error to [`FactoryError::Host`].
fn host_error(error: octocrab::Error) -> FactoryError {
    let message = match &error {
        octocrab::Error::GitHub { source, .. } => source.message.clone(),
        other => other.to_string(),
    };
    FactoryError::Host {
        status: status_of(&error).unwrap_or(0),
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn client(server: &MockServer) -> GitHub {
        GitHub::new("owner/repo", "t0ken", &server.uri()).expect("client")
    }

    #[tokio::test]
    async fn repo_must_be_owner_slash_name() {
        for bad in ["owner", "owner/", "/repo", ""] {
            assert!(GitHub::new(bad, "t", DEFAULT_API).is_err(), "{bad}");
        }
        assert_eq!(
            GitHub::new("o/r", "t", DEFAULT_API).expect("client").repo(),
            "o/r"
        );
    }

    #[tokio::test]
    async fn main_ci_reads_the_latest_run() {
        let server = MockServer::start().await;
        let route = format!("/repos/owner/repo/actions/workflows/{CI_WORKFLOW}/runs");
        let run = |status: &str, conclusion: Option<&str>| {
            json!({ "workflow_runs": [{
                "head_sha": "abc1234def",
                "status": status,
                "conclusion": conclusion,
                "html_url": "https://github.com/owner/repo/actions/runs/1"
            }]})
        };
        for (body, expected) in [
            (run("completed", Some("success")), CiStatus::Green),
            (
                run("completed", Some("failure")),
                CiStatus::Red {
                    sha: "abc1234def".to_owned(),
                    url: "https://github.com/owner/repo/actions/runs/1".to_owned(),
                },
            ),
            (run("in_progress", None), CiStatus::Pending),
            (json!({ "workflow_runs": [] }), CiStatus::Unknown),
        ] {
            server.reset().await;
            Mock::given(method("GET"))
                .and(path(route.clone()))
                .and(query_param("branch", "main"))
                .respond_with(ResponseTemplate::new(200).set_body_json(body))
                .mount(&server)
                .await;
            assert_eq!(
                client(&server).await.main_ci().await.expect("status"),
                expected
            );
        }
        server.reset().await;
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(
                ResponseTemplate::new(404).set_body_json(json!({ "message": "Not Found" })),
            )
            .mount(&server)
            .await;
        assert_eq!(
            client(&server).await.main_ci().await.expect("status"),
            CiStatus::Unknown
        );
    }

    #[tokio::test]
    async fn refusals_become_host_errors_with_the_status() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/repos/owner/repo/pulls/7"))
            .respond_with(
                ResponseTemplate::new(403).set_body_json(json!({ "message": "Forbidden" })),
            )
            .mount(&server)
            .await;
        let error = client(&server).await.pull(7).await.expect_err("403");
        assert!(
            matches!(&error, FactoryError::Host { status: 403, message } if message == "Forbidden"),
            "{error}"
        );
    }
}
