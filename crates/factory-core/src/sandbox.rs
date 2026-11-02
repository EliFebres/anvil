//! The sandbox: anvil and the build tools in a container, or on the host
//! when `[worker] runtime = "none"`.
//!
//! The container sees the worktree, a per-ticket anvil home (configuration
//! and the `factory` profile, never a key file), a read-only cargo registry
//! cache and the egress proxy's socket. Its environment holds the provider
//! API keys and nothing else from the factory's; the GitHub token and the
//! webhook URL never enter it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anvil_types::{ContainerRuntime, ProviderConfig};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::error::{FactoryError, Result};
use crate::paths::FactoryHome;
use crate::ports::{BoxFuture, CheckOutcome, Implementer, ImplementerOutcome, ImplementerStatus};
use crate::proxy::{self, Allowlist, Denied};
use crate::usage;

/// The named volume that caches the cargo registry across tickets.
pub const CACHE_VOLUME: &str = "anvil-factory-cargo";

/// Where the registry cache is mounted in the image.
const CACHE_MOUNT: &str = "/usr/local/cargo/registry";

/// Where the worktree is mounted.
const WORK_MOUNT: &str = "/work";

/// Where the anvil home is mounted.
const HOME_MOUNT: &str = "/anvil-home";

/// Where the proxy socket is mounted; `entrypoint.sh` bridges it to port 3128.
const PROXY_MOUNT: &str = "/run/factory/proxy.sock";

/// The proxy URL inside the container.
const PROXY_URL: &str = "http://127.0.0.1:3128";

/// Variables the host passes through besides the keys, so programs can run at all.
const PASSTHROUGH_ENV: [&str; 9] = [
    "PATH",
    "HOME",
    "USERPROFILE",
    "SYSTEMROOT",
    "TEMP",
    "TMP",
    "LANG",
    "TERM",
    "CARGO_HOME",
];

/// Lines of output kept from a run.
const TAIL_LINES: usize = 60;

/// Bytes of output kept in memory.
const OUTPUT_CAP: usize = 4 * 1024 * 1024;

/// The profile anvil runs with.
const PROFILE: &str = "factory";

/// What the sandbox is built from.
#[derive(Debug, Clone)]
pub struct SandboxConfig {
    /// podman, docker or none.
    pub runtime: ContainerRuntime,
    /// The image built from `deploy/nuc/Containerfile`.
    pub image: String,
    /// The anvil binary inside the container, or a host path with `runtime = "none"`.
    pub anvil_bin: String,
    /// `just` as invoked for Gate A.
    pub just_bin: String,
    /// `cargo` as invoked for the release build.
    pub cargo_bin: String,
    /// Hosts the container may reach besides every provider's base URL.
    pub egress_allow: Vec<String>,
    /// `[providers]` from the anvil configuration: keys to pass, prices, hosts.
    pub providers: BTreeMap<String, ProviderConfig>,
    /// The anvil configuration written into the per-ticket home.
    pub anvil_config: String,
    /// `deploy/nuc/anvil-factory.toml`, written as the `factory` profile.
    pub profile: String,
    /// `~/.factory`.
    pub home: FactoryHome,
}

/// Looks a key variable up; the process environment, or a test's table.
pub type KeyLookup = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// One process the sandbox wants run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    /// The executable: the container runtime, or the program itself with `runtime = "none"`.
    pub program: String,
    /// Its arguments.
    pub args: Vec<String>,
    /// The environment: `None` inherits the factory's (runtime housekeeping),
    /// `Some` replaces it entirely (sandbox commands).
    pub env: Option<Vec<(String, String)>>,
    /// The working directory, when it matters.
    pub cwd: Option<PathBuf>,
}

/// How a sandbox command ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exit {
    /// The exit code, `None` when killed by a signal or the deadline.
    pub code: Option<i32>,
    /// Whether the deadline passed and the command was killed.
    pub timed_out: bool,
    /// stdout then stderr, as text.
    pub output: String,
}

impl Exit {
    /// An exit with a code and its output, as a finished command reports.
    ///
    /// # Arguments
    ///
    /// * `code` - The exit code.
    /// * `output` - stdout then stderr.
    pub fn of(code: i32, output: &str) -> Self {
        Self {
            code: Some(code),
            timed_out: false,
            output: output.to_owned(),
        }
    }
}

/// Runs the processes the sandbox asks for: the container runtime on the
/// factory host, or a fake in tests.
pub trait CommandRunner: Send + Sync {
    /// Runs the process until it exits or the deadline passes, collecting its output.
    ///
    /// # Arguments
    ///
    /// * `invocation` - What to run.
    /// * `deadline` - When to kill it.
    ///
    /// # Errors
    ///
    /// [`FactoryError::Spawn`] when the program cannot be started.
    fn run<'a>(&'a self, invocation: Invocation, deadline: Instant) -> BoxFuture<'a, Result<Exit>>;
}

