//! Each managed policy key, enforced through [`Wiring::with_policy`], and
//! the user's own configuration failing to loosen it.

use std::path::Path;

use anvil_core::provider::ToolCall;
use anvil_core::tools::{PromptAnswer, StaticPrompt, ToolContext, ToolOutcome};
use anvil_types::policy::{self, LoadedPolicy};

use super::*;

const ENDPOINT: &str = "https://models.corp.example/v1";

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, text).expect("write");
}

/// A policy file in `dir` with `extra` after the endpoint table.
fn load_policy(dir: &Path, extra: &str) -> Arc<LoadedPolicy> {
    let path = dir.join(policy::POLICY_FILE);
    write(
        &path,
        &format!("[endpoints]\nallowed_base_urls = [\"{ENDPOINT}\"]\n{extra}"),
    );
    Arc::new(policy::load(&path).expect("valid").expect("present"))
}

/// The user's configuration: a corporate and a public provider, plus `extra`.
fn user_config(extra: &str) -> AnvilConfig {
    toml::from_str(&format!(
        r#"
[providers.corp]
protocol = "openai"
base_url = "{ENDPOINT}"
api_key_env = "PATH"
model = "m"
context_window = 1000
supports_tools = true
supports_reasoning = false

[providers.public]
protocol = "openai"
base_url = "https://api.openai.com/v1"
api_key_env = "PATH"
model = "m"
context_window = 1000
supports_tools = true
supports_reasoning = false

[providers.lan]
protocol = "openai"
base_url = "http://gpu.lan:8000/v1"
api_key_env = "PATH"
model = "m"
context_window = 1000
supports_tools = true
supports_reasoning = false

[roles]
coder = "corp"
summarizer = "corp"

{extra}
"#
    ))
    .expect("config parses")
}

struct Setup {
    _dirs: Vec<tempfile::TempDir>,
    project: PathBuf,
    home: PathBuf,
    policy_dir: PathBuf,
}

fn setup() -> Setup {
    let project = tempfile::tempdir().expect("project");
    let home = tempfile::tempdir().expect("home");
    let policy_dir = tempfile::tempdir().expect("policy");
    Setup {
        project: project.path().to_path_buf(),
        home: home.path().to_path_buf(),
        policy_dir: policy_dir.path().to_path_buf(),
        _dirs: vec![project, home, policy_dir],
    }
}

fn wiring(setup: &Setup, config: &str, policy: Option<Arc<LoadedPolicy>>) -> Wiring {
    let wiring = Wiring::new_in(
        setup.project.clone(),
        Some(setup.home.clone()),
        user_config(config),
        ProjectAccess::default(),
    )
    .expect("wiring");
    match policy {
        Some(policy) => wiring.with_policy(policy).expect("policy applies"),
        None => wiring,
    }
}

async fn outcome(wiring: &Wiring, tool: &str, arguments: serde_json::Value) -> ToolOutcome {
    let registry = wiring
        .registry(Box::new(StaticPrompt(PromptAnswer::No)))
        .expect("registry");
    let call = ToolCall {
        id: "p1".to_owned(),
        name: tool.to_owned(),
        arguments_json: arguments.to_string(),
    };
    registry
        .execute(&call, &ToolContext::new(&wiring.cwd))
        .await
        .outcome
}

#[test]
fn only_allowed_endpoints_are_called_and_the_user_cannot_add_http_hosts() {
    let s = setup();
    let config = "[network]\nallow_http_hosts = [\"gpu.lan\"]\n";
    let personal = wiring(&s, config, None);
    assert!(personal.provider("public").is_ok(), "personal mode");
    assert!(
        personal.provider("lan").is_ok(),
        "personal mode allows the listed host"
    );

    let managed = wiring(&s, config, Some(load_policy(&s.policy_dir, "")));
    assert!(managed.provider("corp").is_ok());
    let public = managed.provider("public").err().expect("refused");
    assert!(
        format!("{public:#}").contains("not under an endpoint"),
        "{public:#}"
    );
    let lan = managed.provider("lan").err().expect("refused");
    assert!(format!("{lan:#}").contains("plain http"), "{lan:#}");
}

#[test]
fn hooks_are_off_unless_the_policy_turns_them_on() {
    let s = setup();
    write(
        &s.home.join(".anvil").join("settings.json"),
        r#"{"hooks": {"SessionStart": [{"hooks": [{"type": "command", "command": "echo hi"}]}]}}"#,
    );
    assert!(
        !wiring(&s, "", None).hooks.is_empty(),
        "the user's own hook"
    );
    let off = wiring(&s, "", Some(load_policy(&s.policy_dir, "")));
    assert!(off.hooks.is_empty());
    let on = wiring(
        &s,
        "",
        Some(load_policy(&s.policy_dir, "[extensions]\nhooks = true\n")),
    );
    assert!(!on.hooks.is_empty());
}

