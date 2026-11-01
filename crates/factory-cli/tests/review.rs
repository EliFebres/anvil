//! `factory review` against a fake GitHub and two fake model providers.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PR: u64 = 7;

const DIFF: &str = "diff --git a/src/a.rs b/src/a.rs
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,2 +1,3 @@
 fn a() {}
+fn b() {}
 fn c() {}
";

/// A review as the OpenAI protocol would return it.
fn openai_reply(review: &Value) -> Value {
    json!({
        "choices": [{"message": {"content": review.to_string()}}],
        "usage": {"prompt_tokens": 100, "completion_tokens": 10}
    })
}

/// A review as the Anthropic protocol would return it.
fn anthropic_reply(review: &Value) -> Value {
    json!({
        "content": [{"type": "text", "text": review.to_string()}],
        "usage": {"input_tokens": 100, "output_tokens": 10}
    })
}

fn approve() -> Value {
    json!({"verdict": "approve", "findings": []})
}

/// The test world: a temp home with the anvil config, a project with
/// factory.toml, and the three mock servers.
struct World {
    _dir: tempfile::TempDir,
    home: PathBuf,
    project: PathBuf,
    github: MockServer,
    alpha: MockServer,
    beta: MockServer,
}

impl World {
    /// With `coder_on_alpha`, the coder shares alpha's base URL, which the
    /// independence check must refuse.
    async fn new(coder_on_alpha: bool) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join("home");
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).expect("mkdir");
        let github = MockServer::start().await;
        let alpha = MockServer::start().await;
        let beta = MockServer::start().await;
        let coder_url = if coder_on_alpha {
            format!("{}/v1", alpha.uri())
        } else {
            "https://coder.example/v1".to_owned()
        };
        write(
            &home.join(".anvil").join("config.toml"),
            &format!(
                r#"
[providers.coder]
protocol = "anthropic"
base_url = "{coder_url}"
api_key_env = "CODER_KEY"
model = "coder-1"
context_window = 100000
supports_tools = true
supports_reasoning = false

[providers.alpha]
protocol = "openai"
base_url = "{alpha}/v1"
api_key_env = "ALPHA_KEY"
model = "alpha-1"
context_window = 100000
supports_tools = true
supports_reasoning = false

[providers.beta]
protocol = "anthropic"
base_url = "{beta}/v1"
api_key_env = "BETA_KEY"
model = "beta-1"
context_window = 100000
supports_tools = true
supports_reasoning = false

[roles]
coder = "coder"
summarizer = "coder"
"#,
                alpha = alpha.uri(),
                beta = beta.uri()
            ),
        );
        write(
            &project.join("factory.toml"),
            r#"
[roles]
coder = "coder"

[[reviewers]]
provider = "alpha"
stance = "correctness"

[[reviewers]]
provider = "beta"
stance = "safety"

[github]
repo = "owner/repo"

[notify]
webhook_env = "WEBHOOK_URL"
"#,
        );
        write(&project.join("CLAUDE.md"), "# Rules\n\nNo unwrap.\n");
        Self {
            _dir: dir,
            home,
            project,
            github,
            alpha,
            beta,
        }
    }

    /// Mounts the pull request, its diff and its files on the fake GitHub.
    async fn mount_pull(&self) {
        Mock::given(method("GET"))
            .and(path(format!("/repos/owner/repo/pulls/{PR}")))
            .and(header("accept", "application/vnd.github.v3.diff"))
            .respond_with(ResponseTemplate::new(200).set_body_string(DIFF))
            .mount(&self.github)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/repos/owner/repo/pulls/{PR}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "number": PR,
                "title": "Add b",
                "body": "Adds b.\n\n- [ ] b exists\n",
                "head": {"sha": "abc123"}
            })))
            .mount(&self.github)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/repos/owner/repo/pulls/{PR}/files")))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!([{"filename": "src/a.rs"}])),
            )
            .mount(&self.github)
            .await;
    }

    async fn mount_models(&self, alpha: Value, beta: Value) {
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(openai_reply(&alpha)))
            .mount(&self.alpha)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(anthropic_reply(&beta)))
            .mount(&self.beta)
            .await;
    }

    /// Runs `factory review <PR>` in the project with every key set.
    fn run(&self) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_factory"))
            .current_dir(&self.project)
            .env("FACTORY_HOME", &self.home)
            .env("FACTORY_GITHUB_API", self.github.uri())
            .env("GITHUB_TOKEN", "ghp_test")
            .env("ALPHA_KEY", "a")
            .env("BETA_KEY", "b")
            .env("CODER_KEY", "c")
            .args(["review", &PR.to_string()])
            .output()
            .expect("factory runs")
    }

    /// The bodies of every POST the fake GitHub received.
    async fn posted_reviews(&self) -> Vec<Value> {
        self.github
            .received_requests()
            .await
            .expect("recorded")
            .iter()
            .filter(|request| request.method == "POST")
            .map(|request| serde_json::from_slice(&request.body).expect("json body"))
            .collect()
    }
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, text).expect("write");
}