/// The runner the factory uses: real processes on this machine.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn run<'a>(&'a self, invocation: Invocation, deadline: Instant) -> BoxFuture<'a, Result<Exit>> {
        Box::pin(run_process(invocation, deadline))
    }
}

/// Runs the implementer and the build tools for one factory.
pub struct Sandbox {
    config: SandboxConfig,
    keys: KeyLookup,
    runner: Arc<dyn CommandRunner>,
    denied: Denied,
    proxy: Mutex<Option<proxy::ProxyHandle>>,
}

impl Sandbox {
    /// Builds the sandbox; keys come from the process environment.
    ///
    /// # Arguments
    ///
    /// * `config` - What to run and how.
    pub fn new(config: SandboxConfig) -> Self {
        Self::with_key_lookup(config, Arc::new(|name| std::env::var(name).ok()))
    }

    /// Builds the sandbox with its own source of key values.
    ///
    /// Tests use this because the workspace forbids `unsafe`, which setting
    /// environment variables needs.
    ///
    /// # Arguments
    ///
    /// * `config` - What to run and how.
    /// * `keys` - Returns the value of a key variable, or `None` when unset.
    pub fn with_key_lookup(config: SandboxConfig, keys: KeyLookup) -> Self {
        Self::with_runner(config, keys, Arc::new(SystemRunner))
    }

    /// Builds the sandbox over its own process runner, so tests can script
    /// what the container runtime answers.
    ///
    /// # Arguments
    ///
    /// * `config` - What to run and how.
    /// * `keys` - Returns the value of a key variable, or `None` when unset.
    /// * `runner` - Runs every process the sandbox asks for.
    pub fn with_runner(
        config: SandboxConfig,
        keys: KeyLookup,
        runner: Arc<dyn CommandRunner>,
    ) -> Self {
        Self {
            config,
            keys,
            runner,
            denied: Arc::new(Mutex::new(BTreeSet::new())),
            proxy: Mutex::new(None),
        }
    }

    /// Whether commands run in a container.
    pub fn containerized(&self) -> bool {
        self.config.runtime != ContainerRuntime::None
    }

