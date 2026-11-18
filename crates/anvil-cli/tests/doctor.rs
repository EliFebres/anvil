//! `anvil doctor` and `anvil setup --endpoint` against mock providers: the
//! failing provider lands in its own row with a fix, the healthy one passes,
//! an echoed key never shows, and setup writes one provider for every role.

use std::path::Path;
use std::process::Command;

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// One streamed `OK` in the OpenAI chat-completions shape.
const GOOD_STREAM: &str = concat!(
    "data: {\"id\":\"doctor\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"OK\"},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"doctor\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":1,\"total_tokens\":11}}\n\n",
    "data: [DONE]\n\n",
);

fn config(good: &str, bad: &str) -> String {
    format!(
        r#"
[providers.good]
protocol = "openai"
base_url = "{good}/v1"
api_key_env = "ANVIL_TEST_GOOD_KEY"
model = "good-model"
context_window = 1000
supports_tools = true
supports_reasoning = false

[providers.bad]
protocol = "openai"
base_url = "{bad}/v1"
api_key_env = "ANVIL_TEST_BAD_KEY"
model = "bad-model"
context_window = 1000
supports_tools = true
supports_reasoning = false

[roles]
coder = "good"
summarizer = "good"
"#
    )
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, text).expect("write");
}

#[tokio::test]
async fn doctor_pings_the_planner_and_lists_it_in_the_roles() {
    let good = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(GOOD_STREAM, "text/event-stream"))
        .mount(&good)
        .await;
    let planner = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(GOOD_STREAM, "text/event-stream"))
        .expect(1)
        .mount(&planner)
        .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let home = dir.path().join("home");
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).expect("mkdir");
    // `bad` is the planner here: the mock behind it answers.
    write(
        &home.join(".anvil").join("config.toml"),
        &config(&good.uri(), &planner.uri()).replace(
            "summarizer = \"good\"",
            "summarizer = \"good\"
planner = \"bad\"",
        ),
    );

    let out = Command::new(env!("CARGO_BIN_EXE_anvil"))
        .current_dir(&project)
        .env("ANVIL_HOME", &home)
        .env("ANVIL_LOG", "off")
        .env("ANVIL_TEST_GOOD_KEY", "good-key")
        .env("ANVIL_TEST_BAD_KEY", "bad-key")
        .arg("doctor")
        .output()
        .expect("anvil runs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let row = stdout
        .lines()
        .find(|line| line.contains("provider:bad"))
        .unwrap_or_else(|| {
            panic!(
                "no row for the planner's provider:
{stdout}"
            )
        });
    assert!(row.starts_with("PASS"), "{row}");
    assert!(stdout.contains("planner=bad"), "{stdout}");
}

#[tokio::test]
async fn doctor_reports_the_failing_provider_in_its_row_with_a_fix() {
    let good = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(GOOD_STREAM, "text/event-stream"))
        .mount(&good)
        .await;
    let bad = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(401).set_body_raw(
            r#"{"error":{"message":"Incorrect API key provided","type":"invalid_request_error"}}"#,
            "application/json",
        ))
        .mount(&bad)
        .await;

    let dir = tempfile::tempdir().expect("tempdir");
    let home = dir.path().join("home");
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).expect("mkdir");
    write(
        &home.join(".anvil").join("config.toml"),
        &config(&good.uri(), &bad.uri()),
    );

    let out = Command::new(env!("CARGO_BIN_EXE_anvil"))
        .current_dir(&project)
        .env("ANVIL_HOME", &home)
        .env("ANVIL_LOG", "off")
        .env("ANVIL_TEST_GOOD_KEY", "good-key")
        .env("ANVIL_TEST_BAD_KEY", "bad-key")
        .arg("doctor")
        .output()
        .expect("anvil runs");
    assert_eq!(
        out.status.code(),
        Some(1),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().collect();

    let bad_index = lines
        .iter()
        .position(|line| line.contains("provider:bad"))
        .unwrap_or_else(|| panic!("no provider:bad row:\n{stdout}"));
    assert!(lines[bad_index].starts_with("FAIL"), "{}", lines[bad_index]);
    assert!(lines[bad_index].contains("401"), "{}", lines[bad_index]);
    let fix = lines[bad_index + 1];
    assert!(fix.trim_start().starts_with("fix:"), "{fix}");
    assert!(fix.contains("ANVIL_TEST_BAD_KEY"), "{fix}");
    assert!(fix.contains(&bad.uri()), "{fix}");

    let good_row = lines
        .iter()
        .find(|line| line.contains("provider:good"))
        .unwrap_or_else(|| panic!("no provider:good row:\n{stdout}"));
    assert!(good_row.starts_with("PASS"), "{good_row}");
    assert!(good_row.contains("good-model"), "{good_row}");
    assert!(good_row.contains(" ms"), "{good_row}");

    assert!(stdout.contains("PASS  key:ANVIL_TEST_GOOD_KEY"), "{stdout}");
    assert!(!stdout.contains("github:"), "{stdout}");
    assert!(stdout.contains("SKIP  secrets"), "{stdout}");
    assert!(stdout.contains("check(s) failed"), "{stdout}");
}

