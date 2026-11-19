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
fn the_users_own_hooks_conflict_and_shared_or_project_hooks_do_not_load() {
    let s = setup();
    let hook =
        r#"{"hooks": {"SessionStart": [{"hooks": [{"type": "command", "command": "echo hi"}]}]}}"#;
    write(&s.home.join(".anvil").join("settings.json"), hook);
    assert!(
        !wiring(&s, "", None).hooks.is_empty(),
        "the user's own hook"
    );
    let text = refused(&s, "", load_policy(&s.policy_dir, ""));
    assert!(text.contains("[features] hooks = false"), "{text}");
    let on = wiring(
        &s,
        "",
        Some(load_policy(&s.policy_dir, "[features]\nhooks = true\n")),
    );
    assert!(!on.hooks.is_empty());

    let s = setup();
    write(&s.project.join(".claude").join("settings.json"), hook);
    write(&s.home.join(".claude").join("settings.json"), hook);
    let trusted = ProjectAccess {
        trusted: true,
        rules: ProjectRules::default(),
    };
    let unmanaged = try_wiring(&s, "", None, trusted.clone()).expect("wiring");
    assert!(!unmanaged.hooks.is_empty());
    let managed = try_wiring(&s, "", Some(load_policy(&s.policy_dir, "")), trusted)
        .expect("a repository's or Claude Code's hooks are left out, not an error");
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
    let off = wiring(&s, "", Some(load_policy(&s.policy_dir, "")));
    assert!(off.skills.get("mine").is_none());
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

    let managed = wiring(&s, "", Some(load_policy(&s.policy_dir, "")));
    let command = serde_json::json!({"command": "echo hi"});
    outcome_with(&managed, PromptAnswer::Always, "shell", command.clone()).await;
    assert!(!saved.exists(), "an always answer saves nothing");

    let open = wiring(
        &s,
        "",
        Some(load_policy(
            &s.policy_dir,
            "[permissions]\nallow_always = true\n",
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
            "[permissions]\ndeny = [\"shell\"]\n[files]\ndeny = [\"**/*.kdbx\"]\n[features]\ndelegation = true\n",
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