    /// The ticket id a worktree folder is named after.
    fn ticket_id(worktree: &Path) -> String {
        worktree
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "ticket".to_owned())
    }

    /// The per-ticket anvil home, created and seeded on first use.
    ///
    /// # Arguments
    ///
    /// * `worktree` - The ticket's worktree.
    ///
    /// # Returns
    ///
    /// The home folder: `<home>/.anvil/config.toml` and
    /// `<home>/.anvil/profiles/factory.toml` exist, nothing else.
    ///
    /// # Errors
    ///
    /// When the files cannot be written.
    pub fn ticket_home(&self, worktree: &Path) -> Result<PathBuf> {
        let home = self.config.home.ticket_home(&Self::ticket_id(worktree));
        let anvil = home.join(".anvil");
        let profiles = anvil.join("profiles");
        std::fs::create_dir_all(&profiles)
            .map_err(|source| FactoryError::io("creating", &profiles, source))?;
        let config = anvil.join("config.toml");
        std::fs::write(&config, &self.config.anvil_config)
            .map_err(|source| FactoryError::io("writing", &config, source))?;
        let profile = profiles.join(format!("{PROFILE}.toml"));
        std::fs::write(&profile, &self.config.profile)
            .map_err(|source| FactoryError::io("writing", &profile, source))?;
        Ok(home)
    }

    /// The egress allowlist: the configured hosts plus every provider's host.
    pub fn allowlist(&self) -> Allowlist {
        let provider_hosts = self
            .config
            .providers
            .values()
            .filter_map(|provider| host_of(&provider.base_url));
        Allowlist::new(
            self.config
                .egress_allow
                .iter()
                .cloned()
                .chain(provider_hosts),
        )
    }

    /// The names of the variables the container receives: every provider's
    /// `api_key_env` that has a value.
    pub fn key_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .config
            .providers
            .values()
            .map(|provider| provider.api_key_env.clone())
            .filter(|name| (self.keys)(name).is_some())
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// The environment a sandbox command runs with: the keys, the
    /// passthrough variables and `ANVIL_HOME`.
    fn environment(&self, home: &Path) -> Vec<(String, String)> {
        let mut env: Vec<(String, String)> = self
            .key_names()
            .into_iter()
            .filter_map(|name| (self.keys)(&name).map(|value| (name, value)))
            .collect();
        for name in PASSTHROUGH_ENV {
            if let Ok(value) = std::env::var(name) {
                env.push((name.to_owned(), value));
            }
        }
        if !self.containerized() {
            env.push(("ANVIL_HOME".to_owned(), home.to_string_lossy().into_owned()));
        }
        env
    }

    /// The `podman run`/`docker run` arguments for a program in the worktree.
    ///
    /// # Arguments
    ///
    /// * `worktree` - The worktree, mounted at `/work`.
    /// * `home` - The per-ticket anvil home, mounted at `/anvil-home`.
    /// * `program` - The program and its arguments inside the container.
    /// * `cache_writable` - Mount the registry cache read-write (prewarming only).
    ///
    /// # Returns
    ///
    /// The arguments after the runtime's name.
    pub fn run_args(
        &self,
        worktree: &Path,
        home: &Path,
        program: &[&str],
        cache_writable: bool,
    ) -> Vec<String> {
        let id = Self::ticket_id(worktree);
        let mut args: Vec<String> = ["run", "--rm", "--network", "none", "--init"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        match self.config.runtime {
            ContainerRuntime::Podman => args.push("--userns=keep-id".to_owned()),
            ContainerRuntime::Docker => {
                args.push("--user".to_owned());
                args.push(current_uid_gid());
            }
            ContainerRuntime::None => {}
        }
        let cache_mode = if cache_writable { "" } else { ":ro" };
        args.extend([
            "-v".to_owned(),
            format!("{}:{WORK_MOUNT}", worktree.display()),
            "-w".to_owned(),
            WORK_MOUNT.to_owned(),
            "-v".to_owned(),
            format!("{}:{HOME_MOUNT}", home.display()),
            "-e".to_owned(),
            format!("ANVIL_HOME={HOME_MOUNT}"),
            "-v".to_owned(),
            format!("{CACHE_VOLUME}:{CACHE_MOUNT}{cache_mode}"),
            "-v".to_owned(),
            format!(
                "{}:{PROXY_MOUNT}",
                self.config.home.proxy_socket(&id).display()
            ),
        ]);
        for name in [
            "HTTPS_PROXY",
            "HTTP_PROXY",
            "https_proxy",
            "http_proxy",
            "CARGO_HTTP_PROXY",
        ] {
            args.push("-e".to_owned());
            args.push(format!("{name}={PROXY_URL}"));
        }
        args.push("-e".to_owned());
        args.push("NO_PROXY=127.0.0.1,localhost".to_owned());
        for name in self.key_names() {
            args.push("-e".to_owned());
            args.push(name);
        }
        args.push(self.config.image.clone());
        args.extend(program.iter().map(|s| (*s).to_owned()));
        args
    }

    /// The invocation for a program in the worktree, on the host or in the
    /// container, with the sandbox environment and nothing else.
    fn command(
        &self,
        worktree: &Path,
        home: &Path,
        program: &[&str],
        cache_writable: bool,
    ) -> Invocation {
        let env = Some(self.environment(home));
        match self.config.runtime {
            ContainerRuntime::None => {
                let (bin, rest) = program.split_first().unwrap_or((&"true", &[]));
                let (program, mut args) = if bin.ends_with(".sh") {
                    (
                        bash_program().to_string_lossy().into_owned(),
                        vec![(*bin).to_owned()],
                    )
                } else {
                    ((*bin).to_owned(), Vec::new())
                };
                args.extend(rest.iter().map(|s| (*s).to_owned()));
                Invocation {
                    program,
                    args,
                    env,
                    cwd: Some(worktree.to_path_buf()),
                }
            }
            runtime => Invocation {
                program: runtime_name(runtime).to_owned(),
                args: self.run_args(worktree, home, program, cache_writable),
                env,
                cwd: None,
            },
        }
    }

    /// Runs an invocation until it exits or the deadline passes.
    async fn run_until(&self, invocation: Invocation, deadline: Instant) -> Result<Exit> {
        self.runner.run(invocation, deadline).await
    }

    /// Starts the egress proxy for a ticket, replacing a previous one.
    fn start_proxy(&self, worktree: &Path) -> Result<()> {
        let socket = self.config.home.proxy_socket(&Self::ticket_id(worktree));
        let handle = proxy::start(&socket, self.allowlist(), Arc::clone(&self.denied))?;
        *lock(&self.proxy) = Some(handle);
        Ok(())
    }

    /// Stops the proxy.
    pub fn stop_proxy(&self) {
        *lock(&self.proxy) = None;
    }

    /// Warms the registry cache: `cargo fetch` with the cache mounted read-write.
    async fn prewarm(&self, worktree: &Path, home: &Path) -> Result<()> {
        ensure_cache_volume(self.runner.as_ref(), self.config.runtime).await?;
        let cargo = self.config.cargo_bin.clone();
        let command = self.command(worktree, home, &[cargo.as_str(), "fetch"], true);
        let exit = self
            .run_until(command, Instant::now() + Duration::from_secs(20 * 60))
            .await?;
        if exit.code != Some(0) {
            tracing::warn!(output = %exit.output, "cargo fetch did not finish; builds will fetch through the proxy");
        }
        Ok(())
    }

    /// The anvil binary the eval harness runs for a branch build, and the
    /// home it should seed from.
    ///
    /// With a container runtime this is a generated wrapper script that runs
    /// the worktree's release binary inside the same container shape, with
    /// the harness's fixture copy mounted as the work folder. Without one it
    /// is the binary itself.
    ///
    /// # Arguments
    ///
    /// * `worktree` - The ticket's worktree, whose `target/release/anvil` is built.
    ///
    /// # Returns
    ///
    /// `(anvil binary or wrapper, anvil home)`.
    ///
    /// # Errors
    ///
    /// When the wrapper cannot be written.
    pub fn eval_anvil(&self, worktree: &Path) -> Result<(PathBuf, PathBuf)> {
        let home = self.ticket_home(worktree)?;
        let binary = worktree
            .join("target")
            .join("release")
            .join(if cfg!(windows) { "anvil.exe" } else { "anvil" });
        if !self.containerized() {
            return Ok((binary, home));
        }
        let id = Self::ticket_id(worktree);
        let wrapper = self.config.home.eval_wrapper(&id);
        let mut args = self.run_args(
            Path::new("$PWD"),
            Path::new("$ANVIL_HOME"),
            &["anvil"],
            false,
        );
        // The harness mounts its fixture copy, not the worktree; the binary comes from the worktree.
        let image_index = args
            .iter()
            .position(|arg| *arg == self.config.image)
            .unwrap_or(args.len() - 1);
        args.insert(image_index, "-v".to_owned());
        args.insert(
            image_index + 1,
            format!("{}:/usr/local/bin/anvil:ro", binary.display()),
        );
        let quoted: Vec<String> = args.iter().map(|arg| shell_quote(arg)).collect();
        let keys: Vec<String> = self.key_names();
        let keep: String = keys
            .iter()
            .map(String::as_str)
            .chain(PASSTHROUGH_ENV.iter().copied())
            .map(|name| format!("{name}=\"${{{name}}}\""))
            .collect::<Vec<_>>()
            .join(" ");
        let script = format!(
            "#!/usr/bin/env bash\n# Generated by the factory for ticket {id}: runs the branch's anvil inside the sandbox.\n# Only the provider keys reach the container; the factory's own secrets stay out.\nset -euo pipefail\nexec env -i {keep} {runtime} {args} \"$@\"\n",
            runtime = runtime_name(self.config.runtime),
            args = quoted.join(" ")
        );
        std::fs::write(&wrapper, script)
            .map_err(|source| FactoryError::io("writing", &wrapper, source))?;
        make_executable(&wrapper)?;
        Ok((wrapper, home))
    }

    /// Runs a program in the worktree and reports pass or fail with its output tail.
    async fn run_check(
        &self,
        worktree: &Path,
        program: &[&str],
        deadline: Instant,
    ) -> Result<CheckOutcome> {
        let home = self.ticket_home(worktree)?;
        let command = self.command(worktree, &home, program, false);
        let exit = self.run_until(command, deadline).await?;
        Ok(CheckOutcome {
            passed: exit.code == Some(0),
            output_tail: tail(&exit.output),
        })
    }
}