#[tokio::test]
async fn a_blocker_exits_2_and_only_the_minors_are_posted() {
    let world = World::new(false).await;
    world.mount_pull().await;
    Mock::given(method("POST"))
        .and(path(format!("/repos/owner/repo/pulls/{PR}/reviews")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"id": 1})))
        .expect(1)
        .mount(&world.github)
        .await;
    world
        .mount_models(
            json!({"verdict": "changes", "findings": [
                {"severity": "blocker", "file": "src/a.rs", "line": 2, "message": "b does nothing", "suggested_fix": "implement b"},
                {"severity": "minor", "file": "src/a.rs", "line": 2, "message": "name it better", "suggested_fix": null},
                {"severity": "minor", "file": "src/zzz.rs", "line": 9, "message": "elsewhere", "suggested_fix": "move it"}
            ]}),
            approve(),
        )
        .await;

    let out = world.run();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(2),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(stdout.contains("verdict: changes (1 blocker)"), "{stdout}");
    assert!(stdout.contains("alpha/correctness"), "{stdout}");
    assert!(stdout.contains("beta/safety"), "{stdout}");
    assert!(
        stdout.contains("posted 1 inline comment(s) and 1 in the review body"),
        "{stdout}"
    );

    let reviews = world.posted_reviews().await;
    assert_eq!(reviews.len(), 1, "exactly one review is posted");
    let review = &reviews[0];
    assert_eq!(review["event"], "COMMENT");
    assert_eq!(review["commit_id"], "abc123");
    let comments = review["comments"].as_array().expect("comments");
    assert_eq!(comments.len(), 1);
    assert_eq!(comments[0]["path"], "src/a.rs");
    assert_eq!(comments[0]["line"], 2);
    assert_eq!(comments[0]["side"], "RIGHT");
    assert_eq!(
        comments[0]["body"],
        "**minor** (alpha/correctness): name it better"
    );
    let body = review["body"].as_str().expect("body");
    assert!(
        body.contains("`src/zzz.rs:9`: **minor** (alpha/correctness): elsewhere"),
        "{body}"
    );
    assert!(body.contains("Suggested fix: move it"), "{body}");
    assert!(
        !body.contains("b does nothing"),
        "the blocker is never posted: {body}"
    );
    assert!(!review.to_string().contains("implement b"));

    let alpha_requests = world.alpha.received_requests().await.expect("recorded");
    let sent: Value = serde_json::from_slice(&alpha_requests[0].body).expect("json");
    let user = sent["messages"][1]["content"]
        .as_str()
        .expect("user message");
    assert!(
        user.contains("- b exists"),
        "acceptance from the checklist: {user}"
    );
    assert!(user.contains("No unwrap."), "CLAUDE.md is included: {user}");
    assert!(user.contains("+fn b() {}"), "the diff is included: {user}");
}

#[tokio::test]
async fn approval_exits_0_and_posts_nothing() {
    let world = World::new(false).await;
    world.mount_pull().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&world.github)
        .await;
    world.mount_models(approve(), approve()).await;

    let out = world.run();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains("verdict: approve (no blocking findings)"),
        "{stdout}"
    );
    assert!(!stdout.contains("posted"), "{stdout}");
    assert!(world.posted_reviews().await.is_empty());
}

#[tokio::test]
async fn a_reviewer_on_the_coder_url_is_refused_before_any_request() {
    let world = World::new(true).await;
    world.mount_pull().await;
    world.mount_models(approve(), approve()).await;

    let out = world.run();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "stderr: {stderr}");
    assert!(stderr.contains("shares base_url"), "{stderr}");
    assert!(
        world
            .alpha
            .received_requests()
            .await
            .expect("recorded")
            .is_empty()
    );
    assert!(
        world
            .beta
            .received_requests()
            .await
            .expect("recorded")
            .is_empty()
    );
    assert!(
        world
            .github
            .received_requests()
            .await
            .expect("recorded")
            .is_empty()
    );
}