#[tokio::test]
async fn a_server_that_echoes_the_key_cannot_put_it_in_output_or_logs() {
    const KEY: &str = "echoed-secret-value-1234567890";
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(401).set_body_raw(
            format!(r#"{{"error":{{"message":"bad key Bearer {KEY}"}}}}"#),
            "application/json",
        ))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().expect("tempdir");
    let home = dir.path().join("home");
    let project = dir.path().join("project");
    std::fs::create_dir_all(&project).expect("mkdir");
    write(
        &home.join(".anvil").join("config.toml"),
        &config(&server.uri(), &server.uri()),
    );
    let out = Command::new(env!("CARGO_BIN_EXE_anvil"))
        .current_dir(&project)
        .env("ANVIL_HOME", &home)
        .env("ANVIL_LOG", "debug")
        .env("ANVIL_TEST_GOOD_KEY", KEY)
        .env("ANVIL_TEST_BAD_KEY", KEY)
        .arg("doctor")
        .output()
        .expect("anvil runs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stdout.contains("[redacted]"), "{stdout}");
    assert!(!stdout.contains(KEY), "{stdout}");
    assert!(!stderr.contains(KEY), "{stderr}");
    let logs = project.join(".anvil").join("logs");
    for entry in std::fs::read_dir(&logs).expect("logs written").flatten() {
        let text = std::fs::read_to_string(entry.path()).unwrap_or_default();
        assert!(
            !text.contains(KEY),
            "{} holds the key",
            entry.path().display()
        );
    }
}

#[tokio::test]
async fn setup_endpoint_probes_the_server_and_points_every_role_at_it() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"data": [{"id": "qwen-coder", "max_model_len": 65536}]}"#,
            "application/json",
        ))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().expect("tempdir");
    let home = dir.path().join("home");
    let out = Command::new(env!("CARGO_BIN_EXE_anvil"))
        .current_dir(dir.path())
        .env("ANVIL_HOME", &home)
        .env("ANVIL_LOG", "off")
        .env("ANVIL_ENDPOINT_KEY", "endpoint-key")
        .args(["setup", "--endpoint", &server.uri()])
        .output()
        .expect("anvil runs");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let text = std::fs::read_to_string(home.join(".anvil").join("config.toml")).expect("config");
    let config: anvil_types::AnvilConfig = toml::from_str(&text).expect("parses");
    config.validate().expect("validates");
    let endpoint = &config.providers["endpoint"];
    assert_eq!(
        endpoint.base_url,
        format!("{}/v1", server.uri()),
        "fell back to /v1"
    );
    assert_eq!(endpoint.model, "qwen-coder");
    assert_eq!(endpoint.context_window, 65536);
    assert_eq!(config.roles.coder, "endpoint");
    assert_eq!(config.roles.summarizer, "endpoint");
    assert_eq!(config.providers.len(), 1, "no hosted provider was written");
}

#[test]
fn setup_endpoint_refuses_plain_http_to_a_remote_host_before_sending() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = Command::new(env!("CARGO_BIN_EXE_anvil"))
        .current_dir(dir.path())
        .env("ANVIL_HOME", dir.path().join("home"))
        .env("ANVIL_LOG", "off")
        .args(["setup", "--endpoint", "http://198.51.100.7:8000/v1"])
        .output()
        .expect("anvil runs");
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("plain http"), "{stderr}");
    assert!(
        !dir.path()
            .join("home")
            .join(".anvil")
            .join("config.toml")
            .exists()
    );
}
