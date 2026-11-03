//! `factory update`: stop the service, pull `main`, rebuild, refresh the
//! worker image and the unit, and start the service again.
//!
//! This is what `deploy/nuc/bootstrap.sh` does after its first run, as one
//! command to type on the NUC. A ticket in progress is allowed to finish
//! first: the pause flag keeps the loop from claiming another one, and the
//! state file says when the worker is idle.
//!
//! anvil is built twice: for the host, where `anvil doctor` and the eval
//! harness run it, and as a static musl binary for the worker container,
//! whose glibc is older than the host's. `ldd` proves the second one static
//! before the service starts again.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anvil_types::factory_config::{DEFAULT_ANVIL_BIN, IMPLEMENTER_TARGET};
use anvil_types::{ContainerRuntime, FactoryConfig};
use anyhow::Context;
use factory_core::sandbox::snap_overrides;
use factory_core::{FactoryHome, InProgress, ServiceState, service};
use sha2::{Digest, Sha256};

use crate::install::{InstallOptions, UNIT_NAME, install};

/// The pause reason the update sets while it waits.
pub const PAUSE_REASON: &str = "factory update";

/// The worker image's build context: the repository root, so the recipe can
/// read `rust-toolchain.toml` (`.dockerignore` lets in only what it copies).
const IMAGE_CONTEXT: &str = ".";

/// The image recipe.
const CONTAINERFILE: &str = "deploy/nuc/Containerfile";

/// Every file that decides what the image contains, relative to the
/// repository root; their hash is the image's recipe label.
pub const RECIPE_FILES: [&str; 3] = [
    "rust-toolchain.toml",
    CONTAINERFILE,
    "deploy/nuc/entrypoint.sh",
];

/// The image label that holds the recipe hash it was built from.
pub const RECIPE_LABEL: &str = "factory.recipe";

/// The apt package that provides `musl-gcc`, which the C parts of the TLS
/// stack need for the musl target.
const MUSL_PACKAGE: &str = "musl-tools";

/// What the update needs.
pub struct UpdateOptions {
    /// The clone of the monorepo, `~/factory-repo` on the NUC.
    pub repo_root: PathBuf,
    /// `~/.factory`.
    pub home: FactoryHome,
    /// `$XDG_CONFIG_HOME`, normally `~/.config`.
    pub config_home: PathBuf,
    /// Stop at once instead of waiting for the ticket in progress.
    pub now: bool,
    /// How often the state file is read while waiting.
    pub poll: Duration,
    /// Rebuild the worker image even when its recipe has not changed.
    pub rebuild_image: bool,
    /// This process is the freshly built factory an older one handed the
    /// update to after its pull, so it must not hand off again.
    pub handed_off: bool,
}

/// One external command of the rebuild, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    /// What it does, for the console.
    pub label: String,
    /// The program.
    pub program: String,
    /// Its arguments.
    pub args: Vec<String>,
}

