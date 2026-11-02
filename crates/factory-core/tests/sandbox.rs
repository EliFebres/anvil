//! The sandbox two ways: with `runtime = "none"` a shell script stands in
//! for anvil and dumps the environment it was given, so the env filtering is
//! tested for real; with `runtime = "podman"` a fake runtime answers the
//! `podman` calls, so the volume, image and start-up handling are tested
//! without a container.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anvil_types::config::SETUP_PROVIDERS;
use anvil_types::{AnvilConfig, ContainerRuntime};
use factory_core::fakes::FakeRuntime;
use factory_core::sandbox::CACHE_VOLUME;
use factory_core::{
    Exit, FactoryError, FactoryHome, Implementer, ImplementerStatus, Sandbox, SandboxConfig,
    ensure_cache_volume, preflight,
};

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
  *nostart*) echo "error: no provider configured" >&2; exit 1 ;;
esac
echo "session 01ARZ3NDEKTSV4RRFFQ69G5FAV" >&2
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
    let never_started = sandbox
        .run(&worktree, "nostart", deadline)
        .await
        .expect_err("a failure before the session line is the factory's");
    assert!(
        never_started
            .to_string()
            .contains("the host could not start the implementer (exit 1 before anvil's session line): error: no provider configured"),
        "{never_started}"
    );

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

/// A podman sandbox over the fake runtime, with the ticket home under `temp`.
fn podman_sandbox(temp: &Path, runtime: Arc<FakeRuntime>) -> Sandbox {
    let anvil: AnvilConfig = toml::from_str(SETUP_PROVIDERS).expect("parses");
    let keys: factory_core::sandbox::KeyLookup = Arc::new(|_: &str| None);
    Sandbox::with_runner(
        SandboxConfig {
            runtime: ContainerRuntime::Podman,
            image: "localhost/anvil-factory:latest".to_owned(),
            anvil_bin: "anvil".to_owned(),
            just_bin: "just".to_owned(),
            cargo_bin: "cargo".to_owned(),
            egress_allow: Vec::new(),
            providers: anvil.providers,
            anvil_config: SETUP_PROVIDERS.to_owned(),
            profile: "[roles]\ncoder = \"anthropic\"\n".to_owned(),
            home: FactoryHome::under(temp),
        },
        keys,
        runtime,
    )
}

#[tokio::test]
async fn the_cache_volume_is_reused_when_it_exists_and_created_once_when_not() {
    // Exists already: no create.
    let runtime = FakeRuntime::default().with_answer(&["volume", "exists"], Exit::of(0, ""));
    ensure_cache_volume(&runtime, ContainerRuntime::Podman)
        .await
        .expect("an existing volume is fine");
    assert_eq!(
        runtime.calls_matching(&["volume", "exists", CACHE_VOLUME]),
        1
    );
    assert_eq!(runtime.calls_matching(&["volume", "create"]), 0);
    let first = &runtime.calls()[0];
    assert_eq!(first.program, "podman");
    assert!(
        first.env.is_none(),
        "housekeeping keeps the factory's environment"
    );

    // Missing: created once.
    let runtime = FakeRuntime::default()
        .with_answer(&["volume", "exists"], Exit::of(1, ""))
        .with_answer(&["volume", "create"], Exit::of(0, "anvil-factory-cargo\n"));
    ensure_cache_volume(&runtime, ContainerRuntime::Podman)
        .await
        .expect("created");
    assert_eq!(
        runtime.calls_matching(&["volume", "create", CACHE_VOLUME]),
        1
    );

    // A refusal because it exists is not an error either.
    let runtime = FakeRuntime::default()
        .with_answer(&["volume", "exists"], Exit::of(1, ""))
        .with_answer(
            &["volume", "create"],
            Exit::of(
                125,
                "Error: volume with name anvil-factory-cargo already exists: volume already exists\n",
            ),
        );
    ensure_cache_volume(&runtime, ContainerRuntime::Podman)
        .await
        .expect("already exists is success");

    // Any other refusal is.
    let runtime = FakeRuntime::default()
        .with_answer(&["volume", "exists"], Exit::of(1, ""))
        .with_answer(
            &["volume", "create"],
            Exit::of(125, "Error: cannot connect to podman\n"),
        );
    let error = ensure_cache_volume(&runtime, ContainerRuntime::Podman)
        .await
        .expect_err("refused");
    assert!(
        error
            .to_string()
            .contains("podman volume create anvil-factory-cargo: Error: cannot connect"),
        "{error}"
    );

    // No container: nothing to do.
    let runtime = FakeRuntime::default();
    ensure_cache_volume(&runtime, ContainerRuntime::None)
        .await
        .expect("nothing to do");
    assert!(runtime.calls().is_empty());
}