impl Implementer for Sandbox {
    fn prepare<'a>(&'a self, worktree: &'a Path) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let home = self.ticket_home(worktree)?;
            if self.containerized() {
                self.start_proxy(worktree)?;
                self.prewarm(worktree, &home).await?;
            }
            Ok(())
        })
    }

    fn run<'a>(
        &'a self,
        worktree: &'a Path,
        task: &'a str,
        deadline: Instant,
    ) -> BoxFuture<'a, Result<ImplementerOutcome>> {
        Box::pin(async move {
            let home = self.ticket_home(worktree)?;
            usage::clear(worktree);
            let anvil = self.config.anvil_bin.clone();
            let program = [anvil.as_str(), "--yes", "--profile", PROFILE, task];
            let command = self.command(worktree, &home, &program, false);
            let exit = self.run_until(command, deadline).await?;
            if let Some(reason) = infrastructure_failure(&exit) {
                return Err(FactoryError::Sandbox(format!(
                    "{} could not start the implementer ({reason}): {}",
                    runtime_word(self.config.runtime),
                    last_line(&exit.output)
                )));
            }
            let used = usage::read(worktree, &self.config.providers);
            let status = if exit.timed_out {
                ImplementerStatus::Timeout
            } else {
                match exit.code {
                    Some(0) => ImplementerStatus::Done,
                    Some(2) => ImplementerStatus::Incomplete(incomplete_reason(&exit.output)),
                    code => ImplementerStatus::Failed { code },
                }
            };
            Ok(ImplementerOutcome {
                status,
                tokens: used.tokens,
                cost_usd: used.cost_usd,
                output_tail: tail(&exit.output),
            })
        })
    }

    fn check<'a>(
        &'a self,
        worktree: &'a Path,
        deadline: Instant,
    ) -> BoxFuture<'a, Result<CheckOutcome>> {
        Box::pin(async move {
            let just = self.config.just_bin.clone();
            self.run_check(worktree, &[just.as_str(), "check"], deadline)
                .await
        })
    }

    fn build_release<'a>(
        &'a self,
        worktree: &'a Path,
        deadline: Instant,
    ) -> BoxFuture<'a, Result<CheckOutcome>> {
        Box::pin(async move {
            let cargo = self.config.cargo_bin.clone();
            self.run_check(
                worktree,
                &[cargo.as_str(), "build", "--release", "-p", "anvil-cli"],
                deadline,
            )
            .await
        })
    }

    fn denied_hosts(&self) -> Vec<String> {
        lock(&self.denied).iter().cloned().collect()
    }
}