/// Runs the whole update, printing each stage as it goes.
///
/// # Arguments
///
/// * `options` - Where the repository and the home are, and how to stop.
///
/// # Returns
///
/// Nothing; the console shows every stage and the child processes' output.
///
/// # Errors
///
/// When `systemctl`, `git`, `cargo` or the container runtime fails, or
/// `factory.toml` cannot be read after the pull. The pause flag the update
/// set is cleared on every path once the service was stopped.
pub fn update(options: &UpdateOptions) -> anyhow::Result<()> {
    let root = &options.repo_root;
    let running = systemctl_ok(&["--user", "is-active", "--quiet", UNIT_NAME]);
    if running {
        // A pause set by hand or by the service itself stays as it is; only a
        // flag this update sets is cleared again.
        let was_paused = service::pause_reason(&options.home);
        if was_paused.is_none() {
            service::pause(&options.home, PAUSE_REASON).context("setting the pause flag")?;
        }
        let stranded = wait_for_idle(&options.home, options.poll, options.now, &mut |line| {
            println!("{line}")
        })?;
        let stopped = run(
            &Step::new("service: stop", "systemctl", &["--user", "stop", UNIT_NAME]),
            root,
        );
        if was_paused.is_none() {
            service::resume(&options.home).context("clearing the pause flag")?;
        }
        stopped?;
        if let Some(ticket) = stranded {
            println!(
                "warning: ticket {} ({}) was at `{}` and stays in tickets/claimed/ on factory/state; to retry it, move the file to tickets/open/, set status = \"open\" and drop claimed_at and claimed_by",
                ticket.id, ticket.title, ticket.stage
            );
        }
    } else {
        println!("== service: not running");
    }

    let before = git_head(root);
    run(
        &Step::new("repo: pull", "git", &["pull", "--ff-only", "--quiet"]),
        root,
    )?;
    let after = git_head(root);
    if before == after {
        println!("repo: already at {after}");
    } else {
        println!("repo: {before} -> {after}");
    }
    // The pulled factory.toml may hold settings this binary does not know,
    // so the new code reads it: build the new factory and let it finish.
    if let Some(steps) = handoff_steps(before != after, options.handed_off, options.rebuild_image) {
        for step in steps {
            run(&step, root)?;
        }
        return Ok(());
    }

    let factory = crate::wiring::read_factory(root)?;
    let recipe = recipe_hash(root)?;
    let reason = runtime_program(factory.worker.runtime).and_then(|program| {
        let built = built_recipe(program, &factory.worker.image);
        image_build_reason(built.as_deref(), &recipe, options.rebuild_image)
    });
    match (&reason, factory.worker.runtime) {
        (_, ContainerRuntime::None) => println!("image: none (the worker runs on the host)"),
        (None, _) => println!("image: up to date (recipe {})", short(&recipe)),
        (Some(why), _) => println!("image: rebuilding ({why})"),
    }
    let image = reason.map(|_| recipe);
    for step in rebuild_steps(&factory, musl_gcc_present(), image.as_deref()) {
        run(&step, root)?;
    }
    verify_static(root)?;
    // The freshly built factory runs the selftest, so the code under test is
    // the code the service will run; on a failure the unit is not started.
    run(&selftest_step(), root).context(
        "the selftest failed, so the service stays stopped; fix the failing step and run `factory update` again",
    )?;

    println!("== service: install and start");
    for line in install(&InstallOptions {
        repo_root: root.clone(),
        config_home: options.config_home.clone(),
        run_systemctl: true,
    })? {
        println!("{line}");
    }
    Ok(())
}

/// The steps that hand the rest of an update to the newly pulled factory.
///
/// # Arguments
///
/// * `pulled` - Whether the pull moved `HEAD`.
/// * `handed_off` - Whether this process already is the handed-off factory.
/// * `rebuild_image` - Passed on as `--rebuild-image`.
///
/// # Returns
///
/// Building `factory-cli`, then its `update --now --handed-off`; `None` when
/// nothing was pulled or the handoff already happened.
pub fn handoff_steps(pulled: bool, handed_off: bool, rebuild_image: bool) -> Option<Vec<Step>> {
    if !pulled || handed_off {
        return None;
    }
    let mut args = vec!["update", "--now", "--handed-off"];
    if rebuild_image {
        args.push("--rebuild-image");
    }
    Some(vec![
        Step::new(
            "build: factory (the new code finishes the update)",
            "cargo",
            &["build", "--release", "-p", "factory-cli"],
        ),
        Step::new(
            "handoff: the new factory updates",
            "target/release/factory",
            &args,
        ),
    ])
}

