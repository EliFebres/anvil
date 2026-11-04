//! `anvil doctor` against mock providers: the failing one lands in its own
//! row with a fix, the healthy one passes.

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
        .env_remove("GITHUB_TOKEN")
        .env_remove("FACTORY_GITHUB_TOKEN")
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
    assert!(stdout.contains("SKIP  github:token"), "{stdout}");
    assert!(stdout.contains("SKIP  secrets"), "{stdout}");
    assert!(stdout.contains("check(s) failed"), "{stdout}");
}
