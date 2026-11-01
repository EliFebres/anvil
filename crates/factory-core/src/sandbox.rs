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

/// Runs the implementer and the build tools for one factory.
pub struct Sandbox {
    config: SandboxConfig,
    keys: KeyLookup,
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
        Self {
            config,
            keys,
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

    /// The command for a program in the worktree, on the host or in the container.
    fn command(
        &self,
        worktree: &Path,
        home: &Path,
        program: &[&str],
        cache_writable: bool,
    ) -> Command {
        let mut command = match self.config.runtime {
            ContainerRuntime::None => {
                let (bin, rest) = program.split_first().unwrap_or((&"true", &[]));
                let mut command = if bin.ends_with(".sh") {
                    let mut c = Command::new(bash_program());
                    c.arg(bin);
                    c
                } else {
                    Command::new(bin)
                };
                command.args(rest);
                command.current_dir(worktree);
                command
            }
            runtime => {
                let mut command = Command::new(runtime_name(runtime));
                command.args(self.run_args(worktree, home, program, cache_writable));
                command
            }
        };
        command.env_clear();
        command.envs(self.environment(home));
        command.stdin(Stdio::null());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        command.kill_on_drop(true);
        command
    }

    /// Runs a command until it exits or the deadline passes.
    async fn run_until(&self, mut command: Command, deadline: Instant) -> Result<Exit> {
        let program = format!("{:?}", command.as_std().get_program());
        let mut child = command
            .spawn()
            .map_err(|source| FactoryError::Spawn { program, source })?;
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
                    program: "sandbox command".to_owned(),
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
        let runtime = runtime_name(self.config.runtime);
        let created = Command::new(runtime)
            .args(["volume", "create", CACHE_VOLUME])
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(|source| FactoryError::Spawn {
                program: runtime.to_owned(),
                source,
            })?;
        if !created.status.success() {
            return Err(FactoryError::Sandbox(format!(
                "{runtime} volume create {CACHE_VOLUME}: {}",
                String::from_utf8_lossy(&created.stderr).trim()
            )));
        }
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

/// How a sandbox command ended.
struct Exit {
    code: Option<i32>,
    timed_out: bool,
    output: String,
}

/// The runtime's executable name.
fn runtime_name(runtime: ContainerRuntime) -> &'static str {
    match runtime {
        ContainerRuntime::Podman => "podman",
        ContainerRuntime::Docker => "docker",
        ContainerRuntime::None => "",
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
    }
}