/// The build commands, in order: the host binaries, the musl toolchain and
/// the static anvil for the container, then the worker image when a recipe
/// hash says to build it and the worker does not run on the host.
///
/// # Arguments
///
/// * `factory` - `factory.toml`, which names the runtime and the image tag.
/// * `have_musl_gcc` - Whether `musl-gcc` is installed; when not, apt installs it.
/// * `image_recipe` - The recipe hash to build the image with and label it
///   by, or `None` to leave the image as it is.
///
/// # Returns
///
/// The steps; `cargo build` first.
pub fn rebuild_steps(
    factory: &FactoryConfig,
    have_musl_gcc: bool,
    image_recipe: Option<&str>,
) -> Vec<Step> {
    let mut steps = vec![Step::new(
        "build: anvil and factory (release)",
        "cargo",
        &["build", "--release", "-p", "anvil-cli", "-p", "factory-cli"],
    )];
    if !have_musl_gcc {
        steps.push(Step::new(
            &format!("toolchain: {MUSL_PACKAGE}"),
            "sudo",
            &["apt-get", "install", "-y", "-qq", MUSL_PACKAGE],
        ));
    }
    steps.push(Step::new(
        &format!("toolchain: rustup target {IMPLEMENTER_TARGET}"),
        "rustup",
        &["target", "add", IMPLEMENTER_TARGET],
    ));
    steps.push(Step::new(
        &format!("build: anvil for the implementer ({IMPLEMENTER_TARGET}, release)"),
        "cargo",
        &[
            "build",
            "--release",
            "--target",
            IMPLEMENTER_TARGET,
            "-p",
            "anvil-cli",
        ],
    ));
    let (Some(runtime), Some(recipe)) = (runtime_program(factory.worker.runtime), image_recipe)
    else {
        return steps;
    };
    steps.push(Step::new(
        &format!("image: {}", factory.worker.image),
        runtime,
        &[
            "build",
            "-q",
            "-t",
            &factory.worker.image,
            "--label",
            &format!("{RECIPE_LABEL}={recipe}"),
            "-f",
            CONTAINERFILE,
            IMAGE_CONTEXT,
        ],
    ));
    steps
}

/// The hash of everything that decides the worker image's contents.
///
/// Each of [`RECIPE_FILES`] contributes its path, its length and its bytes,
/// so moving content from one file to another changes the hash too.
///
/// # Arguments
///
/// * `root` - The repository root.
///
/// # Returns
///
/// The SHA-256, as lowercase hex.
///
/// # Errors
///
/// When one of the files cannot be read.
pub fn recipe_hash(root: &Path) -> anyhow::Result<String> {
    let mut hasher = Sha256::new();
    for relative in RECIPE_FILES {
        let path = root.join(relative);
        let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        hasher.update(relative.as_bytes());
        hasher.update([0]);
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(&bytes);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// Why the image must be built, or `None` when it is up to date.
///
/// # Arguments
///
/// * `built` - The recipe label on the existing image; `None` when the image
///   or its label is missing.
/// * `wanted` - The recipe hash of the working tree.
/// * `forced` - `factory update --rebuild-image`.
///
/// # Returns
///
/// The reason, for the console, or `None` to skip the build.
pub fn image_build_reason(built: Option<&str>, wanted: &str, forced: bool) -> Option<String> {
    match built {
        _ if forced => Some("--rebuild-image".to_owned()),
        None => Some("no image with a recipe label".to_owned()),
        Some(built) if built != wanted => Some(format!(
            "recipe changed: {} -> {}",
            short(built),
            short(wanted)
        )),
        Some(_) => None,
    }
}

/// The recipe label on `image`, or `None` when the image or the label is missing.
fn built_recipe(runtime: &str, image: &str) -> Option<String> {
    let format = format!("{{{{index .Config.Labels \"{RECIPE_LABEL}\"}}}}");
    let mut command = Command::new(runtime);
    for name in snap_overrides() {
        command.env_remove(name);
    }
    let out = command
        .args(["image", "inspect", "--format", &format, image])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|out| out.status.success())?;
    let label = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    // Go templates print `<no value>` for a missing key.
    (!label.is_empty() && label != "<no value>").then_some(label)
}

/// The container runtime's program, or `None` when the worker runs on the host.
fn runtime_program(runtime: ContainerRuntime) -> Option<&'static str> {
    match runtime {
        ContainerRuntime::Podman => Some("podman"),
        ContainerRuntime::Docker => Some("docker"),
        ContainerRuntime::None => None,
    }
}

/// The first 12 characters of a hash, for the console.
fn short(hash: &str) -> &str {
    hash.get(..12).unwrap_or(hash)
}

/// The selftest the update runs after the rebuild, with the new binary.
pub fn selftest_step() -> Step {
    Step::new(
        "selftest: the worker pipeline against a local bare repository",
        "target/release/factory",
        &["selftest"],
    )
}

