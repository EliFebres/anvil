//! GitHub through octocrab: the only module that knows the REST routes.
//!
//! One token, one repository. `factory review` reads pull requests and posts
//! comments with it; the worker opens, labels, merges and closes pull
//! requests through the [`CodeHost`] port.

use std::time::{Duration, Instant};

use octocrab::Octocrab;
use serde::{Deserialize, Serialize};

use crate::error::{FactoryError, Result};
use crate::ports::{BoxFuture, CiStatus, CodeHost, InlineComment, PullRequest};

/// The environment variable that holds the factory's own token.
pub const TOKEN_ENV: &str = "FACTORY_GITHUB_TOKEN";

/// The personal token, used when the factory token is unset (`factory review`
/// on a laptop).
pub const FALLBACK_TOKEN_ENV: &str = "GITHUB_TOKEN";

/// Overrides the API base URL; tests point it at a mock server.
pub const API_ENV: &str = "FACTORY_GITHUB_API";

/// The workflow whose run on `main` decides whether the base is green.
pub const CI_WORKFLOW: &str = "ci.yml";

/// The check run the merge waits for.
pub const CI_CHECK: &str = "ci";

/// Where the real API lives.
const DEFAULT_API: &str = "https://api.github.com";

/// Files fetched per page; GitHub's maximum.
const FILES_PER_PAGE: usize = 100;

/// Wall clock for one request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// How often the CI of a pull request is polled.
const CI_POLL: Duration = Duration::from_secs(60);

/// One repository on GitHub, reached with one token.
pub struct GitHub {
    octocrab: Octocrab,
    owner: String,
    repo: String,
    ci_poll: Duration,
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

/// One inline comment as the review API wants it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct CommentJson<'a> {
    path: &'a str,
    line: u32,
    body: &'a str,
    side: &'static str,
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

/// `GET /repos/{owner}/{repo}/commits/{sha}/check-runs`.
#[derive(Deserialize)]
struct CheckRunsJson {
    check_runs: Vec<CheckRunJson>,
}

#[derive(Deserialize)]
struct CheckRunJson {
    name: String,
    status: Option<String>,
    conclusion: Option<String>,
    #[serde(default)]
    html_url: Option<String>,
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
            ci_poll: CI_POLL,
        })
    }

    /// Changes how often a pull request's CI is polled; tests shorten it.
    ///
    /// # Arguments
    ///
    /// * `interval` - The new interval.
    pub fn with_ci_poll(mut self, interval: Duration) -> Self {
        self.ci_poll = interval;
        self
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
        let comments: Vec<CommentJson<'_>> = comments
            .iter()
            .map(|c| CommentJson {
                path: &c.path,
                line: c.line,
                body: &c.body,
                side: "RIGHT",
            })
            .collect();
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
            (Some("completed"), Some("success" | "skipped" | "neutral")) => CiStatus::Green,
            (Some("completed"), Some(_)) => CiStatus::Red {
                sha: run.head_sha,
                url: run.html_url,
            },
            (Some("completed"), None) => CiStatus::Unknown,
            _ => CiStatus::Pending,
        })
    }

    /// The `ci` check run of a commit right now.
    async fn ci_of_commit(&self, sha: &str) -> Result<CiStatus> {
        let route = format!(
            "/repos/{}/{}/commits/{sha}/check-runs",
            self.owner, self.repo
        );
        let runs: CheckRunsJson = self
            .octocrab
            .get(route, Some(&[("check_name", CI_CHECK), ("per_page", "20")]))
            .await
            .map_err(host_error)?;
        let Some(run) = runs.check_runs.into_iter().find(|run| run.name == CI_CHECK) else {
            return Ok(CiStatus::Unknown);
        };
        Ok(match (run.status.as_deref(), run.conclusion.as_deref()) {
            (Some("completed"), Some("success" | "skipped" | "neutral")) => CiStatus::Green,
            (Some("completed"), Some(_)) => CiStatus::Red {
                sha: sha.to_owned(),
                url: run.html_url.unwrap_or_default(),
            },
            _ => CiStatus::Pending,
        })
    }

    /// Creates the label when the repository lacks it.
    async fn ensure_label(&self, label: &str) -> Result<()> {
        let route = format!("/repos/{}/{}/labels", self.owner, self.repo);
        let payload = serde_json::json!({
            "name": label,
            "color": "d93f0b",
            "description": "a factory pull request that touches a protected path",
        });
        match self
            .octocrab
            .post::<_, serde_json::Value>(route, Some(&payload))
            .await
        {
            Ok(_) => Ok(()),
            Err(error) if status_of(&error) == Some(422) => Ok(()),
            Err(error) => Err(host_error(error)),
        }
    }

    /// `/repos/{owner}/{repo}/pulls/{number}{suffix}`.
    fn pulls_route(&self, number: u64, suffix: &str) -> String {
        format!("/repos/{}/{}/pulls/{number}{suffix}", self.owner, self.repo)
    }

    /// `/repos/{owner}/{repo}/issues/{number}{suffix}`.
    fn issues_route(&self, number: u64, suffix: &str) -> String {
        format!(
            "/repos/{}/{}/issues/{number}{suffix}",
            self.owner, self.repo
        )
    }
}