/// Makes sure the registry cache volume exists, creating it when it does not.
///
/// Every ticket and the start-up preflight call this, so it must succeed
/// when the volume is already there: podman's `volume create` refuses a
/// name that exists, so podman is asked `volume exists` first, and an
/// "already exists" refusal is treated as success for any runtime.
///
/// # Arguments
///
/// * `runner` - Runs the runtime's commands.
/// * `runtime` - podman, docker or none; none needs no volume.
///
/// # Errors
///
/// [`FactoryError::Spawn`] when the runtime cannot be started and
/// [`FactoryError::Sandbox`] when it refuses to create the volume.
pub async fn ensure_cache_volume(
    runner: &dyn CommandRunner,
    runtime: ContainerRuntime,
) -> Result<()> {
    if runtime == ContainerRuntime::None {
        return Ok(());
    }
    if runtime == ContainerRuntime::Podman {
        let exists = housekeeping(runner, runtime, &["volume", "exists", CACHE_VOLUME]).await?;
        if exists.code == Some(0) {
            return Ok(());
        }
    }
    let created = housekeeping(runner, runtime, &["volume", "create", CACHE_VOLUME]).await?;
    if created.code == Some(0) || created.output.contains("already exists") {
        return Ok(());
    }
    Err(FactoryError::Sandbox(format!(
        "{} volume create {CACHE_VOLUME}: {}",
        runtime_name(runtime),
        created.output.trim()
    )))
}

/// Checks that the runtime can run tickets: the worker image is built and
/// the registry cache volume exists or can be created.
///
/// The service runs this before taking a ticket and `factory status`
/// reports its result.
///
/// # Arguments
///
/// * `runner` - Runs the runtime's commands.
/// * `runtime` - podman, docker or none; none always passes.
/// * `image` - `[worker] image`.
///
/// # Errors
///
/// [`FactoryError::Spawn`] when the runtime cannot be started, and
/// [`FactoryError::Sandbox`] naming the `build` command to run when the
/// image is missing, or saying why the volume cannot be created.
pub async fn preflight(
    runner: &dyn CommandRunner,
    runtime: ContainerRuntime,
    image: &str,
) -> Result<()> {
    if runtime == ContainerRuntime::None {
        return Ok(());
    }
    let query: &[&str] = match runtime {
        ContainerRuntime::Podman => &["image", "exists", image],
        _ => &["image", "inspect", image],
    };
    let found = housekeeping(runner, runtime, query).await?;
    if found.code != Some(0) {
        return Err(FactoryError::Sandbox(format!(
            "worker image {image} is not built; run `{}` in the repository (or `just image`)",
            build_command(runtime, image)
        )));
    }
    ensure_cache_volume(runner, runtime).await
}

/// The command that builds the worker image, as `justfile` and `bootstrap.sh` run it.
///
/// # Arguments
///
/// * `runtime` - podman or docker.
/// * `image` - The tag to build.
pub fn build_command(runtime: ContainerRuntime, image: &str) -> String {
    format!(
        "{} build -t {image} -f deploy/nuc/Containerfile deploy/nuc",
        runtime_name(runtime)
    )
}

/// Runs a short runtime command (`volume exists`, `image exists`) with the
/// factory's own environment, which rootless podman needs.
async fn housekeeping(
    runner: &dyn CommandRunner,
    runtime: ContainerRuntime,
    args: &[&str],
) -> Result<Exit> {
    let invocation = Invocation {
        program: runtime_name(runtime).to_owned(),
        args: args.iter().map(|s| (*s).to_owned()).collect(),
        env: None,
        cwd: None,
    };
    runner
        .run(invocation, Instant::now() + HOUSEKEEPING_TIMEOUT)
        .await
}

/// How long a housekeeping command may take.
const HOUSEKEEPING_TIMEOUT: Duration = Duration::from_secs(60);

/// Why an implementer exit is the factory's failure rather than anvil's,
/// or `None` when anvil itself ran.
///
/// The runtime exits 125 when it cannot start the container, 126 and 127
/// when the program inside cannot run; and anvil prints `session <id>` as
/// its first line, so a failure without that line happened before anvil
/// started. A clean exit without the line is still anvil's (stand-ins in
/// tests print nothing), and a timeout has no output to judge.
fn infrastructure_failure(exit: &Exit) -> Option<String> {
    if exit.timed_out {
        return None;
    }
    match exit.code {
        Some(code @ 125..=127) => Some(format!("exit {code}")),
        Some(0) => None,
        _ if started(&exit.output) => None,
        Some(code) => Some(format!("exit {code} before anvil's session line")),
        None => Some("killed before anvil's session line".to_owned()),
    }
}