/// Waits until the state file shows no ticket in progress.
///
/// # Arguments
///
/// * `home` - `~/.factory`, which holds the state file.
/// * `poll` - How long to sleep between reads.
/// * `now` - Do not wait; report the ticket in progress instead.
/// * `log` - Receives one line per wait.
///
/// # Returns
///
/// The ticket that is still in progress when `now` cut the wait short,
/// otherwise `None`.
///
/// # Errors
///
/// When the state file exists but cannot be parsed.
pub fn wait_for_idle(
    home: &FactoryHome,
    poll: Duration,
    now: bool,
    log: &mut dyn FnMut(String),
) -> anyhow::Result<Option<InProgress>> {
    loop {
        let state = ServiceState::load(&home.state_file()).context("reading the state file")?;
        let Some(current) = state.in_progress else {
            return Ok(None);
        };
        if now {
            return Ok(Some(current));
        }
        log(format!(
            "waiting for ticket {} ({}) at `{}`, claimed {}; `factory update --now` stops at once",
            current.id,
            current.title,
            current.stage,
            current.since.format("%Y-%m-%d %H:%M UTC")
        ));
        std::thread::sleep(poll);
    }
}

impl Step {
    /// A step from borrowed parts.
    fn new(label: &str, program: &str, args: &[&str]) -> Self {
        Self {
            label: label.to_owned(),
            program: program.to_owned(),
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
        }
    }
}