impl CodeHost for GitHub {
    fn main_ci(&self) -> BoxFuture<'_, Result<CiStatus>> {
        Box::pin(self.ci_of_main())
    }

    fn open_pull<'a>(
        &'a self,
        head_branch: &'a str,
        title: &'a str,
        body: &'a str,
    ) -> BoxFuture<'a, Result<PullRequest>> {
        Box::pin(async move {
            let payload = serde_json::json!({
                "title": title,
                "head": head_branch,
                "base": "main",
                "body": body,
            });
            let json: PullJson = self
                .octocrab
                .post(
                    format!("/repos/{}/{}/pulls", self.owner, self.repo),
                    Some(&payload),
                )
                .await
                .map_err(host_error)?;
            Ok(PullRequest {
                number: json.number.ok_or_else(|| FactoryError::Host {
                    status: 0,
                    message: "the new pull request has no number".to_owned(),
                })?,
                url: json.html_url.unwrap_or_default(),
                head_sha: json.head.sha,
            })
        })
    }

    fn post_review<'a>(
        &'a self,
        number: u64,
        head_sha: &'a str,
        body: &'a str,
        comments: &'a [InlineComment],
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(GitHub::post_review(self, number, head_sha, body, comments))
    }

    fn add_label<'a>(&'a self, number: u64, label: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.ensure_label(label).await?;
            let payload = serde_json::json!({ "labels": [label] });
            let _: serde_json::Value = self
                .octocrab
                .post(self.issues_route(number, "/labels"), Some(&payload))
                .await
                .map_err(host_error)?;
            Ok(())
        })
    }

    fn wait_for_ci<'a>(
        &'a self,
        head_sha: &'a str,
        deadline: Instant,
    ) -> BoxFuture<'a, Result<CiStatus>> {
        Box::pin(async move {
            loop {
                let status = self.ci_of_commit(head_sha).await?;
                if matches!(status, CiStatus::Green | CiStatus::Red { .. }) {
                    return Ok(status);
                }
                if Instant::now() >= deadline {
                    return Ok(status);
                }
                tokio::time::sleep(self.ci_poll).await;
            }
        })
    }

    fn squash_merge<'a>(&'a self, number: u64, title: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let payload = serde_json::json!({
                "merge_method": "squash",
                "commit_title": title,
            });
            let _: serde_json::Value = self
                .octocrab
                .put(self.pulls_route(number, "/merge"), Some(&payload))
                .await
                .map_err(host_error)?;
            Ok(())
        })
    }

    fn delete_branch<'a>(&'a self, branch: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let route = format!(
                "/repos/{}/{}/git/refs/heads/{branch}",
                self.owner, self.repo
            );
            let response = self
                .octocrab
                ._delete(route, None::<&()>)
                .await
                .map_err(host_error)?;
            let status = response.status();
            if !status.is_success() {
                return Err(FactoryError::Host {
                    status: status.as_u16(),
                    message: format!("deleting branch {branch}"),
                });
            }
            Ok(())
        })
    }

    fn close_pull<'a>(&'a self, number: u64, comment: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let _: serde_json::Value = self
                .octocrab
                .post(
                    self.issues_route(number, "/comments"),
                    Some(&serde_json::json!({ "body": comment })),
                )
                .await
                .map_err(host_error)?;
            let _: serde_json::Value = self
                .octocrab
                .patch(
                    self.pulls_route(number, ""),
                    Some(&serde_json::json!({ "state": "closed" })),
                )
                .await
                .map_err(host_error)?;
            Ok(())
        })
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
    use wiremock::matchers::{body_partial_json, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn client(server: &MockServer) -> GitHub {
        GitHub::new("owner/repo", "t0ken", &server.uri())
            .expect("client")
            .with_ci_poll(Duration::from_millis(10))
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

    #[tokio::test]
    async fn opens_labels_merges_and_closes_pull_requests() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/repos/owner/repo/pulls"))
            .and(body_partial_json(
                json!({ "head": "factory/T1", "base": "main", "title": "Fix it" }),
            ))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "number": 12, "title": "Fix it", "body": "b", "head": { "sha": "feed" },
                "html_url": "https://github.com/owner/repo/pull/12"
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/repos/owner/repo/labels"))
            .respond_with(
                ResponseTemplate::new(422).set_body_json(json!({ "message": "Validation Failed" })),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/repos/owner/repo/issues/12/labels"))
            .and(body_partial_json(json!({ "labels": ["needs-human"] })))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!([{ "name": "needs-human" }])),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/repos/owner/repo/pulls/12/merge"))
            .and(body_partial_json(
                json!({ "merge_method": "squash", "commit_title": "Fix it" }),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "merged": true })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path("/repos/owner/repo/git/refs/heads/factory/T1"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/repos/owner/repo/issues/12/comments"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": 1 })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("PATCH"))
            .and(path("/repos/owner/repo/pulls/12"))
            .and(body_partial_json(json!({ "state": "closed" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "state": "closed" })))
            .expect(1)
            .mount(&server)
            .await;

        let github = client(&server).await;
        let pull = github
            .open_pull("factory/T1", "Fix it", "b")
            .await
            .expect("opened");
        assert_eq!(
            pull,
            PullRequest {
                number: 12,
                url: "https://github.com/owner/repo/pull/12".to_owned(),
                head_sha: "feed".to_owned()
            }
        );
        github.add_label(12, "needs-human").await.expect("labelled");
        github.squash_merge(12, "Fix it").await.expect("merged");
        github.delete_branch("factory/T1").await.expect("deleted");
        github.close_pull(12, "CI failed").await.expect("closed");
    }

    #[tokio::test]
    async fn waits_for_the_ci_check_until_it_completes_or_the_deadline_passes() {
        let server = MockServer::start().await;
        let route = "/repos/owner/repo/commits/feed/check-runs";
        let runs = |status: &str, conclusion: Option<&str>| {
            json!({ "check_runs": [
                { "name": "check (ubuntu-latest)", "status": "completed", "conclusion": "success" },
                { "name": "ci", "status": status, "conclusion": conclusion, "html_url": "https://ci/1" }
            ]})
        };
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(runs("completed", Some("success"))),
            )
            .mount(&server)
            .await;
        let github = client(&server).await;
        let far = Instant::now() + Duration::from_secs(60);
        assert_eq!(
            github.wait_for_ci("feed", far).await.expect("ci"),
            CiStatus::Green
        );

        server.reset().await;
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(runs("completed", Some("failure"))),
            )
            .mount(&server)
            .await;
        assert_eq!(
            github.wait_for_ci("feed", far).await.expect("ci"),
            CiStatus::Red {
                sha: "feed".to_owned(),
                url: "https://ci/1".to_owned()
            }
        );

        server.reset().await;
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(ResponseTemplate::new(200).set_body_json(runs("in_progress", None)))
            .mount(&server)
            .await;
        let soon = Instant::now() + Duration::from_millis(50);
        assert_eq!(
            github.wait_for_ci("feed", soon).await.expect("ci"),
            CiStatus::Pending
        );

        server.reset().await;
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "check_runs": [] })))
            .mount(&server)
            .await;
        let soon = Instant::now() + Duration::from_millis(50);
        assert_eq!(
            github.wait_for_ci("feed", soon).await.expect("ci"),
            CiStatus::Unknown
        );
    }
}
