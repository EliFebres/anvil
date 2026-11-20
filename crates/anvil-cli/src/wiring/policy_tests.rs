//! Each managed policy key, enforced through [`Wiring::with_policy`] and
//! `crate::policy::check_config`, and every lower layer failing to loosen
//! it: the user's configuration, a profile, anvil's own files under
//! `~/.anvil`, and files a repository ships.

use std::path::Path;

use anvil_core::provider::ToolCall;
use anvil_core::tools::{PromptAnswer, StaticPrompt, ToolContext, ToolOutcome};
use anvil_types::config::{LoadedConfig, load_profile};
use anvil_types::policy::{self, LoadedPolicy};

use super::*;

const ENDPOINT: &str = "https://models.corp.example/v1";

/// A policy table that turns the shell on in `ask` mode; a policy without
/// `[shell]` leaves the tool out.
const SHELL_ASK: &str = "[shell]\nmode = \"ask\"\n";

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, text).expect("write");
}

/// A policy file in `dir` with `extra` after the network table.
fn load_policy(dir: &Path, extra: &str) -> Arc<LoadedPolicy> {
    let path = dir.join(policy::POLICY_FILE);
    write(
        &path,
        &format!("version = 1\n[network]\nallowed_hosts = [\"models.corp.example:443\"]\n{extra}"),
    );
    Arc::new(policy::load(&path).expect("valid").expect("present"))
}

/// One corporate provider and `extra` providers or tables.
fn config_text(extra: &str) -> String {
    format!(
        r#"
[providers.corp]
protocol = "openai"
base_url = "{ENDPOINT}"
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
    )
}

