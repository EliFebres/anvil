//! The sandbox with `runtime = "none"`: a shell script stands in for anvil
//! and dumps the environment it was given, so the env filtering is tested
//! for real.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anvil_types::config::SETUP_PROVIDERS;
use anvil_types::{AnvilConfig, ContainerRuntime};
use factory_core::{FactoryHome, Implementer, ImplementerStatus, Sandbox, SandboxConfig};

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, text).expect("write");
}

/// Whether a bash is available to run the stand-in scripts.
fn have_bash() -> bool {
    std::process::Command::new("bash")
        .arg("--version")
        .output()
        .is_ok()
}

#[tokio::test]
async fn only_provider_keys_reach_the_implementer_and_its_exit_is_mapped() {
    if !have_bash() {
        eprintln!("bash not available; skipping");
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    let home = FactoryHome::under(temp.path());
    let worktree = home.work_dir("T1");
    std::fs::create_dir_all(&worktree).expect("mkdir");
    let fake_anvil = temp.path().join("fake-anvil.sh");
    write(
        &fake_anvil,
        r#"#!/usr/bin/env bash
# Dump the environment names and the arguments, then exit as the task says.
env | cut -d= -f1 | sort > env-names.txt
printf '%s\n' "$@" > args.txt
printf '%s\n' "ANVIL_HOME=$ANVIL_HOME" > home.txt
mkdir -p .anvil/telemetry
printf '%s\n' '{"event":"turn","session":"s","provider":"anthropic","model":"m","tokens":{"input":1000000,"output":0,"cached":0}}' > .anvil/telemetry/2026-10-31.jsonl
case "$4" in
  *incomplete*) echo "incomplete: token budget" >&2; exit 2 ;;
  *crash*) exit 7 ;;
  *) exit 0 ;;
esac
"#,
    );
    let fake_just = temp.path().join("fake-just.sh");
    write(
        &fake_just,
        "#!/usr/bin/env bash\necho checked \"$@\"\nexit 3\n",
    );

    let anvil: AnvilConfig = toml::from_str(SETUP_PROVIDERS).expect("parses");
    // The key table stands for the factory's environment: one provider key
    // set, the others unset. The factory's own secrets are never looked up.
    let keys: factory_core::sandbox::KeyLookup = Arc::new(|name: &str| match name {
        "ANTHROPIC_API_KEY" => Some("sk-ant-test".to_owned()),
        _ => None,
    });
    let sandbox = Sandbox::with_key_lookup(
        SandboxConfig {
            runtime: ContainerRuntime::None,
            image: String::new(),
            anvil_bin: fake_anvil.to_string_lossy().into_owned(),
            just_bin: fake_just.to_string_lossy().into_owned(),
            cargo_bin: "cargo".to_owned(),
            egress_allow: Vec::new(),
            providers: anvil.providers.clone(),
            anvil_config: SETUP_PROVIDERS.to_owned(),
            profile: "[roles]
coder = \"anthropic\"
"
            .to_owned(),
            home: home.clone(),
        },
        keys,
    );

    sandbox.prepare(&worktree).await.expect("prepare");
    let deadline = Instant::now() + Duration::from_secs(60);
    let outcome = sandbox
        .run(&worktree, "do the thing", deadline)
        .await
        .expect("runs");
    assert_eq!(outcome.status, ImplementerStatus::Done);
    assert_eq!(outcome.tokens.input, 1_000_000);
    assert!((outcome.cost_usd.expect("priced") - 4.0).abs() < 1e-9);

    let names: Vec<String> = std::fs::read_to_string(worktree.join("env-names.txt"))
        .expect("env dump")
        .lines()
        .map(str::to_owned)
        .collect();
    assert!(names.contains(&"ANTHROPIC_API_KEY".to_owned()), "{names:?}");
    assert!(names.contains(&"ANVIL_HOME".to_owned()), "{names:?}");
    assert!(
        !names.contains(&"OPENAI_API_KEY".to_owned()),
        "unset keys are not invented"
    );
    for forbidden in ["FACTORY_GITHUB_TOKEN", "WEBHOOK_URL", "GITHUB_TOKEN"] {
        assert!(
            !names.contains(&forbidden.to_owned()),
            "{forbidden} leaked: {names:?}"
        );
    }
    let args = std::fs::read_to_string(worktree.join("args.txt")).expect("args");
    assert_eq!(args, "--yes\n--profile\nfactory\ndo the thing\n");
    let anvil_home = std::fs::read_to_string(worktree.join("home.txt")).expect("home");
    assert!(anvil_home.trim().ends_with("T1-home"), "{anvil_home}");
    assert!(
        home.ticket_home("T1")
            .join(".anvil/profiles/factory.toml")
            .is_file()
    );

    let incomplete = sandbox
        .run(&worktree, "please go incomplete", deadline)
        .await
        .expect("runs");
    assert_eq!(
        incomplete.status,
        ImplementerStatus::Incomplete("token budget".to_owned())
    );
    let crashed = sandbox
        .run(&worktree, "crash now", deadline)
        .await
        .expect("runs");
    assert_eq!(crashed.status, ImplementerStatus::Failed { code: Some(7) });

    let check = sandbox.check(&worktree, deadline).await.expect("check");
    assert!(!check.passed);
    assert!(
        check.output_tail.contains("checked check"),
        "{}",
        check.output_tail
    );

    let timed_out = sandbox
        .run(&worktree, "x", Instant::now())
        .await
        .expect("runs");
    assert_eq!(timed_out.status, ImplementerStatus::Timeout);
    assert!(sandbox.denied_hosts().is_empty());
}