/// Whether anvil printed its `session <id>` line.
fn started(output: &str) -> bool {
    output.lines().any(|line| {
        line.trim()
            .strip_prefix("session ")
            .is_some_and(|id| !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric()))
    })
}

/// The last non-empty line of output, or `(no output)`.
fn last_line(output: &str) -> String {
    output
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("(no output)")
        .to_owned()
}

/// Spawns the invocation and waits for it, killing it at the deadline.
async fn run_process(invocation: Invocation, deadline: Instant) -> Result<Exit> {
    let mut command = Command::new(&invocation.program);
    command.args(&invocation.args);
    if let Some(env) = &invocation.env {
        command.env_clear();
        command.envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    }
    if let Some(cwd) = &invocation.cwd {
        command.current_dir(cwd);
    }
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    command.kill_on_drop(true);
    let mut child = command.spawn().map_err(|source| FactoryError::Spawn {
        program: invocation.program.clone(),
        source,
    })?;
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let reader = async {
        let mut out = Vec::new();
        let mut err = Vec::new();
        if let Some(stdout) = stdout.as_mut() {
            let _ = stdout.take(OUTPUT_CAP as u64).read_to_end(&mut out).await;
        }
        if let Some(stderr) = stderr.as_mut() {
            let _ = stderr.take(OUTPUT_CAP as u64).read_to_end(&mut err).await;
        }
        (out, err)
    };
    let remaining = deadline.saturating_duration_since(Instant::now());
    let waited = tokio::time::timeout(remaining, async {
        let (output, status) = tokio::join!(reader, child.wait());
        (output, status)
    })
    .await;
    match waited {
        Ok(((out, err), status)) => {
            let status = status.map_err(|source| FactoryError::Spawn {
                program: invocation.program.clone(),
                source,
            })?;
            Ok(Exit {
                code: status.code(),
                timed_out: false,
                output: combine(&out, &err),
            })
        }
        Err(_) => {
            let _ = child.kill().await;
            Ok(Exit {
                code: None,
                timed_out: true,
                output: "(killed: deadline passed)".to_owned(),
            })
        }
    }
}

/// The runtime's executable name.
fn runtime_name(runtime: ContainerRuntime) -> &'static str {
    match runtime {
        ContainerRuntime::Podman => "podman",
        ContainerRuntime::Docker => "docker",
        ContainerRuntime::None => "",
    }
}

/// The runtime as it reads in a message: its name, or `the host`.
fn runtime_word(runtime: ContainerRuntime) -> &'static str {
    match runtime {
        ContainerRuntime::None => "the host",
        other => runtime_name(other),
    }
}

/// `uid:gid` of this process for `docker --user`; `1000:1000` where unknown.
#[cfg(unix)]
fn current_uid_gid() -> String {
    let out = std::process::Command::new("id").arg("-u").output().ok();
    let uid = out
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_else(|| "1000".to_owned());
    format!("{uid}:{uid}")
}

/// `uid:gid` for `docker --user`; there is no uid on Windows, so the usual first user.
#[cfg(not(unix))]
fn current_uid_gid() -> String {
    "1000:1000".to_owned()
}

/// The host of a base URL, without scheme, port or path.
fn host_of(url: &str) -> Option<String> {
    let rest = url.split("://").nth(1)?;
    let host = rest.split('/').next()?;
    let host = host.rsplit('@').next()?;
    let host = host.split(':').next()?;
    if host.is_empty() {
        None
    } else {
        Some(host.to_ascii_lowercase())
    }
}

/// stdout then stderr, as text.
fn combine(out: &[u8], err: &[u8]) -> String {
    let mut text = String::from_utf8_lossy(out).into_owned();
    if !err.is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&String::from_utf8_lossy(err));
    }
    text
}

/// The last lines of output.
fn tail(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(TAIL_LINES);
    lines[start..].join("\n")
}

/// The reason anvil printed on its last `incomplete:` line, or `unknown`.
fn incomplete_reason(output: &str) -> String {
    output
        .lines()
        .rev()
        .find_map(|line| line.trim().strip_prefix("incomplete:"))
        .map(|reason| reason.trim().to_owned())
        .unwrap_or_else(|| "unknown".to_owned())
}

/// Quotes one argument for a bash script; `$PWD` and `$ANVIL_HOME` stay unquoted.
fn shell_quote(arg: &str) -> String {
    if arg.contains("$PWD") || arg.contains("$ANVIL_HOME") {
        return format!("\"{arg}\"");
    }
    if arg
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_./:=,@".contains(c))
    {
        arg.to_owned()
    } else {
        format!("'{}'", arg.replace('\'', "'\\''"))
    }
}