#[tokio::test]
async fn a_container_that_never_starts_is_an_error_not_an_implementer_result() {
    let temp = tempfile::tempdir().expect("tempdir");
    let worktree = FactoryHome::under(temp.path()).work_dir("T2");
    std::fs::create_dir_all(&worktree).expect("mkdir");
    let deadline = Instant::now() + Duration::from_secs(60);

    // podman exits 125 when it cannot start the container: the live failure.
    let runtime = Arc::new(FakeRuntime::default().with_answer(
        &["anvil", "--yes"],
        Exit::of(
            125,
            "Resolving \"localhost/anvil-factory\" using unqualified-search registries\nTrying to pull localhost/anvil-factory:latest...\nError: initializing source docker://localhost/anvil-factory:latest: pinging container registry localhost: connection refused\n",
        ),
    ));
    let sandbox = podman_sandbox(temp.path(), Arc::clone(&runtime));
    let error = sandbox
        .run(&worktree, "do the thing", deadline)
        .await
        .expect_err("podman's failure is the factory's");
    assert!(matches!(error, FactoryError::Sandbox(_)), "{error:?}");
    let text = error.to_string();
    assert!(
        text.contains("podman could not start the implementer (exit 125)"),
        "{text}"
    );
    assert!(
        text.contains("Error: initializing source docker://localhost/anvil-factory:latest"),
        "{text}"
    );
    let call = &runtime.calls()[0];
    assert_eq!(call.program, "podman");
    assert_eq!(call.args[0], "run");
    assert!(
        call.env.is_some(),
        "sandbox commands get the filtered environment"
    );

    // Any failure before anvil's session line is the same.
    let runtime = Arc::new(FakeRuntime::default().with_answer(
        &["anvil", "--yes"],
        Exit::of(
            1,
            "Error: crun: executable file `anvil` not found in $PATH\n",
        ),
    ));
    let error = podman_sandbox(temp.path(), runtime)
        .run(&worktree, "do the thing", deadline)
        .await
        .expect_err("never started");
    assert!(
        error
            .to_string()
            .contains("exit 1 before anvil's session line"),
        "{error}"
    );

    // After the session line, a failure is anvil's and stays a result.
    let runtime = Arc::new(FakeRuntime::default().with_answer(
        &["anvil", "--yes"],
        Exit::of(
            1,
            "session 01ARZ3NDEKTSV4RRFFQ69G5FAV\nerror: provider refused\n",
        ),
    ));
    let outcome = podman_sandbox(temp.path(), runtime)
        .run(&worktree, "do the thing", deadline)
        .await
        .expect("anvil ran");
    assert_eq!(outcome.status, ImplementerStatus::Failed { code: Some(1) });

    // A clean exit needs no session line.
    let outcome = podman_sandbox(temp.path(), Arc::new(FakeRuntime::default()))
        .run(&worktree, "do the thing", deadline)
        .await
        .expect("anvil ran");
    assert_eq!(outcome.status, ImplementerStatus::Done);
}

#[tokio::test]
async fn preflight_wants_the_image_built_and_the_volume_creatable() {
    let image = "localhost/anvil-factory:latest";

    // Image missing: the exact build command.
    let runtime = FakeRuntime::default().with_answer(&["image", "exists"], Exit::of(1, ""));
    let error = preflight(&runtime, ContainerRuntime::Podman, image)
        .await
        .expect_err("not built");
    assert_eq!(
        error.to_string(),
        "sandbox: worker image localhost/anvil-factory:latest is not built; run `podman build -t localhost/anvil-factory:latest -f deploy/nuc/Containerfile deploy/nuc` in the repository (or `just image`)"
    );
    assert_eq!(runtime.calls_matching(&["image", "exists", image]), 1);
    assert_eq!(runtime.calls_matching(&["volume"]), 0);

    // Image built, volume there: ready, and only questions were asked.
    let runtime = FakeRuntime::default()
        .with_answer(&["image", "exists"], Exit::of(0, ""))
        .with_answer(&["volume", "exists"], Exit::of(0, ""));
    preflight(&runtime, ContainerRuntime::Podman, image)
        .await
        .expect("ready");
    let asked: Vec<Vec<String>> = runtime.calls().into_iter().map(|c| c.args).collect();
    assert_eq!(
        asked,
        vec![
            vec!["image".to_owned(), "exists".to_owned(), image.to_owned()],
            vec![
                "volume".to_owned(),
                "exists".to_owned(),
                CACHE_VOLUME.to_owned()
            ],
        ]
    );

    // Image built, volume refused: the volume's error.
    let runtime = FakeRuntime::default()
        .with_answer(&["image", "exists"], Exit::of(0, ""))
        .with_answer(&["volume", "exists"], Exit::of(1, ""))
        .with_answer(
            &["volume", "create"],
            Exit::of(125, "Error: no space left\n"),
        );
    let error = preflight(&runtime, ContainerRuntime::Podman, image)
        .await
        .expect_err("volume");
    assert!(
        error
            .to_string()
            .contains("volume create anvil-factory-cargo: Error: no space left"),
        "{error}"
    );

    // Docker has no `exists` verbs: inspect the image, create the volume outright.
    let runtime = FakeRuntime::default();
    preflight(&runtime, ContainerRuntime::Docker, image)
        .await
        .expect("ready");
    assert_eq!(runtime.calls_matching(&["image", "inspect", image]), 1);
    assert_eq!(
        runtime.calls_matching(&["volume", "create", CACHE_VOLUME]),
        1
    );
    assert_eq!(runtime.calls_matching(&["volume", "exists"]), 0);

    // No container: nothing to check.
    let runtime = FakeRuntime::default();
    preflight(&runtime, ContainerRuntime::None, image)
        .await
        .expect("nothing to check");
    assert!(runtime.calls().is_empty());
}