/// Runs one step in `dir` with the console attached, so build output shows.
fn run(step: &Step, dir: &Path) -> anyhow::Result<()> {
    println!("== {}", step.label);
    let mut command = Command::new(&step.program);
    for name in snap_overrides() {
        command.env_remove(name);
    }
    let status = command
        .args(&step.args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .status()
        .with_context(|| format!("running {}", step.program))?;
    if !status.success() {
        anyhow::bail!("{} {} failed ({status})", step.program, step.args.join(" "));
    }
    Ok(())
}

/// Proves the implementer's anvil is static: prints what `ldd` says about
/// it and fails when any shared library is listed.
///
/// `ldd` is run for its text, not its status: glibc's `ldd` exits 1 with
/// "not a dynamic executable" for a plain static binary and 0 with
/// "statically linked" for a static-pie one, which is what the musl target
/// produces.
///
/// # Arguments
///
/// * `root` - The repository, under which `target/` holds the build.
///
/// # Errors
///
/// When `ldd` cannot run, or the binary links shared libraries.
pub fn verify_static(root: &Path) -> anyhow::Result<()> {
    println!("== verify: ldd {DEFAULT_ANVIL_BIN}");
    let text = verify_static_binary(&root.join(DEFAULT_ANVIL_BIN))?;
    print!("{text}");
    Ok(())
}

/// Runs `ldd` on one binary and fails unless it is static; the check behind
/// [`verify_static`], for any path.
///
/// # Arguments
///
/// * `binary` - The executable to check.
///
/// # Returns
///
/// `ldd`'s output.
///
/// # Errors
///
/// When `ldd` cannot run, or the binary links shared libraries.
pub fn verify_static_binary(binary: &Path) -> anyhow::Result<String> {
    let out = Command::new("ldd")
        .arg(binary)
        .stdin(Stdio::null())
        .output()
        .context("running ldd")?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    static_verdict(&text).map(|()| text).map_err(|libraries| {
        anyhow::anyhow!(
            "{} is not static; it links {libraries}, which the worker image cannot provide",
            binary.display()
        )
    })
}

/// Judges `ldd`'s output: `Ok` for a static binary, else the shared
/// libraries it names, comma-separated.
///
/// # Arguments
///
/// * `ldd_output` - stdout and stderr of `ldd <binary>`.
///
/// # Errors
///
/// The lines that name a shared library, when `ldd` did not call the
/// binary static.
pub fn static_verdict(ldd_output: &str) -> Result<(), String> {
    if ldd_output.contains("statically linked") || ldd_output.contains("not a dynamic executable") {
        return Ok(());
    }
    let libraries: Vec<&str> = ldd_output
        .lines()
        .map(str::trim)
        .filter(|line| line.contains("=>") || line.contains(".so"))
        .collect();
    if libraries.is_empty() {
        return Err(format!("(ldd said: {})", ldd_output.trim()));
    }
    Err(libraries.join(", "))
}

/// Whether `musl-gcc` runs; `false` when it is not installed.
fn musl_gcc_present() -> bool {
    Command::new("musl-gcc")
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Whether a `systemctl` query exits 0; `false` when systemctl is missing.
///
/// # Arguments
///
/// * `args` - The arguments after `systemctl`.
///
/// # Returns
///
/// `true` when systemctl ran and exited 0.
pub fn systemctl_ok(args: &[&str]) -> bool {
    Command::new("systemctl")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// The short sha of `HEAD`, or `?` when git cannot say.
fn git_head(dir: &Path) -> String {
    Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(dir)
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .unwrap_or_else(|| "?".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(runtime: ContainerRuntime) -> FactoryConfig {
        let mut factory: FactoryConfig =
            toml::from_str(&FactoryConfig::template("owner/repo")).expect("template");
        factory.worker.runtime = runtime;
        factory.worker.image = "localhost/test:latest".to_owned();
        factory
    }

    #[test]
    fn the_rebuild_builds_the_binaries_then_the_image_for_the_configured_runtime() {
        let steps = rebuild_steps(&config(ContainerRuntime::Podman), true, Some("abc"));
        assert_eq!(steps.len(), 4);
        assert_eq!(steps[0].program, "cargo");
        assert_eq!(steps[0].args[..3], ["build", "--release", "-p"]);
        assert_eq!(steps[1].program, "rustup");
        assert_eq!(
            steps[1].args,
            ["target", "add", "x86_64-unknown-linux-musl"]
        );
        assert_eq!(steps[2].program, "cargo");
        assert_eq!(
            steps[2].args,
            [
                "build",
                "--release",
                "--target",
                "x86_64-unknown-linux-musl",
                "-p",
                "anvil-cli"
            ]
        );
        assert_eq!(steps[3].program, "podman");
        assert_eq!(
            steps[3].args,
            [
                "build",
                "-q",
                "-t",
                "localhost/test:latest",
                "--label",
                "factory.recipe=abc",
                "-f",
                "deploy/nuc/Containerfile",
                "."
            ]
        );
        assert_eq!(
            rebuild_steps(&config(ContainerRuntime::Docker), true, Some("abc"))[3].program,
            "docker"
        );
        let on_host = rebuild_steps(&config(ContainerRuntime::None), true, Some("abc"));
        assert_eq!(on_host.len(), 3);
        assert_eq!(
            on_host[2].args[2..4],
            ["--target", "x86_64-unknown-linux-musl"]
        );
    }

    #[test]
    fn an_up_to_date_image_is_not_rebuilt() {
        let steps = rebuild_steps(&config(ContainerRuntime::Podman), true, None);
        assert_eq!(steps.len(), 3);
        assert!(steps.iter().all(|step| step.program != "podman"));
    }

    #[test]
    fn a_missing_musl_gcc_is_installed_before_the_target_is_added() {
        let steps = rebuild_steps(&config(ContainerRuntime::Podman), false, Some("abc"));
        assert_eq!(steps.len(), 5);
        assert_eq!(steps[1].program, "sudo");
        assert_eq!(
            steps[1].args,
            ["apt-get", "install", "-y", "-qq", "musl-tools"]
        );
        assert_eq!(steps[2].program, "rustup");
        assert_eq!(
            steps[3].args[2..4],
            ["--target", "x86_64-unknown-linux-musl"]
        );
        assert_eq!(steps[4].program, "podman");
    }

    #[test]
    fn the_image_is_built_when_missing_changed_or_forced() {
        let wanted = "def4560000000000";
        assert_eq!(image_build_reason(Some(wanted), wanted, false), None);
        assert_eq!(
            image_build_reason(Some(wanted), wanted, true).as_deref(),
            Some("--rebuild-image")
        );
        assert_eq!(
            image_build_reason(None, wanted, false).as_deref(),
            Some("no image with a recipe label")
        );
        assert_eq!(
            image_build_reason(Some("abc1230000000000"), wanted, false).as_deref(),
            Some("recipe changed: abc123000000 -> def456000000")
        );
    }

    #[test]
    fn the_recipe_hash_changes_with_every_input_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        for relative in RECIPE_FILES {
            let path = dir.path().join(relative);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            std::fs::write(&path, relative).expect("write");
        }
        let first = recipe_hash(dir.path()).expect("hash");
        assert_eq!(first.len(), 64);
        assert_eq!(first, recipe_hash(dir.path()).expect("hash"), "stable");
        let mut seen = vec![first];
        for relative in RECIPE_FILES {
            std::fs::write(dir.path().join(relative), "changed").expect("write");
            let hash = recipe_hash(dir.path()).expect("hash");
            assert!(!seen.contains(&hash), "{relative} must change the hash");
            seen.push(hash);
        }
        std::fs::remove_file(dir.path().join(RECIPE_FILES[0])).expect("rm");
        assert!(recipe_hash(dir.path()).is_err());
    }

    #[test]
    fn ldd_output_is_judged_static_or_names_the_libraries() {
        assert_eq!(static_verdict("\tstatically linked\n"), Ok(()));
        assert_eq!(static_verdict("\tnot a dynamic executable\n"), Ok(()));
        let dynamic = "\tlinux-vdso.so.1 (0x00007ffd)\n\tlibgcc_s.so.1 => /lib/x86_64-linux-gnu/libgcc_s.so.1 (0x00007f)\n\tlibc.so.6 => /lib/x86_64-linux-gnu/libc.so.6 (0x00007f)\n\t/lib64/ld-linux-x86-64.so.2 (0x00007f)\n";
        let error = static_verdict(dynamic).expect_err("dynamic");
        assert!(
            error.contains("libc.so.6 => /lib/x86_64-linux-gnu/libc.so.6"),
            "{error}"
        );
        assert!(error.contains("libgcc_s.so.1"), "{error}");
        let error = static_verdict("ldd: no such file\n").expect_err("ldd failed");
        assert!(error.contains("ldd said: ldd: no such file"), "{error}");
    }

    fn in_progress(stage: &str) -> ServiceState {
        ServiceState {
            in_progress: Some(InProgress {
                id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".to_owned(),
                title: "Add a hello skill".to_owned(),
                stage: stage.to_owned(),
                since: "2026-10-30T10:00:00Z".parse().expect("rfc3339"),
            }),
            ..ServiceState::default()
        }
    }

    #[test]
    fn waits_until_the_state_file_shows_nothing_in_progress() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = FactoryHome::under(dir.path());
        std::fs::create_dir_all(home.root()).expect("mkdir");
        let path = home.state_file();
        in_progress("gate B").save(&path).expect("save");
        let clearer = {
            let path = path.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(150));
                ServiceState::default().save(&path).expect("save");
            })
        };
        let mut lines = Vec::new();
        let stranded = wait_for_idle(&home, Duration::from_millis(20), false, &mut |line| {
            lines.push(line)
        })
        .expect("waits");
        clearer.join().expect("thread");
        assert!(stranded.is_none());
        assert!(!lines.is_empty());
        assert!(
            lines[0].contains("01ARZ3NDEKTSV4RRFFQ69G5FAV"),
            "{}",
            lines[0]
        );
        assert!(lines[0].contains("gate B"), "{}", lines[0]);
        assert!(lines[0].contains("--now"), "{}", lines[0]);
    }

    #[test]
    fn a_pull_hands_the_update_to_the_new_factory_once() {
        assert_eq!(handoff_steps(false, false, false), None);
        assert_eq!(handoff_steps(true, true, false), None);
        let steps = handoff_steps(true, false, true).expect("handoff");
        assert_eq!(steps[0].args, ["build", "--release", "-p", "factory-cli"]);
        assert_eq!(steps[1].program, "target/release/factory");
        assert_eq!(
            steps[1].args,
            ["update", "--now", "--handed-off", "--rebuild-image"]
        );
    }

    #[test]
    fn now_reports_the_ticket_in_progress_without_waiting() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = FactoryHome::under(dir.path());
        std::fs::create_dir_all(home.root()).expect("mkdir");
        in_progress("implementer attempt 1")
            .save(&home.state_file())
            .expect("save");
        let mut lines = Vec::new();
        let stranded = wait_for_idle(&home, Duration::from_secs(60), true, &mut |line| {
            lines.push(line)
        })
        .expect("returns")
        .expect("the ticket");
        assert_eq!(stranded.stage, "implementer attempt 1");
        assert!(lines.is_empty());
    }

    #[test]
    fn no_state_file_means_idle() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = FactoryHome::under(dir.path());
        let stranded = wait_for_idle(&home, Duration::from_secs(60), false, &mut |_| {})
            .expect("missing file is idle");
        assert!(stranded.is_none());
    }
}