/// The bash that runs `.sh` stand-ins on the host; Git for Windows first,
/// because the `bash.exe` in `System32` is the WSL launcher.
fn bash_program() -> PathBuf {
    if cfg!(windows) {
        for base in ["ProgramFiles", "ProgramW6432", "LOCALAPPDATA"] {
            let Some(root) = std::env::var_os(base) else {
                continue;
            };
            let root = PathBuf::from(root);
            let candidates = [
                root.join("Git").join("bin").join("bash.exe"),
                root.join("Git").join("usr").join("bin").join("bash.exe"),
                root.join("Programs")
                    .join("Git")
                    .join("bin")
                    .join("bash.exe"),
            ];
            if let Some(found) = candidates.into_iter().find(|p| p.is_file()) {
                return found;
            }
        }
    }
    PathBuf::from("bash")
}

/// Marks a script executable where that matters.
fn make_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .map_err(|source| FactoryError::io("chmod", path, source))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

/// Locks a mutex, recovering from a poisoned one.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anvil_types::config::SETUP_PROVIDERS;

    fn config(runtime: ContainerRuntime) -> SandboxConfig {
        let anvil: anvil_types::AnvilConfig = toml::from_str(SETUP_PROVIDERS).expect("parses");
        SandboxConfig {
            runtime,
            image: "localhost/anvil-factory:latest".to_owned(),
            anvil_bin: "anvil".to_owned(),
            just_bin: "just".to_owned(),
            cargo_bin: "cargo".to_owned(),
            egress_allow: vec!["github.com".to_owned(), "index.crates.io".to_owned()],
            providers: anvil.providers,
            anvil_config: SETUP_PROVIDERS.to_owned(),
            profile: "[roles]\ncoder = \"anthropic\"\n".to_owned(),
            home: FactoryHome::under(Path::new("/h")),
        }
    }

    #[test]
    fn run_args_isolate_the_network_mount_the_tree_and_name_keys_only() {
        let sandbox = Sandbox::new(config(ContainerRuntime::Podman));
        let args = sandbox.run_args(
            Path::new("/h/.factory/work/T1"),
            Path::new("/h/.factory/work/T1-home"),
            &["anvil", "--yes", "--profile", "factory", "do it"],
            false,
        );
        let text = args.join(" ");
        assert!(
            text.starts_with("run --rm --network none --init --userns=keep-id "),
            "{text}"
        );
        assert!(
            text.contains("-v /h/.factory/work/T1:/work -w /work"),
            "{text}"
        );
        assert!(text.contains("-v /h/.factory/work/T1-home:/anvil-home -e ANVIL_HOME=/anvil-home"));
        assert!(
            text.contains(&format!("-v {CACHE_VOLUME}:{CACHE_MOUNT}:ro")),
            "{text}"
        );
        let socket = sandbox.config.home.proxy_socket("T1");
        assert!(
            text.contains(&format!("-v {}:/run/factory/proxy.sock", socket.display())),
            "{text}"
        );
        assert!(text.contains("-e HTTPS_PROXY=http://127.0.0.1:3128"));
        assert!(text.contains("-e https_proxy=http://127.0.0.1:3128"));
        assert!(text.contains("-e CARGO_HTTP_PROXY=http://127.0.0.1:3128"));
        assert!(
            text.ends_with("localhost/anvil-factory:latest anvil --yes --profile factory do it")
        );
        for forbidden in ["FACTORY_GITHUB_TOKEN", "GITHUB_TOKEN", "WEBHOOK_URL"] {
            assert!(!text.contains(forbidden), "{forbidden} in {text}");
        }
        for (name, value) in sandbox.environment(Path::new("/h/.factory/work/T1-home")) {
            assert!(
                !["FACTORY_GITHUB_TOKEN", "GITHUB_TOKEN", "WEBHOOK_URL"].contains(&name.as_str()),
                "{name}={value}"
            );
        }
        let writable = sandbox.run_args(
            Path::new("/w"),
            Path::new("/w-home"),
            &["cargo", "fetch"],
            true,
        );
        assert!(
            writable
                .join(" ")
                .contains(&format!("{CACHE_VOLUME}:{CACHE_MOUNT} "))
        );
        let docker = Sandbox::new(config(ContainerRuntime::Docker));
        assert!(
            docker
                .run_args(Path::new("/w"), Path::new("/w-home"), &["x"], false)
                .join(" ")
                .contains("--user ")
        );
    }

    #[test]
    fn allowlist_holds_the_configured_hosts_and_every_provider_host() {
        let sandbox = Sandbox::new(config(ContainerRuntime::Podman));
        let list = sandbox.allowlist();
        assert!(list.allows("github.com", 443));
        assert!(list.allows("index.crates.io", 443));
        assert!(list.allows("api.anthropic.com", 443));
        assert!(list.allows("api.openai.com", 443));
        assert!(list.allows("generativelanguage.googleapis.com", 443));
        assert!(!list.allows("evil.example", 443));
        assert_eq!(
            host_of("https://api.anthropic.com/v1"),
            Some("api.anthropic.com".to_owned())
        );
        assert_eq!(host_of("http://user@HOST:8080/x"), Some("host".to_owned()));
        assert_eq!(host_of("nonsense"), None);
    }

    #[test]
    fn the_ticket_home_holds_the_configuration_and_the_profile_only() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut config = config(ContainerRuntime::None);
        config.home = FactoryHome::under(temp.path());
        let sandbox = Sandbox::new(config);
        let worktree = temp.path().join(".factory/work/T9");
        let home = sandbox.ticket_home(&worktree).expect("home");
        assert_eq!(home, temp.path().join(".factory/work/T9-home"));
        assert!(home.join(".anvil/config.toml").is_file());
        assert_eq!(
            std::fs::read_to_string(home.join(".anvil/profiles/factory.toml")).expect("read"),
            "[roles]\ncoder = \"anthropic\"\n"
        );
        assert!(!home.join(".anvil/.env").exists());
        let (binary, eval_home) = sandbox.eval_anvil(&worktree).expect("eval");
        assert_eq!(eval_home, home);
        assert!(binary.starts_with(worktree.join("target").join("release")));
    }

    #[test]
    fn the_eval_wrapper_mounts_the_fixture_and_the_branch_binary() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut config = config(ContainerRuntime::Podman);
        config.home = FactoryHome::under(temp.path());
        let sandbox = Sandbox::new(config);
        let worktree = temp.path().join(".factory/work/T9");
        let (wrapper, _) = sandbox.eval_anvil(&worktree).expect("wrapper");
        let script = std::fs::read_to_string(&wrapper).expect("read");
        assert!(script.starts_with("#!/usr/bin/env bash"), "{script}");
        assert!(script.contains("exec env -i "), "{script}");
        assert!(script.contains("-v \"$PWD:/work\""), "{script}");
        assert!(
            script.contains("-v \"$ANVIL_HOME:/anvil-home\""),
            "{script}"
        );
        assert!(script.contains("/usr/local/bin/anvil:ro"), "{script}");
        assert!(script.contains("--network none"), "{script}");
        assert!(script.trim_end().ends_with("anvil \"$@\""), "{script}");
        assert!(!script.contains("FACTORY_GITHUB_TOKEN"));
    }

    #[test]
    fn output_helpers() {
        assert_eq!(
            incomplete_reason("x\nincomplete: token budget\n"),
            "token budget"
        );
        assert_eq!(incomplete_reason("nothing"), "unknown");
        assert_eq!(combine(b"a", b"b"), "a\nb");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("plain-arg:1"), "plain-arg:1");
        assert_eq!(shell_quote("$PWD:/work"), "\"$PWD:/work\"");
        let long: String = (0..100).map(|i| format!("{i}\n")).collect();
        assert!(!tail(&long).contains("39\n"));
        assert!(tail(&long).starts_with("40\n"));
        assert_eq!(last_line("a\nError: gone\n\n"), "Error: gone");
        assert_eq!(last_line(""), "(no output)");
    }

    #[test]
    fn the_session_line_separates_anvil_failures_from_runtime_failures() {
        assert!(started("session 01ARZ3NDEKTSV4RRFFQ69G5FAV\nworking\n"));
        assert!(started("noise\n  session abc\n"));
        assert!(!started("session \n"));
        assert!(!started(
            "Trying to pull localhost/anvil-factory:latest...\n"
        ));

        let exit = |code: Option<i32>, output: &str| Exit {
            code,
            timed_out: false,
            output: output.to_owned(),
        };
        assert_eq!(
            infrastructure_failure(&exit(Some(125), "session x\n")),
            Some("exit 125".to_owned())
        );
        assert_eq!(
            infrastructure_failure(&exit(Some(1), "Error: image not known\n")),
            Some("exit 1 before anvil's session line".to_owned())
        );
        assert_eq!(
            infrastructure_failure(&exit(Some(1), "session x\nboom\n")),
            None
        );
        assert_eq!(infrastructure_failure(&exit(Some(0), "")), None);
        assert_eq!(
            infrastructure_failure(&exit(None, "")),
            Some("killed before anvil's session line".to_owned())
        );
        let timed_out = Exit {
            code: None,
            timed_out: true,
            output: "(killed: deadline passed)".to_owned(),
        };
        assert_eq!(infrastructure_failure(&timed_out), None);
        assert_eq!(
            build_command(ContainerRuntime::Podman, "localhost/anvil-factory:latest"),
            "podman build -t localhost/anvil-factory:latest -f deploy/nuc/Containerfile deploy/nuc"
        );
    }
}