#[test]
fn only_listed_mcp_commands_start() {
    let s = setup();
    write(
        &s.home.join(".anvil").join("mcp.toml"),
        "[servers.git]\ncommand = \"mcp-git\"\n[servers.other]\ncommand = \"mcp-other\"\n",
    );
    let none = wiring(&s, "", Some(load_policy(&s.policy_dir, "")));
    assert!(none.mcp_servers.is_empty());
    let listed = wiring(
        &s,
        "",
        Some(load_policy(
            &s.policy_dir,
            "[extensions]\nmcp_commands = [\"MCP-GIT\"]\n",
        )),
    );
    let names: Vec<&str> = listed
        .mcp_servers
        .servers()
        .iter()
        .map(|server| server.name.as_str())
        .collect();
    assert_eq!(names, ["git"]);
}

#[test]
fn skills_load_only_from_the_policy_folders() {
    let s = setup();
    let skill = |dir: &Path, name: &str| {
        write(
            &dir.join(name).join("SKILL.md"),
            &format!("---\nname: {name}\ndescription: d\n---\nbody\n"),
        );
    };
    skill(&s.home.join(".anvil").join("skills"), "mine");
    let approved = s.policy_dir.join("skills");
    skill(&approved, "approved");
    let managed = wiring(
        &s,
        "",
        Some(load_policy(
            &s.policy_dir,
            &format!("[extensions]\nskill_dirs = ['{}']\n", approved.display()),
        )),
    );
    assert!(managed.skills.get("mine").is_none());
    assert!(managed.skills.get("approved").is_some());
}

#[tokio::test]
async fn the_users_allow_tools_is_ignored_when_skipping_prompts_is_off() {
    let s = setup();
    std::fs::write(s.project.join("a.txt"), "old").expect("write");
    let config = "[permissions]\nallow_tools = [\"edit_file\"]\n";
    let edit = serde_json::json!({"path": "a.txt", "old": "old", "new": "new"});
    let mut managed = wiring(&s, config, Some(load_policy(&s.policy_dir, "")));
    managed.allow_tools(&[]).expect("valid");
    assert_eq!(
        outcome(&managed, "edit_file", edit.clone()).await,
        ToolOutcome::DeclinedByUser
    );
    let mut open = wiring(
        &s,
        config,
        Some(load_policy(
            &s.policy_dir,
            "[permissions]\nallow_skip_prompts = true\n",
        )),
    );
    open.allow_tools(&[]).expect("valid");
    assert_eq!(
        outcome(&open, "edit_file", edit).await,
        ToolOutcome::Completed
    );
}

#[tokio::test]
async fn a_forced_profile_replaces_the_users_allows_but_keeps_their_denies() {
    let s = setup();
    write(
        &s.policy_dir.join("strict.toml"),
        "[permissions]\nallow = [\"write_file\"]\ndefault = \"deny\"\n",
    );
    let policy = load_policy(&s.policy_dir, "[permissions]\nprofile = \"strict.toml\"\n");
    let config = "[permissions]\nallow = [\"shell\"]\ndeny = [\"write_file:secret/**\"]\n";
    let managed = wiring(&s, config, Some(policy));
    assert_eq!(
        outcome(&managed, "shell", serde_json::json!({"command": "echo hi"})).await,
        ToolOutcome::DeniedByRule,
        "the user's allow does not survive the forced profile"
    );
    assert_eq!(
        outcome(
            &managed,
            "write_file",
            serde_json::json!({"path": "secret/x", "content": "y"})
        )
        .await,
        ToolOutcome::DeniedByRule,
        "the user's deny still applies"
    );
    assert_eq!(
        outcome(
            &managed,
            "write_file",
            serde_json::json!({"path": "ok.txt", "content": "y"})
        )
        .await,
        ToolOutcome::Completed
    );
}

#[test]
fn summarizer_calls_plan_approval_and_transcripts_follow_the_policy() {
    let s = setup();
    let personal = wiring(&s, "", None);
    assert!(personal.summarizer_calls_allowed());
    assert!(!personal.plans_need_a_person());
    assert_eq!(
        personal.sessions_root(),
        s.project.join(".anvil").join("sessions")
    );

    let strict = wiring(&s, "", Some(load_policy(&s.policy_dir, "")));
    assert!(
        !strict.summarizer_calls_allowed(),
        "off unless the policy turns it on"
    );

    let transcripts = s.policy_dir.join("transcripts");
    let set = wiring(
        &s,
        "[memory]\nalways_summarize = true\n",
        Some(load_policy(
            &s.policy_dir,
            &format!(
                "[permissions]\nrequire_plan_approval = true\n[sessions]\nsummarizer_calls = true\ntranscript_dir = '{}'\n",
                transcripts.display()
            ),
        )),
    );
    assert!(set.summarizer_calls_allowed());
    assert!(set.plans_need_a_person());
    let (session_dir, _) = set.new_session_dir();
    assert!(session_dir.starts_with(&transcripts), "{session_dir:?}");
}