/// A provider table named `name` at `url`.
fn provider(name: &str, url: &str) -> String {
    format!(
        "[providers.{name}]\nprotocol = \"openai\"\nbase_url = \"{url}\"\napi_key_env = \"PATH\"\nmodel = \"m\"\ncontext_window = 1000\nsupports_tools = true\nsupports_reasoning = false\n"
    )
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

/// Writes `~/.anvil/config.toml` with `extra` and loads it with `profile`.
fn loaded_config(s: &Setup, extra: &str, profile: Option<&str>) -> LoadedConfig {
    write(
        &s.home.join(".anvil").join("config.toml"),
        &config_text(extra),
    );
    load_profile(Some(&s.home), profile).expect("config loads")
}

/// The wiring as startup builds it: the configuration check, then the policy.
fn try_wiring(
    s: &Setup,
    config: &str,
    policy: Option<Arc<LoadedPolicy>>,
    project: ProjectAccess,
) -> anyhow::Result<Wiring> {
    let loaded = loaded_config(s, config, None);
    if let Some(policy) = &policy {
        crate::policy::check_config(policy, &loaded)?;
    }
    let wiring = Wiring::new_in(
        s.project.clone(),
        Some(s.home.clone()),
        loaded.config,
        project,
    )?;
    match policy {
        Some(policy) => wiring.with_policy(policy),
        None => Ok(wiring),
    }
}

fn wiring(s: &Setup, config: &str, policy: Option<Arc<LoadedPolicy>>) -> Wiring {
    try_wiring(s, config, policy, ProjectAccess::default()).expect("wiring")
}

/// The error a conflicting setup gives.
fn refused(s: &Setup, config: &str, policy: Arc<LoadedPolicy>) -> String {
    let error = try_wiring(s, config, Some(policy), ProjectAccess::default())
        .err()
        .expect("the policy refuses this setup");
    format!("{error:#}")
}

async fn outcome_with(
    wiring: &Wiring,
    answer: PromptAnswer,
    tool: &str,
    arguments: serde_json::Value,
) -> ToolOutcome {
    let registry = wiring
        .registry(Box::new(StaticPrompt(answer)))
        .expect("registry");
    let call = ToolCall {
        id: "p1".to_owned(),
        name: tool.to_owned(),
        arguments_json: arguments.to_string(),
    };
    registry
        .execute(&call, &wiring.tool_context())
        .await
        .outcome
}

async fn outcome(wiring: &Wiring, tool: &str, arguments: serde_json::Value) -> ToolOutcome {
    outcome_with(wiring, PromptAnswer::No, tool, arguments).await
}

#[test]
fn a_provider_host_outside_the_allowlist_is_an_error_naming_the_key() {
    let s = setup();
    let public = provider("public", "https://api.openai.com/v1");
    assert!(
        wiring(&s, &public, None).provider("public").is_ok(),
        "unmanaged"
    );
    let text = refused(&s, &public, load_policy(&s.policy_dir, ""));
    assert!(text.contains("[network] allowed_hosts"), "{text}");
    assert!(text.contains("api.openai.com:443"), "{text}");

    let managed = wiring(&s, "", Some(load_policy(&s.policy_dir, "")));
    assert!(managed.provider("corp").is_ok());
}

#[test]
fn plain_http_settings_of_the_user_conflict_with_the_policy() {
    let s = setup();
    let text = refused(
        &s,
        "[network]\nallow_http_hosts = [\"gpu.lan\"]\n",
        load_policy(&s.policy_dir, ""),
    );
    assert!(text.contains("[network] require_tls"), "{text}");
    let lan = provider("lan", "http://models.corp.example:443/v1");
    let policy = load_policy(&s.policy_dir, "");
    let managed = wiring(&s, &lan, Some(policy));
    let error = managed.provider("lan").err().expect("refused");
    assert!(format!("{error:#}").contains("require_tls"), "{error:#}");
}

#[test]
fn providers_and_models_outside_the_policy_are_errors() {
    let s = setup();
    let text = refused(
        &s,
        "",
        load_policy(&s.policy_dir, "[models]\nproviders = [\"approved\"]\n"),
    );
    assert!(text.contains("[models] providers"), "{text}");
    let text = refused(
        &s,
        "",
        load_policy(&s.policy_dir, "[models]\nmodels = [\"approved-model\"]\n"),
    );
    assert!(text.contains("[models] models"), "{text}");
    wiring(
        &s,
        "",
        Some(load_policy(
            &s.policy_dir,
            "[models]\nproviders = [\"corp\"]\nmodels = [\"m\"]\n",
        )),
    );
}

#[test]
fn a_profile_cannot_loosen_the_policy() {
    let s = setup();
    write(
        &s.home.join(".anvil").join("profiles").join("loose.toml"),
        "[permissions]\nallow = [\"shell\"]\n[limits]\nsession_tokens = 9000000\n",
    );
    let loaded = loaded_config(&s, "", Some("loose"));
    let policy = load_policy(
        &s.policy_dir,
        "[permissions]\nask = [\"shell\"]\n[limits]\nsession_tokens = 1000\n",
    );
    let error = crate::policy::check_config(&policy, &loaded).expect_err("conflict");
    let text = format!("{error:#}");
    assert!(text.contains("[permissions] ask rule `shell`"), "{text}");
    write(
        &s.home.join(".anvil").join("profiles").join("big.toml"),
        "[limits]\nsession_tokens = 9000000\n",
    );
    let loaded = loaded_config(&s, "", Some("big"));
    let error = crate::policy::check_config(&policy, &loaded).expect_err("conflict");
    assert!(
        format!("{error:#}").contains("[limits] session_tokens = 1000"),
        "{error:#}"
    );
}

#[test]
fn a_home_chosen_through_anvil_home_is_still_under_the_policy() {
    let s = setup();
    write(
        &s.home.join(".anvil").join("config.toml"),
        &config_text(&provider("public", "https://api.openai.com/v1")),
    );
    let home = home_dir_from(Some(s.home.clone().into_os_string())).expect("home");
    assert_eq!(home, s.home);
    let loaded = load_profile(Some(&home), None).expect("loads");
    let error = crate::policy::check_config(&load_policy(&s.policy_dir, ""), &loaded)
        .expect_err("the environment's home does not escape the policy");
    assert!(format!("{error:#}").contains("allowed_hosts"), "{error:#}");
}

#[test]
fn the_session_budget_is_capped_and_an_explicit_excess_is_an_error() {
    let s = setup();
    let policy = load_policy(&s.policy_dir, "[limits]\nsession_tokens = 5000\n");
    let capped = wiring(&s, "", Some(Arc::clone(&policy)));
    assert_eq!(
        capped.config.limits.session_tokens, 5000,
        "the default is capped"
    );
    let text = refused(&s, "[limits]\nsession_tokens = 6000\n", policy);
    assert!(text.contains("session_tokens"), "{text}");
}

#[test]
fn the_users_hooks_conflict_and_project_hooks_do_not_load() {
    let hook =
        r#"{"hooks": {"SessionStart": [{"hooks": [{"type": "command", "command": "echo hi"}]}]}}"#;
    for folder in [".anvil", ".claude"] {
        let s = setup();
        write(&s.home.join(folder).join("settings.json"), hook);
        assert!(
            !wiring(&s, "", None).hooks.is_empty(),
            "the user's own hook in ~/{folder}"
        );
        let text = refused(&s, "", load_policy(&s.policy_dir, ""));
        assert!(
            text.contains("[features] hooks = false"),
            "{folder}: {text}"
        );
        let on = wiring(
            &s,
            "",
            Some(load_policy(
                &s.policy_dir,
                "[features]
hooks = true
",
            )),
        );
        assert!(!on.hooks.is_empty());
    }

    let s = setup();
    write(&s.project.join(".claude").join("settings.json"), hook);
    let trusted = ProjectAccess {
        trusted: true,
        rules: ProjectRules::default(),
    };
    let unmanaged = try_wiring(&s, "", None, trusted.clone()).expect("wiring");
    assert!(!unmanaged.hooks.is_empty());
    let managed = try_wiring(&s, "", Some(load_policy(&s.policy_dir, "")), trusted)
        .expect("a repository's hooks are left out with a note, not an error");
    assert!(
        managed.hooks.is_empty(),
        "the policy wins over the project file"
    );
}

#[test]
fn mcp_servers_need_the_mode_and_a_matching_name_and_program() {
    let s = setup();
    write(
        &s.home.join(".anvil").join("mcp.toml"),
        "[servers.git]\ncommand = \"mcp-git\"\n",
    );
    let text = refused(&s, "", load_policy(&s.policy_dir, ""));
    assert!(text.contains("[features] mcp = \"off\""), "{text}");
    let listed = wiring(
        &s,
        "",
        Some(load_policy(
            &s.policy_dir,
            "[features]\nmcp = \"allowlist\"\nmcp_servers = [{ name = \"git\", command = \"MCP-GIT\" }]\n",
        )),
    );
    assert_eq!(listed.mcp_servers.servers().len(), 1);
    let text = refused(
        &s,
        "",
        load_policy(
            &s.policy_dir,
            "[features]\nmcp = \"allowlist\"\nmcp_servers = [{ name = \"git\", command = \"node\" }]\n",
        ),
    );
    assert!(text.contains("mcp_servers"), "{text}");

    let s = setup();
    write(
        &s.project.join(".mcp.json"),
        r#"{"mcpServers": {"evil": {"command": "node", "args": ["x.js"]}}}"#,
    );
    let trusted = ProjectAccess {
        trusted: true,
        rules: ProjectRules::default(),
    };
    let managed = try_wiring(&s, "", Some(load_policy(&s.policy_dir, "")), trusted)
        .expect("a repository's server is left out");
    assert!(
        managed.mcp_servers.is_empty(),
        "the policy wins over .mcp.json"
    );
}

#[test]
fn http_mcp_servers_need_a_url_rule_and_an_allowed_host() {
    let s = setup();
    let mcp = s.home.join(".anvil").join("mcp.toml");
    write(
        &mcp,
        "[servers.docs]
url = \"https://models.corp.example/mcp\"
",
    );
    let text = refused(&s, "", load_policy(&s.policy_dir, ""));
    assert!(text.contains("[features] mcp = \"off\""), "{text}");
    let by_command = refused(
        &s,
        "",
        load_policy(
            &s.policy_dir,
            "[features]
mcp = \"allowlist\"
mcp_servers = [{ name = \"docs\", command = \"https://models.corp.example/mcp\" }]
",
        ),
    );
    assert!(by_command.contains("mcp_servers"), "{by_command}");
    let listed = wiring(
        &s,
        "",
        Some(load_policy(
            &s.policy_dir,
            "[features]
mcp = \"allowlist\"
mcp_servers = [{ name = \"docs\", url = \"https://models.corp.example/mcp\" }]
",
        )),
    );
    assert_eq!(listed.mcp_servers.servers().len(), 1);
    assert!(listed.check_endpoints().is_ok());

    write(
        &mcp,
        "[servers.docs]
url = \"https://mcp.elsewhere.example/mcp\"
",
    );
    let text = refused(
        &s,
        "",
        load_policy(
            &s.policy_dir,
            "[features]
mcp = \"on\"
",
        ),
    );
    assert!(text.contains("[network] allowed_hosts"), "{text}");
}

#[test]
fn the_policy_network_table_sets_how_connections_are_made() {
    let s = setup();
    let pki = s.policy_dir.join("pki");
    let extra = format!(
        "ca_bundle = '{}'
proxy = \"system\"
",
        pki.join("ca.pem").display()
    );
    let managed = wiring(&s, "", Some(load_policy(&s.policy_dir, &extra)));
    let connection = managed.egress.connection();
    assert_eq!(connection.ca_bundle, Some(pki.join("ca.pem")));
    assert_eq!(connection.proxy, anvil_types::policy::ProxyMode::System);
    let error = managed
        .provider("corp")
        .err()
        .expect("the bundle is missing");
    assert!(
        format!("{error:#}").contains("[network] ca_bundle"),
        "{error:#}"
    );
}

#[test]
fn skills_follow_the_policy_mode() {
    let s = setup();
    write(
        &s.home
            .join(".claude")
            .join("skills")
            .join("mine")
            .join("SKILL.md"),
        "---\nname: mine\ndescription: d\n---\nbody\n",
    );
    let text = refused(&s, "", load_policy(&s.policy_dir, ""));
    assert!(
        text.contains("[features] skills = \"off\""),
        "a skill in ~/.claude is the user's own: {text}"
    );
    let bundled = refused(
        &s,
        "",
        load_policy(
            &s.policy_dir,
            "[features]
skills = \"bundled\"
",
        ),
    );
    assert!(
        bundled.contains("[features] skills = \"bundled\""),
        "{bundled}"
    );
    let local = wiring(
        &s,
        "",
        Some(load_policy(
            &s.policy_dir,
            "[features]\nskills = \"local\"\n",
        )),
    );
    assert!(local.skills.get("mine").is_some());
    write(
        &s.home
            .join(".anvil")
            .join("skills")
            .join("own")
            .join("SKILL.md"),
        "---\nname: own\ndescription: d\n---\nbody\n",
    );
    let text = refused(&s, "", load_policy(&s.policy_dir, ""));
    assert!(text.contains("[features] skills = \"off\""), "{text}");
}

#[tokio::test]
async fn policy_rules_beat_the_users_and_allow_tools_is_refused() {
    let s = setup();
    std::fs::write(s.project.join("a.txt"), "old").expect("write");
    let text = refused(
        &s,
        "[permissions]\nallow_tools = [\"edit_file\"]\n",
        load_policy(&s.policy_dir, ""),
    );
    assert!(text.contains("allow_allow_tool"), "{text}");
    let policy = load_policy(
        &s.policy_dir,
        "[permissions]\ndeny = [\"write_file\"]\nask = [\"edit_file\"]\n",
    );
    let managed = wiring(&s, "[permissions]\nallow = [\"list_dir\"]\n", Some(policy));
    assert_eq!(
        outcome_with(
            &managed,
            PromptAnswer::Yes,
            "write_file",
            serde_json::json!({"path": "b.txt", "content": "y"})
        )
        .await,
        ToolOutcome::DeniedByRule
    );
    assert_eq!(
        outcome(
            &managed,
            "edit_file",
            serde_json::json!({"path": "a.txt", "old": "old", "new": "new"})
        )
        .await,
        ToolOutcome::DeclinedByUser,
        "the policy's ask reaches a person"
    );
}

#[test]
fn a_default_of_allow_conflicts_with_allow_yes_false_in_every_user_layer() {
    let s = setup();
    let text = refused(
        &s,
        "[permissions]\ndefault = \"allow\"\n",
        load_policy(&s.policy_dir, ""),
    );
    assert!(text.contains("[permissions] allow_yes = false"), "{text}");
    assert!(text.contains("default = \"allow\""), "{text}");

    write(
        &s.home.join(".anvil").join("profiles").join("loose.toml"),
        "[permissions]\ndefault = \"allow\"\n",
    );
    let loaded = loaded_config(&s, "", Some("loose"));
    let error = crate::policy::check_config(&load_policy(&s.policy_dir, ""), &loaded)
        .expect_err("a profile's default of allow conflicts");
    assert!(
        format!("{error:#}").contains("[permissions] allow_yes = false"),
        "{error:#}"
    );

    let saved = anvil_types::trust::user_permissions_path(&s.home, &s.project);
    write(&saved, "default = \"allow\"\n");
    let text = refused(
        &s,
        "",
        load_policy(&s.policy_dir, "[permissions]\nallow_always = true\n"),
    );
    assert!(text.contains("[permissions] allow_yes = false"), "{text}");

    wiring(
        &s,
        "[permissions]\ndefault = \"allow\"\n",
        Some(load_policy(
            &s.policy_dir,
            "[permissions]\nallow_yes = true\n",
        )),
    );
    wiring(
        &s,
        "[permissions]\ndefault = \"ask\"\n",
        Some(load_policy(&s.policy_dir, "")),
    );
}

#[tokio::test]
async fn saved_always_rules_and_the_always_answer_follow_allow_always() {
    let s = setup();
    let saved = anvil_types::trust::user_permissions_path(&s.home, &s.project);
    write(&saved, "allow = [\"shell:echo *\"]\n");
    let text = refused(&s, "", load_policy(&s.policy_dir, ""));
    assert!(
        text.contains("[permissions] allow_always = false"),
        "{text}"
    );
    std::fs::remove_file(&saved).expect("remove");

    let managed = wiring(&s, "", Some(load_policy(&s.policy_dir, SHELL_ASK)));
    let command = serde_json::json!({"command": "echo hi"});
    outcome_with(&managed, PromptAnswer::Always, "shell", command.clone()).await;
    assert!(!saved.exists(), "an always answer saves nothing");

    let open = wiring(
        &s,
        "",
        Some(load_policy(
            &s.policy_dir,
            &format!("[permissions]\nallow_always = true\n{SHELL_ASK}"),
        )),
    );
    outcome_with(&open, PromptAnswer::Always, "shell", command).await;
    assert!(saved.exists(), "allowed: the rule is saved");
}

#[tokio::test]
async fn policy_files_settings_reach_the_file_tools() {
    let s = setup();
    let outside = tempfile::tempdir().expect("outside");
    write(&outside.path().join("spec.md"), "spec");
    write(&s.project.join("db.kdbx"), "vault");
    let text = refused(
        &s,
        &format!("[tools]\nextra_dirs = ['{}']\n", outside.path().display()),
        load_policy(&s.policy_dir, ""),
    );
    assert!(text.contains("[files] extra_roots"), "{text}");
    let managed = wiring(
        &s,
        "",
        Some(load_policy(
            &s.policy_dir,
            &format!(
                "[files]\nextra_roots = ['{}']\ndeny = [\"**/*.kdbx\"]\n",
                outside.path().display()
            ),
        )),
    );
    let spec = outside.path().join("spec.md").display().to_string();
    assert_eq!(
        outcome(&managed, "read_file", serde_json::json!({"path": spec})).await,
        ToolOutcome::Completed,
        "an extra root is readable"
    );
    let write_there = outside.path().join("new.md").display().to_string();
    assert_ne!(
        outcome_with(
            &managed,
            PromptAnswer::Yes,
            "write_file",
            serde_json::json!({"path": write_there, "content": "x"})
        )
        .await,
        ToolOutcome::Completed,
        "but not writable"
    );
    assert_ne!(
        outcome(
            &managed,
            "read_file",
            serde_json::json!({"path": "db.kdbx"})
        )
        .await,
        ToolOutcome::Completed,
        "a denied glob is refused inside the workspace"
    );
    let ctx: ToolContext = managed.tool_context();
    assert!(ctx.workspace.refuses(&s.project.join("db.kdbx")));
    assert!(
        !managed
            .tool_context()
            .workspace
            .refuses(&s.project.join("a.rs"))
    );
}

#[test]
fn delegation_and_memory_follow_their_switches() {
    let s = setup();
    let unmanaged = wiring(&s, "", None);
    assert!(unmanaged.memory_allowed() && unmanaged.delegation_allowed());
    let registry = unmanaged
        .registry(Box::new(StaticPrompt(PromptAnswer::No)))
        .expect("registry");
    assert!(registry.names().contains(&"delegate"));

    let managed = wiring(&s, "", Some(load_policy(&s.policy_dir, "")));
    assert!(!managed.memory_allowed() && !managed.delegation_allowed());
    let registry = managed
        .registry(Box::new(StaticPrompt(PromptAnswer::No)))
        .expect("registry");
    assert!(!registry.names().contains(&"delegate"));
    let text = refused(
        &s,
        "[memory]\nalways_summarize = true\n",
        load_policy(&s.policy_dir, ""),
    );
    assert!(text.contains("[features] memory = false"), "{text}");

    let on = wiring(
        &s,
        "",
        Some(load_policy(
            &s.policy_dir,
            "[features]\nmemory = true\ndelegation = true\n",
        )),
    );
    assert!(on.memory_allowed() && on.delegation_allowed());
}

#[test]
fn memory_off_reads_nothing_under_the_memory_folder() {
    let s = setup();
    let facts = s.home.join(".anvil").join("memory");
    write(&facts.join("tooling.md"), "- the build uses just\n");
    let with = crate::memory::prompt_sections(Some(&s.home), &s.project, true).expect("sections");
    let without =
        crate::memory::prompt_sections(Some(&s.home), &s.project, false).expect("sections");
    let text = |sections: &crate::memory::PromptSections| {
        sections
            .coder
            .iter()
            .map(|section| section.text.clone())
            .collect::<String>()
    };
    assert!(text(&with).contains("just"), "facts load when allowed");
    assert!(
        !text(&without).contains("just"),
        "no facts when memory is off"
    );
}

#[tokio::test]
async fn memory_off_writes_nothing_under_the_memory_folder_and_loads_no_facts() {
    let s = setup();
    let memory = s.home.join(".anvil").join("memory");
    write(
        &memory.join("tooling.md"),
        "- the build uses just
",
    );
    let before = std::fs::read_dir(&memory).expect("list").count();
    let managed = wiring(
        &s,
        "",
        Some(load_policy(
            &s.policy_dir,
            "[shell]
mode = \"ask\"
",
        )),
    );
    assert!(!managed.memory_allowed());

    let registry = managed
        .registry(Box::new(StaticPrompt(PromptAnswer::No)))
        .expect("registry");
    let config = managed
        .loop_config("task".to_owned(), s.project.join("session"), &registry)
        .expect("config");
    assert!(
        config
            .sections
            .iter()
            .all(|section| !section.text.contains("just")),
        "no remembered fact reaches the prompt"
    );

    let messages = [
        anvil_core::provider::Message::user("task"),
        anvil_core::provider::Message::Assistant {
            text: "done; the build uses just".to_owned(),
            reasoning: Vec::new(),
            tool_calls: Vec::new(),
        },
    ];
    crate::memory::finish_session(
        &managed,
        "t1",
        &messages,
        Box::new(|_: &anvil_memory::Proposal| anvil_memory::Decision::Keep),
        &crate::console::Console::Stderr,
        None,
    )
    .await;
    assert_eq!(
        std::fs::read_dir(&memory).expect("list").count(),
        before,
        "nothing is written under ~/.anvil/memory"
    );
    assert_eq!(
        std::fs::read_to_string(memory.join("tooling.md")).expect("read"),
        "- the build uses just
"
    );
}

#[test]
fn plan_approval_and_transcripts_follow_the_policy() {
    let s = setup();
    let unmanaged = wiring(&s, "", None);
    assert!(!unmanaged.plans_need_a_person());
    assert_eq!(
        unmanaged.sessions_root(),
        s.project.join(".anvil").join("sessions")
    );
    let transcripts = s.policy_dir.join("transcripts");
    let set = wiring(
        &s,
        "",
        Some(load_policy(
            &s.policy_dir,
            &format!(
                "[permissions]\nrequire_plan_approval = true\n[data]\ntranscript_dir = '{}'\n",
                transcripts.display()
            ),
        )),
    );
    assert!(set.plans_need_a_person());
    let (session_dir, _) = set.new_session_dir();
    assert!(session_dir.starts_with(&transcripts), "{session_dir:?}");
}

#[tokio::test]
async fn a_subagent_gets_the_same_policy_rules_and_file_limits() {
    let s = setup();
    write(&s.project.join("db.kdbx"), "vault");
    let managed = wiring(
        &s,
        "",
        Some(load_policy(
            &s.policy_dir,
            &format!(
                "[permissions]\ndeny = [\"shell\"]\n[files]\ndeny = [\"**/*.kdbx\"]\n[features]\ndelegation = true\n{SHELL_ASK}"
            ),
        )),
    );
    let env: &dyn SubagentEnv = &managed;
    let registry = env.registry(true).expect("subagent registry");
    let ctx = env.tool_context();
    let call = |name: &str, args: serde_json::Value| ToolCall {
        id: "s1".to_owned(),
        name: name.to_owned(),
        arguments_json: args.to_string(),
    };
    let shell = registry
        .execute(
            &call("shell", serde_json::json!({"command": "echo hi"})),
            &ctx,
        )
        .await;
    assert_eq!(shell.outcome, ToolOutcome::DeniedByRule);
    let read = registry
        .execute(
            &call("read_file", serde_json::json!({"path": "db.kdbx"})),
            &ctx,
        )
        .await;
    assert_ne!(read.outcome, ToolOutcome::Completed);
}

fn shell_call(command: &str) -> ToolCall {
    ToolCall {
        id: "sh".to_owned(),
        name: "shell".to_owned(),
        arguments_json: serde_json::json!({ "command": command }).to_string(),
    }
}

#[test]
fn the_shell_is_off_under_a_policy_unless_the_policy_turns_it_on() {
    let s = setup();
    let unmanaged = wiring(&s, "", None);
    let registry = unmanaged
        .registry(Box::new(StaticPrompt(PromptAnswer::No)))
        .expect("registry");
    assert!(
        registry.names().contains(&"shell"),
        "unmanaged default: ask"
    );

    let managed = wiring(
        &s,
        "",
        Some(load_policy(
            &s.policy_dir,
            "[features]\ndelegation = true\n",
        )),
    );
    assert_eq!(managed.shell.mode, anvil_types::shell::ShellMode::Off);
    let registry = managed
        .registry(Box::new(StaticPrompt(PromptAnswer::Yes)))
        .expect("registry");
    assert!(!registry.names().contains(&"shell"), "policy default: off");
    let env: &dyn SubagentEnv = &managed;
    let sub = env.registry(true).expect("subagent registry");
    assert!(
        !sub.names().contains(&"shell"),
        "a subagent has no shell either"
    );
    let error = crate::policy::check_build_shell(managed.policy.as_deref(), &managed.shell)
        .expect_err("anvil build needs the shell");
    assert!(
        error.to_string().contains("[shell] mode = \"off\""),
        "{error}"
    );

    let on = wiring(&s, "", Some(load_policy(&s.policy_dir, SHELL_ASK)));
    let registry = on
        .registry(Box::new(StaticPrompt(PromptAnswer::No)))
        .expect("registry");
    assert!(registry.names().contains(&"shell"));
    assert!(crate::policy::check_build_shell(on.policy.as_deref(), &on.shell).is_ok());

    let own = wiring(
        &s,
        "[shell]
mode = \"off\"
",
        None,
    );
    let registry = own
        .registry(Box::new(StaticPrompt(PromptAnswer::No)))
        .expect("registry");
    assert!(!registry.names().contains(&"shell"), "the user's own off");
    let error = crate::policy::check_build_shell(None, &own.shell).expect_err("refused");
    assert!(error.to_string().contains("your configuration"), "{error}");
}

#[test]
fn user_shell_settings_that_loosen_the_policy_are_errors_naming_the_key() {
    let s = setup();
    let allowlist = "[shell]\nmode = \"allowlist\"\nallow = [\"cargo test *\", \"just check\"]\n";
    let cases = [
        ("", "[shell]\nmode = \"ask\"\n", "[shell] mode = \"off\""),
        (
            allowlist,
            "[shell]\nmode = \"ask\"\n",
            "[shell] mode = \"allowlist\"",
        ),
        (
            SHELL_ASK,
            "[shell]\nexecutor = \"container\"\n[shell.container]\nimage = \"x:1\"\n",
            "[shell] executor = \"host\"",
        ),
        (
            "",
            "[tools]\ncheck = \"just check\"\n",
            "[shell] mode = \"off\"",
        ),
        (
            allowlist,
            "[tools]\ncheck = \"npm test\"\n",
            "[shell] allow",
        ),
        (
            allowlist,
            "[tools]\ncheck = \"just check; curl x\"\n",
            "[shell] allow",
        ),
    ];
    for (policy_text, config, key) in cases {
        let text = refused(&s, config, load_policy(&s.policy_dir, policy_text));
        assert!(text.contains(key), "{config}: {text}");
    }
    let fits = wiring(
        &s,
        "[tools]\ncheck = \"just check\"\n[shell]\nmode = \"allowlist\"\nallow = [\"just *\"]\n",
        Some(load_policy(&s.policy_dir, allowlist)),
    );
    assert_eq!(
        fits.shell.layers.len(),
        2,
        "the user's list narrows the policy's"
    );
    let stricter = wiring(
        &s,
        "[shell]\nmode = \"off\"\n",
        Some(load_policy(&s.policy_dir, SHELL_ASK)),
    );
    assert_eq!(stricter.shell.mode, anvil_types::shell::ShellMode::Off);
}

#[tokio::test]
async fn a_policy_allowlist_refuses_metacharacters_and_starts_programs_directly() {
    let s = setup();
    let managed = wiring(
        &s,
        "[permissions]\nallow = [\"shell\"]\n",
        Some(load_policy(
            &s.policy_dir,
            "[shell]\nmode = \"allowlist\"\nallow = [\"git status\", \"git push origin *\"]\n",
        )),
    );
    let registry = managed
        .registry(Box::new(StaticPrompt(PromptAnswer::Yes)))
        .expect("registry");
    let ctx = managed.tool_context();
    assert!(
        ctx.runner.direct,
        "allowlist mode starts programs without a shell"
    );
    for command in [
        "git status; echo pwned",
        "git status && echo pwned",
        "git status | echo pwned",
        "git status > out.txt",
        "git status `id`",
        "git status $(id)",
        "git status\necho pwned",
        "cargo build",
        "git status -s",
    ] {
        let run = registry.execute(&shell_call(command), &ctx).await;
        assert_eq!(run.outcome, ToolOutcome::Blocked, "{command:?}");
    }
    let push = managed.permit_shell(
        "git push origin main",
        Box::new(StaticPrompt(PromptAnswer::Yes)),
        None,
    );
    assert!(
        push.as_ref()
            .is_err_and(|reason| reason.contains("git subcommand `push`")),
        "allowlisted git runs only the listed subcommands, even one a pattern names: {push:?}"
    );
    let section = managed.shell.prompt_section().expect("the model is told");
    assert!(section.text.contains("`git status`"), "{}", section.text);
}

#[test]
fn the_policys_container_executor_reaches_every_tool_context() {
    let s = setup();
    let managed = wiring(
        &s,
        "",
        Some(load_policy(
            &s.policy_dir,
            "[shell]\nmode = \"ask\"\nexecutor = \"container\"\n[shell.container]\nruntime = \"podman\"\nimage = \"registry.corp.example/build@sha256:0123\"\n[features]\ndelegation = true\n",
        )),
    );
    for ctx in [managed.tool_context(), SubagentEnv::tool_context(&managed)] {
        let container = ctx.runner.container.expect("container executor");
        assert_eq!(container.program, "podman");
        assert_eq!(
            container.settings.image,
            "registry.corp.example/build@sha256:0123"
        );
    }
}

#[tokio::test]
async fn subagents_are_confined_like_the_session() {
    let s = setup();
    let outside = tempfile::tempdir().expect("outside");
    write(&outside.path().join("notes.md"), "outside");
    write(&s.project.join(".env"), "API_KEY=x");
    write(&s.project.join("id_ed25519"), "key");
    write(&s.project.join("db.kdbx"), "vault");
    let managed = wiring(
        &s,
        "",
        Some(load_policy(
            &s.policy_dir,
            "[files]\ndeny = [\"**/*.kdbx\"]\n[features]\ndelegation = true\n[shell]\nmode = \"allowlist\"\nallow = [\"git status\"]\n",
        )),
    );
    let main_ctx = managed.tool_context();
    let env: &dyn SubagentEnv = &managed;
    let registry = env.registry(true).expect("subagent registry");
    let ctx = env.tool_context();
    assert_eq!(ctx.cwd, main_ctx.cwd);
    assert_eq!(ctx.runner.direct, main_ctx.runner.direct);
    assert!(ctx.runner.direct);
    let read = |path: String| ToolCall {
        id: "r".to_owned(),
        name: "read_file".to_owned(),
        arguments_json: serde_json::json!({ "path": path }).to_string(),
    };
    for path in [
        outside.path().join("notes.md").display().to_string(),
        "../outside.md".to_owned(),
        ".env".to_owned(),
        "id_ed25519".to_owned(),
        "db.kdbx".to_owned(),
    ] {
        let run = registry.execute(&read(path.clone()), &ctx).await;
        assert!(run.result.is_error, "{path}: {}", run.result.content);
    }
    for command in ["git status; id", "cargo build", "curl https://example.com"] {
        let run = registry.execute(&shell_call(command), &ctx).await;
        assert_eq!(run.outcome, ToolOutcome::Blocked, "{command}");
    }
}

#[tokio::test]
async fn mcp_approve_is_refused_under_a_policy() {
    let s = setup();
    let policy = load_policy(
        &s.policy_dir,
        "[features]\nmcp = \"allowlist\"\nmcp_servers = [{ name = \"docs\", command = \"mcp-docs\" }]\n",
    );
    let managed = wiring(&s, "", Some(policy));
    let error = crate::commands::mcp_approve(&managed, "docs")
        .await
        .expect_err("refused under a policy");
    let message = format!("{error:#}");
    assert!(
        message.contains("listing_sha256") && message.contains("[features] mcp_servers"),
        "{message}"
    );
    assert!(managed.mcp_store().is_managed());
    let unmanaged = wiring(&s, "", None);
    assert!(!unmanaged.mcp_store().is_managed());
}

#[test]
fn doctor_checks_the_policys_transcript_and_mirror_folders_too() {
    let s = setup();
    let transcripts = s.policy_dir.join("transcripts");
    let mirror = s.policy_dir.join("mirror");
    let policy = load_policy(
        &s.policy_dir,
        &format!(
            "[data]\ntranscript_dir = '{}'\n[audit]\nmirror_dir = '{}'\n",
            transcripts.display(),
            mirror.display()
        ),
    );
    let managed = wiring(&s, "", Some(policy));
    let rows: Vec<(&str, PathBuf)> = crate::doctor::private_dirs(&managed);
    assert!(
        rows.contains(&("data:transcript_dir", transcripts)),
        "{rows:?}"
    );
    assert!(rows.contains(&("data:mirror_dir", mirror)), "{rows:?}");
    let unmanaged = wiring(&s, "", None);
    assert_eq!(crate::doctor::private_dirs(&unmanaged).len(), 2);
}

#[test]
fn the_memory_summarizer_runs_only_with_a_full_transcript() {
    for (transcript, runs) in [("full", true), ("metadata", false), ("off", false)] {
        let s = setup();
        let policy = load_policy(
            &s.policy_dir,
            &format!("[features]\nmemory = true\n[data]\ntranscript = \"{transcript}\"\n"),
        );
        let managed = wiring(&s, "", Some(policy));
        let reason = crate::memory::policy_skip_reason(&managed);
        assert_eq!(reason.is_none(), runs, "{transcript}: {reason:?}");
        if let Some(reason) = reason {
            assert!(reason.contains("[data] transcript"), "{reason}");
        }
    }
}

#[tokio::test]
async fn chat_ends_without_the_summarizer_below_full() {
    let s = setup();
    let policy = load_policy(
        &s.policy_dir,
        "[features]\nmemory = true\n[data]\ntranscript = \"metadata\"\n",
    );
    let managed = wiring(&s, "", Some(policy));
    let asked = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = std::sync::Arc::clone(&asked);
    let messages = [
        anvil_core::provider::Message::user("task"),
        anvil_core::provider::Message::Assistant {
            text: "done; the build uses just".to_owned(),
            reasoning: Vec::new(),
            tool_calls: Vec::new(),
        },
    ];
    let started = std::time::Instant::now();
    crate::memory::finish_session(
        &managed,
        "t1",
        &messages,
        Box::new(move |_: &anvil_memory::Proposal| {
            seen.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            anvil_memory::Decision::Keep
        }),
        &crate::console::Console::Stderr,
        None,
    )
    .await;
    assert_eq!(asked.load(std::sync::atomic::Ordering::Relaxed), 0);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "no provider was contacted"
    );
    assert!(!s.home.join(".anvil").join("memory").exists());
}

#[tokio::test]
async fn resume_refuses_below_a_full_transcript() {
    for mode in ["metadata", "off"] {
        let s = setup();
        let policy = load_policy(&s.policy_dir, &format!("[data]\ntranscript = \"{mode}\"\n"));
        let managed = wiring(&s, "", Some(policy));
        let error = crate::commands::resume(
            &managed,
            "01ARZ3NDEKTSV4RRFFQ69G5FAV",
            true,
            crate::frontend::UiKind::Plain,
        )
        .await
        .expect_err("refused");
        let message = format!("{error:#}");
        assert!(
            message.contains("needs the full transcript") && message.contains(mode),
            "{message}"
        );
    }
}

#[test]
fn retention_runs_when_a_session_starts() {
    let s = setup();
    let policy = load_policy(
        &s.policy_dir,
        "[data]
retention_days = 0
",
    );
    let managed = wiring(&s, "", Some(policy));
    let root = managed.sessions_root();
    let old = root.join("01ARZ3NDEKTSV4RRFFQ69G5FAV");
    let live = root.join("01ARZ3NDEKTSV4RRFFQ69G5FAW");
    for dir in [&old, &live] {
        std::fs::create_dir_all(dir).expect("mkdir");
        std::fs::write(
            dir.join("session.jsonl"),
            "{}
",
        )
        .expect("write");
    }
    let lock = std::fs::File::create(live.join(anvil_types::status::LOCK_FILE)).expect("lock");
    lock.lock().expect("locks");
    std::thread::sleep(std::time::Duration::from_millis(20));
    crate::before_session(&managed, crate::frontend::UiKind::Plain);
    assert!(
        !old.exists(),
        "a session past retention_days is removed at session start"
    );
    assert!(live.is_dir(), "a running session stays");
}

#[test]
fn the_redaction_off_note_is_printed_and_redaction_is_off() {
    let s = setup();
    let policy = load_policy(&s.policy_dir, "[data]\nredaction = false\n");
    let managed = wiring(&s, "", Some(policy));
    assert!(!managed.guard.redacts());
    assert!(REDACTION_OFF_NOTE.contains("[data] redaction = false"));
    let source = include_str!("../wiring.rs");
    assert!(
        source.contains("eprintln!(\"{REDACTION_OFF_NOTE}\")"),
        "with_policy prints the note"
    );
}
