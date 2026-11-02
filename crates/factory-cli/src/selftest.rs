//! `factory selftest`: the whole worker pipeline, for real, on this machine.
//!
//! A bare repository in a temp folder stands in for GitHub and a scripted
//! `protocol = "fake"` provider stands in for the models, so the run needs
//! no network and no key. Everything else is production: the worker image,
//! the resolved anvil binary mounted into it, the egress proxy (with an
//! empty allowlist), the ticket queue on `factory/state`, the worktree,
//! Gate A, Gate C with an approving reviewer, the push, the state moves and
//! a squash merge into the bare repository's `main`. Gate B is skipped: the
//! temp repository has no eval baseline and the eval would call models.
//!
//! Each step prints `PASS` or `FAIL`. The first failure stops the run; the
//! cleanup (containers, the volume, the temp folder) runs on every path.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anvil_types::factory_config::Reviewer as ReviewerEntry;
use anvil_types::ticket::dirs::{CLAIMED_DIR, MERGED_DIR, OPEN_DIR, REJECTED_DIR, STATE_BRANCH};
use anvil_types::{
    AnvilConfig, ContainerRuntime, FactoryConfig, Priority, Stance, Ticket, TicketKind,
    TicketOrigin, TicketStatus,
};
use anyhow::{Context, anyhow, bail};
use chrono::Utc;
use factory_core::fakes::LocalHost;
use factory_core::github::{FALLBACK_TOKEN_ENV, TOKEN_ENV};
use factory_core::ports::{BoxFuture, EvalGate, EvalGateReport, GateB, Implementer};
use factory_core::sandbox::KeyLookup;
use factory_core::{
    FactoryHome, Git, LogNotifier, OutcomeKind, Sandbox, SandboxConfig, SandboxOptions, Service,
    ServiceDeps, ServiceState, SystemRunner, Tick, TicketQueue, TicketWorker, WorkerDeps,
    check_image, hostname, resolve_anvil_bin, system_clock,
};
use factory_review::{FakeReviewer, Reviewer};
use tokio::process::Command;
use ulid::Ulid;

use crate::adapters::review_swarm::Swarm;
use crate::wiring::{PROFILE_PATH, REMOTE};

/// The coder's script, relative to the repository root (and so to `/work`).
pub const SCRIPT_PATH: &str = "crates/factory-core/selftest/readme.json";

/// The summarizer's script. Memory extraction builds its own provider after
/// the loop, which would replay a shared script from the top, so the
/// summarizer gets its own: one turn that proposes no facts.
pub const SUMMARIZER_SCRIPT_PATH: &str = "crates/factory-core/selftest/summarizer.json";

/// The line the scripted coder appends to README.md.
pub const MARKER: &str = "selftest";

/// The provider the coder and both reviewers use.
const FAKE_PROVIDER: &str = "fake";

/// The provider the summarizer uses.
const FAKE_SUMMARIZER: &str = "fake-summarizer";

/// The key variable of the fake provider. It never reaches a model; it is a
/// canary that travels the same path a real key does, so the secret scan has
/// something to find if any of the factory's output leaks one.
const CANARY_ENV: &str = "ANVIL_SELFTEST_KEY";

/// The label every selftest container carries, `<LABEL>=<run id>`.
const LABEL: &str = "anvil.selftest";

/// Gives up on the whole run after this long.
const RUN_CAP: Duration = Duration::from_secs(5 * 60);

/// Values shorter than this are not scanned for; the worker's scan uses the same floor.
const MIN_SECRET_LEN: usize = 8;

/// What the selftest needs.
#[derive(Debug, Clone)]
pub struct SelftestOptions {
    /// The repository under test; its working tree is what gets tested.
    pub repo_root: PathBuf,
    /// The home directory, for reading which key variables the real anvil
    /// configuration names; `None` scans for the GitHub tokens only.
    pub home: Option<PathBuf>,
    /// Gate A, as a command line; `true` by default for speed.
    pub check: String,
    /// Keep the temp folder for inspection instead of removing it.
    pub keep: bool,
    /// Run anvil and Gate A on the host instead of in the container, for the
    /// test suite on machines without podman.
    pub no_container: bool,
}

/// Runs the selftest, printing one line per step.
///
/// # Arguments
///
/// * `options` - The repository, Gate A's command and the switches.
///
/// # Returns
///
/// `Ok(())` when every step passed.
///
/// # Errors
///
/// `selftest failed at step <n> (<name>)` when a step failed; the step's
/// own output was printed above it.
pub async fn selftest(options: &SelftestOptions) -> anyhow::Result<()> {
    let started = Instant::now();
    let mut steps = Steps::default();
    let world = steps.report("temp folder", World::create(options))?;
    // The log lands in the temp home, so the secret scan covers it too.
    let log_guard = crate::logging::init(&world.home().logs_dir());
    let run = tokio::time::timeout(RUN_CAP, pipeline(&world, options, &mut steps)).await;
    drop(log_guard);
    let mut result = match run {
        Ok(result) => result,
        Err(_) => steps.report::<()>(
            "time cap",
            Err(anyhow!(
                "the run took longer than {}s and was stopped",
                RUN_CAP.as_secs()
            )),
        ),
    };
    if result.is_ok() {
        result = steps.report(
            "verify: no secret value in the temp folder",
            world.verify_secrets(options).await,
        );
    }
    let cleaned = world.cleanup(options.keep).await;
    let cleanup = steps.report("cleanup: containers, volume, temp folder", cleaned);
    let total = started.elapsed().as_secs_f64();
    result.and(cleanup)?;
    println!("selftest passed in {total:.1}s");
    Ok(())
}

/// The steps between the temp folder and the cleanup; stops at the first failure.
async fn pipeline(
    world: &World,
    options: &SelftestOptions,
    steps: &mut Steps,
) -> anyhow::Result<()> {
    let factory = crate::wiring::read_factory(&options.repo_root)?;
    let anvil_bin = steps.report(
        "preflight: image, anvil binary, script",
        preflight(world, options, &factory).await,
    )?;
    steps.report(
        "bare repo: push the working tree as main and factory/state",
        world.seed(&options.repo_root).await,
    )?;
    let ticket = steps.report(
        "ticket: tickets/open on factory/state",
        world.write_ticket().await,
    )?;
    let parts = steps.report(
        "config: fake provider, factory profile, empty allowlist",
        world.configure(&factory, &anvil_bin, options).await,
    )?;
    steps.report(
        "pipeline: claim, sandbox, gate A, gate B (skipped), gate C, push, merge",
        world.work(&parts, ticket.id).await,
    )?;
    steps.report(
        "verify: main has the README line",
        world.verify_main().await,
    )?;
    steps.report(
        "verify: factory/state has the ticket under tickets/merged",
        world.verify_state(ticket.id).await,
    )?;
    steps.report(
        "verify: worktree, branch and denied hosts",
        world.verify_worktree(ticket.id, &parts.sandbox).await,
    )?;
    Ok(())
}

/// Numbers the steps and prints their outcome.
#[derive(Debug, Default)]
struct Steps {
    count: usize,
    last: Option<Instant>,
}

impl Steps {
    /// Prints `PASS` or `FAIL` for one finished step.
    fn report<T>(&mut self, name: &str, result: anyhow::Result<T>) -> anyhow::Result<T> {
        self.count += 1;
        let since = self.last.replace(Instant::now());
        let secs = since.map_or(0.0, |at| at.elapsed().as_secs_f64());
        let line = step_line(self.count, name, secs, result.as_ref().err());
        println!("{line}");
        result.map_err(|_| anyhow!("selftest failed at step {} ({name})", self.count))
    }
}

/// One step's line: `PASS  3 name (1.2s)`, or `FAIL  3 name` and the error chain.
pub fn step_line(number: usize, name: &str, secs: f64, error: Option<&anyhow::Error>) -> String {
    match error {
        None => format!("PASS {number:>2} {name} ({secs:.1}s)"),
        Some(error) => {
            let detail: String = format!("{error:#}")
                .lines()
                .map(|line| format!("     | {line}\n"))
                .collect();
            format!("FAIL {number:>2} {name}\n{}", detail.trim_end())
        }
    }
}

/// The temp folder and the names that point into it.
struct World {
    dir: Option<tempfile::TempDir>,
    root: PathBuf,
    run_id: String,
    runtime: ContainerRuntime,
    canary: String,
}

/// The objects the pipeline step needs.
struct Parts {
    service: Service,
    sandbox: Arc<Sandbox>,
}

impl World {
    /// Creates the temp folder. Its path stays short: the proxy socket lives
    /// under it, and a unix socket path has a limit of about 100 bytes.
    fn create(options: &SelftestOptions) -> anyhow::Result<Self> {
        let dir = tempfile::Builder::new()
            .prefix("fst-")
            .tempdir_in(std::env::temp_dir())
            .context("creating the temp folder")?;
        let root = dir.path().to_path_buf();
        let run_id = Ulid::generate().to_string().to_lowercase();
        Ok(Self {
            dir: Some(dir),
            root,
            runtime: if options.no_container {
                ContainerRuntime::None
            } else {
                ContainerRuntime::Podman
            },
            canary: format!("selftest-canary-{}", Ulid::generate()),
            run_id,
        })
    }

    fn bare(&self) -> PathBuf {
        self.root.join("remote.git")
    }

    fn clone_dir(&self) -> PathBuf {
        self.root.join("repo")
    }

    fn home(&self) -> FactoryHome {
        FactoryHome::under(&self.root.join("home"))
    }

    fn volume(&self) -> String {
        format!("anvil-selftest-{}", self.run_id)
    }

    fn label(&self) -> String {
        format!("{LABEL}={}", self.run_id)
    }

    /// Pushes a snapshot of the working tree to a new bare repository as
    /// `main` and `factory/state`, and clones it as the factory's repository.
    async fn seed(&self, repo_root: &Path) -> anyhow::Result<()> {
        let commit = snapshot(repo_root, &self.root.join("snapshot.index")).await?;
        let bare = self.bare();
        let bare_text = bare.to_string_lossy();
        Git::new(&self.root)
            .run(&["init", "-q", "--bare", "-b", "main", &bare_text])
            .await?;
        Git::new(repo_root)
            .run(&[
                "push",
                "-q",
                &bare_text,
                &format!("{commit}:refs/heads/main"),
                &format!("{commit}:refs/heads/{STATE_BRANCH}"),
            ])
            .await?;
        Git::new(&self.root)
            .run(&[
                "clone",
                "-q",
                &bare_text,
                &self.clone_dir().to_string_lossy(),
            ])
            .await?;
        Ok(())
    }

    /// Commits the synthetic ticket to `tickets/open/` on `factory/state`.
    async fn write_ticket(&self) -> anyhow::Result<Ticket> {
        let ticket = Ticket {
            id: Ulid::generate(),
            created_at: Utc::now(),
            kind: TicketKind::Bug,
            title: "selftest: append a line to README.md".to_owned(),
            symptom: "The factory selftest needs a change to carry through the pipeline."
                .to_owned(),
            repro: None,
            spec: format!("Add a line `{MARKER}` to the end of README.md."),
            acceptance: vec![format!("README.md ends with the line `{MARKER}`")],
            evidence: Vec::new(),
            priority: Priority::P1,
            status: TicketStatus::Open,
            origin: TicketOrigin::Human,
            claimed_at: None,
            claimed_by: None,
            pr: None,
            note: None,
        };
        ticket
            .validate()
            .context("the synthetic ticket is invalid")?;
        let seed = self.root.join("seed");
        Git::new(&self.root)
            .run(&[
                "clone",
                "-q",
                "--branch",
                STATE_BRANCH,
                &self.bare().to_string_lossy(),
                &seed.to_string_lossy(),
            ])
            .await?;
        let relative = format!("{OPEN_DIR}/{}.toml", ticket.id);
        let path = seed.join(&relative);
        let parent = path.parent().context("the ticket path has a parent")?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
        std::fs::write(&path, toml::to_string(&ticket)?)
            .with_context(|| format!("writing {}", path.display()))?;
        let git = Git::new(&seed);
        git.commit_paths(&[relative], "selftest: open a ticket")
            .await?;
        git.push("origin", STATE_BRANCH).await?;
        std::fs::remove_dir_all(&seed).with_context(|| format!("removing {}", seed.display()))?;
        Ok(ticket)
    }

    /// Builds the service: production parts except the host, the eval gate,
    /// the reviewers and the providers.
    async fn configure(
        &self,
        real: &FactoryConfig,
        anvil_bin: &Path,
        options: &SelftestOptions,
    ) -> anyhow::Result<Parts> {
        let factory = factory_config(real, self.runtime);
        let anvil = anvil_config()?;
        let main = format!("{REMOTE}/main");
        let repo = Git::new(&self.clone_dir());
        let profile = selftest_profile(&repo.show(&main, PROFILE_PATH).await?)?;
        let home = self.home();
        let check: Vec<String> = options
            .check
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        if check.is_empty() {
            bail!("--check is empty");
        }
        let canary = self.canary.clone();
        let keys: KeyLookup =
            Arc::new(move |name: &str| (name == CANARY_ENV).then(|| canary.clone()));
        let sandbox = Arc::new(Sandbox::with_key_lookup(
            SandboxConfig {
                runtime: self.runtime,
                image: factory.worker.image.clone(),
                anvil_bin: anvil_bin.to_path_buf(),
                options: SandboxOptions {
                    check_command: check,
                    cache_volume: self.volume(),
                    prewarm: false,
                    labels: vec![self.label()],
                },
                cargo_bin: "cargo".to_owned(),
                egress_allow: factory.worker.egress_allow.clone(),
                providers: anvil.providers.clone(),
                anvil_config: toml::to_string_pretty(&anvil)?,
                profile,
                home: home.clone(),
            },
            keys,
        ));
        if !sandbox.allowlist().entries().is_empty() {
            bail!(
                "the egress allowlist should be empty, but it is {:?}",
                sandbox.allowlist().entries()
            );
        }
        let queue = TicketQueue::open(&repo, &home.state_dir(), REMOTE).await?;
        let host = Arc::new(LocalHost::new(&self.bare(), &self.root.join("merge")));
        let reviewers: Vec<Box<dyn Reviewer>> = vec![Box::new(FakeReviewer::approving(
            FAKE_PROVIDER,
            Stance::Correctness,
        ))];
        let worker = TicketWorker::new(WorkerDeps {
            repo: repo.clone(),
            remote: REMOTE.to_owned(),
            home: home.clone(),
            config: factory.clone(),
            implementer: Arc::clone(&sandbox) as Arc<_>,
            eval_gate: Arc::new(SkippedEvalGate),
            review: Arc::new(Swarm::new(reviewers)),
            host: Arc::clone(&host) as Arc<_>,
            secrets: secret_values(options.home.as_deref(), &options.repo_root, &self.canary),
            clock: Arc::new(Instant::now),
        })?;
        let service = Service::new(ServiceDeps {
            home,
            config: factory,
            queue,
            worker: Arc::new(worker),
            host,
            notifier: Arc::new(LogNotifier),
            hostname: hostname(),
            clock: system_clock(),
        })?;
        Ok(Parts { service, sandbox })
    }

    /// One pass of the service; it must merge the selftest ticket.
    async fn work(&self, parts: &Parts, id: Ulid) -> anyhow::Result<()> {
        let tick = parts.service.run_once().await;
        parts.sandbox.stop_proxy();
        match tick? {
            Tick::Worked(worked) if worked == id => {}
            other => bail!("the service did not work the selftest ticket: {other:?}"),
        }
        let state = ServiceState::load(&self.home().state_file())?;
        let outcome = state
            .outcomes
            .front()
            .context("the service recorded no outcome")?;
        if outcome.kind == OutcomeKind::Merged {
            return Ok(());
        }
        let notes = Git::new(&self.bare())
            .show(STATE_BRANCH, &format!("{REJECTED_DIR}/{id}.notes.md"))
            .await
            .unwrap_or_else(|_| "(no notes on factory/state)".to_owned());
        bail!(
            "the ticket ended {:?}, not merged: {}\n\n{notes}",
            outcome.kind,
            outcome.detail
        )
    }

    async fn verify_main(&self) -> anyhow::Result<()> {
        let readme = Git::new(&self.bare()).show("main", "README.md").await?;
        let last = readme.lines().last().unwrap_or("");
        if last != MARKER {
            bail!("README.md on main ends with {last:?}, not {MARKER:?}");
        }
        Ok(())
    }

    async fn verify_state(&self, id: Ulid) -> anyhow::Result<()> {
        let bare = Git::new(&self.bare());
        let file = format!("{id}.toml");
        let merged = bare.ls_tree(STATE_BRANCH, MERGED_DIR).await?;
        if !merged.iter().any(|path| path.ends_with(&file)) {
            bail!("{MERGED_DIR}/{file} is not on {STATE_BRANCH}; it has {merged:?}");
        }
        for dir in [OPEN_DIR, CLAIMED_DIR] {
            let listed = bare.ls_tree(STATE_BRANCH, dir).await?;
            if listed.iter().any(|path| path.ends_with(&file)) {
                bail!("{dir}/{file} is still on {STATE_BRANCH}");
            }
        }
        let text = bare
            .show(STATE_BRANCH, &format!("{MERGED_DIR}/{file}"))
            .await?;
        let ticket: Ticket = toml::from_str(&text).context("parsing the merged ticket")?;
        ticket.validate().context("validating the merged ticket")?;
        if ticket.status != TicketStatus::Merged {
            bail!("the merged ticket has status {:?}", ticket.status);
        }
        Ok(())
    }

    async fn verify_worktree(&self, id: Ulid, sandbox: &Sandbox) -> anyhow::Result<()> {
        let home = self.home();
        let work = home.work_dir(&id.to_string());
        if work.exists() {
            bail!("the worktree {} is still there", work.display());
        }
        let listing = Git::new(&self.clone_dir())
            .run(&["worktree", "list", "--porcelain"])
            .await?;
        let worktrees: Vec<&str> = listing
            .lines()
            .filter_map(|line| line.strip_prefix("worktree "))
            .collect();
        if worktrees.len() != 2 {
            bail!("expected the clone and the state worktree, found {worktrees:?}");
        }
        let branch = format!("refs/heads/factory/{id}");
        if Git::new(&self.bare())
            .try_rev_parse(&branch)
            .await
            .is_some()
        {
            bail!("{branch} still exists on the remote");
        }
        let denied = sandbox.denied_hosts();
        if !denied.is_empty() {
            bail!("the sandbox tried to reach {denied:?}");
        }
        Ok(())
    }

    async fn verify_secrets(&self, options: &SelftestOptions) -> anyhow::Result<()> {
        let secrets = secret_values(options.home.as_deref(), &options.repo_root, &self.canary);
        let mut hits = Vec::new();
        scan_tree(&self.root, &secrets, &mut hits)?;
        let history = Git::new(&self.bare())
            .run(&["log", "-p", "--all", "--format=%H %s%n%b"])
            .await?;
        if contains_any(history.as_bytes(), &secrets) {
            hits.push(self.bare().join("(history)"));
        }
        if !hits.is_empty() {
            let list: Vec<String> = hits.iter().map(|path| path.display().to_string()).collect();
            bail!("a secret value appears in:\n{}", list.join("\n"));
        }
        Ok(())
    }

    /// Removes the selftest's containers, its volume and the temp folder.
    async fn cleanup(mut self, keep: bool) -> anyhow::Result<()> {
        let mut problems = Vec::new();
        if self.runtime != ContainerRuntime::None {
            let filter = format!("label={}", self.label());
            match podman(&["ps", "-aq", "--filter", &filter]).await {
                Ok(ids) => {
                    for id in ids.split_whitespace() {
                        if let Err(error) = podman(&["rm", "-f", id]).await {
                            problems.push(format!("removing container {id}: {error:#}"));
                        }
                    }
                }
                Err(error) => problems.push(format!("listing containers: {error:#}")),
            }
            let volume = self.volume();
            if podman(&["volume", "exists", &volume]).await.is_ok()
                && let Err(error) = podman(&["volume", "rm", "-f", &volume]).await
            {
                problems.push(format!("removing volume {volume}: {error:#}"));
            }
        }
        if let Some(dir) = self.dir.take() {
            if keep {
                let kept = dir.keep();
                println!("kept {}", kept.display());
            } else if let Err(error) = dir.close() {
                problems.push(format!("removing {}: {error}", self.root.display()));
            }
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(anyhow!(problems.join("\n")))
        }
    }
}

/// Checks the image, resolves the anvil binary and checks it is static, and
/// checks that the script's edit still matches README.md.
async fn preflight(
    world: &World,
    options: &SelftestOptions,
    factory: &FactoryConfig,
) -> anyhow::Result<PathBuf> {
    let root = &options.repo_root;
    let anvil = resolve_anvil_bin(&factory.worker.anvil_bin, root)?;
    if world.runtime != ContainerRuntime::None {
        check_image(&SystemRunner, world.runtime, &factory.worker.image).await?;
        crate::update::verify_static_binary(&anvil)?;
    }
    let script = std::fs::read_to_string(root.join(SCRIPT_PATH))
        .with_context(|| format!("reading {SCRIPT_PATH}"))?;
    let readme = std::fs::read_to_string(root.join("README.md")).context("reading README.md")?;
    check_anchor(&script, &readme)?;
    Ok(anvil)
}

/// Checks that the script's `edit_file` call replaces README.md's last line.
///
/// # Arguments
///
/// * `script` - The provider script's JSON.
/// * `readme` - README.md as it is in the working tree.
///
/// # Errors
///
/// When the script has no `edit_file` call on README.md, or its `old` text
/// is not README.md's last line (README changed after the script was written).
pub fn check_anchor(script: &str, readme: &str) -> anyhow::Result<()> {
    let value: serde_json::Value =
        serde_json::from_str(script).with_context(|| format!("parsing {SCRIPT_PATH}"))?;
    let edit = value["turns"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|turn| turn["tool_calls"].as_array().into_iter().flatten())
        .find(|call| call["name"] == "edit_file" && call["arguments"]["path"] == "README.md")
        .with_context(|| format!("{SCRIPT_PATH} has no edit_file call on README.md"))?;
    let old = edit["arguments"]["old"].as_str().unwrap_or_default();
    let last = readme.trim_end().lines().last().unwrap_or_default();
    if old != last {
        bail!(
            "script anchor stale: {SCRIPT_PATH} replaces {old:?}, but README.md now ends with {last:?}; update the script's `old` and `new`"
        );
    }
    Ok(())
}

/// The selftest's `factory.toml`: the real worker settings with fake roles,
/// an empty allowlist and a protected path the ticket does not touch.
fn factory_config(real: &FactoryConfig, runtime: ContainerRuntime) -> FactoryConfig {
    let mut factory = real.clone();
    factory.roles.coder = FAKE_PROVIDER.to_owned();
    factory.reviewers = vec![ReviewerEntry {
        provider: FAKE_PROVIDER.to_owned(),
        stance: Stance::Correctness,
    }];
    factory.worker.runtime = runtime;
    factory.worker.egress_allow = Vec::new();
    factory.budget.wall_clock_minutes = 10;
    factory.protected.paths = vec!["deploy/**".to_owned()];
    factory
}

/// The anvil configuration: fake providers for every role, and the
/// in-loop Verify state's check set to `true`.
fn anvil_config() -> anyhow::Result<AnvilConfig> {
    let text = format!(
        r#"[providers.{FAKE_PROVIDER}]
protocol = "fake"
api_key_env = "{CANARY_ENV}"
model = "selftest"
context_window = 200000
supports_tools = true
supports_reasoning = false
script = "{SCRIPT_PATH}"

[providers.{FAKE_SUMMARIZER}]
protocol = "fake"
model = "selftest"
context_window = 200000
supports_tools = true
supports_reasoning = false
script = "{SUMMARIZER_SCRIPT_PATH}"

[roles]
coder = "{FAKE_PROVIDER}"
summarizer = "{FAKE_SUMMARIZER}"
reviewer = "{FAKE_PROVIDER}"

[tools]
check = "true"
"#
    );
    let config: AnvilConfig = toml::from_str(&text).context("parsing the selftest anvil config")?;
    config
        .validate()
        .context("validating the selftest anvil config")?;
    Ok(config)
}

/// The real factory profile with its roles pointed at the fake providers;
/// its permissions are kept, so the scripted edit goes through them.
///
/// # Arguments
///
/// * `real` - `deploy/nuc/anvil-factory.toml` from `main`.
///
/// # Errors
///
/// When the profile is not TOML.
pub fn selftest_profile(real: &str) -> anyhow::Result<String> {
    let mut table: toml::Table = toml::from_str(real).context("parsing the factory profile")?;
    let mut roles = toml::Table::new();
    roles.insert("coder".to_owned(), FAKE_PROVIDER.into());
    roles.insert("reviewer".to_owned(), FAKE_PROVIDER.into());
    roles.insert("summarizer".to_owned(), FAKE_SUMMARIZER.into());
    table.insert("roles".to_owned(), roles.into());
    let tools = table
        .entry("tools")
        .or_insert_with(|| toml::Table::new().into());
    if let Some(tools) = tools.as_table_mut() {
        tools.insert("check".to_owned(), "true".into());
    }
    Ok(toml::to_string(&table)?)
}

/// Every secret value the scan looks for: the canary, the GitHub tokens, and
/// each key the real anvil configuration names, when one can be read.
fn secret_values(home: Option<&Path>, repo_root: &Path, canary: &str) -> Vec<String> {
    let mut names = vec![TOKEN_ENV.to_owned(), FALLBACK_TOKEN_ENV.to_owned()];
    if let Ok(loaded) = anvil_types::config::load(home, repo_root) {
        names.extend(
            loaded
                .config
                .providers
                .values()
                .map(|provider| provider.api_key_env.clone()),
        );
    }
    if let Ok(factory) = crate::wiring::read_factory(repo_root) {
        names.push(factory.notify.webhook_env);
    }
    let mut values: Vec<String> = names
        .iter()
        .filter(|name| !name.is_empty())
        .filter_map(|name| std::env::var(name).ok())
        .filter(|value| !anvil_types::dotenv::is_placeholder(value))
        .collect();
    values.push(canary.to_owned());
    values.retain(|value| value.len() >= MIN_SECRET_LEN);
    values.sort();
    values.dedup();
    values
}

/// Collects every file under `dir` whose bytes contain one of `secrets`.
///
/// # Arguments
///
/// * `dir` - The folder to walk; symlinks are not followed.
/// * `secrets` - The values to look for.
/// * `hits` - Receives the matching paths.
///
/// # Errors
///
/// When a folder cannot be listed.
pub fn scan_tree(dir: &Path, secrets: &[String], hits: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("listing {}", dir.display()))? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let path = entry.path();
        if kind.is_dir() {
            scan_tree(&path, secrets, hits)?;
        } else if kind.is_file()
            && let Ok(bytes) = std::fs::read(&path)
            && contains_any(&bytes, secrets)
        {
            hits.push(path);
        }
    }
    Ok(())
}

/// Whether `haystack` contains any of `needles`.
fn contains_any(haystack: &[u8], needles: &[String]) -> bool {
    needles.iter().any(|needle| {
        let needle = needle.as_bytes();
        !needle.is_empty()
            && haystack
                .windows(needle.len())
                .any(|window| window == needle)
    })
}

/// Commits the working tree, untracked files included and ignored files not,
/// without touching the real index, `HEAD` or any branch.
///
/// The commit has no parent: a shallow clone (CI's checkout) cannot push its
/// truncated history to another repository, and the selftest needs none.
///
/// # Returns
///
/// The commit id.
async fn snapshot(repo_root: &Path, index: &Path) -> anyhow::Result<String> {
    let with_index = |args: &[&str]| {
        let mut command = Command::new("git");
        command
            .args([
                "-c",
                "user.name=factory selftest",
                "-c",
                "user.email=selftest@factory.invalid",
            ])
            .args(args)
            .current_dir(repo_root)
            .env("GIT_INDEX_FILE", index);
        command
    };
    run_git(with_index(&["read-tree", "HEAD"])).await?;
    run_git(with_index(&["add", "-A"])).await?;
    let tree = run_git(with_index(&["write-tree"])).await?;
    let head_tree = run_git(with_index(&["rev-parse", "HEAD^{tree}"])).await?;
    if tree != head_tree {
        println!("     | testing HEAD plus the working tree's uncommitted changes");
    }
    run_git(with_index(&[
        "commit-tree",
        &tree,
        "-m",
        "selftest: the working tree",
    ]))
    .await
}

/// Runs a prepared git command; its trimmed stdout, or its stderr as the error.
async fn run_git(mut command: Command) -> anyhow::Result<String> {
    let output = command.output().await.context("starting git")?;
    if !output.status.success() {
        bail!(
            "git failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Runs one podman command; its stdout, or its output as the error.
async fn podman(args: &[&str]) -> anyhow::Result<String> {
    let output = Command::new("podman")
        .args(args)
        .output()
        .await
        .context("starting podman")?;
    if !output.status.success() {
        bail!(
            "podman {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Gate B for the selftest: no eval, no release build.
struct SkippedEvalGate;

impl EvalGate for SkippedEvalGate {
    fn run<'a>(&'a self, _worktree: &'a Path) -> BoxFuture<'a, factory_core::Result<GateB>> {
        Box::pin(std::future::ready(Ok(GateB::Passed(EvalGateReport {
            regressions: Vec::new(),
            summary: "Gate B skipped: the selftest has no eval baseline and calls no model."
                .to_owned(),
            cost_usd: None,
        }))))
    }

    fn needs_release_build(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_step_line_says_pass_with_time_or_fail_with_the_error_chain() {
        assert_eq!(step_line(3, "ticket", 1.25, None), "PASS  3 ticket (1.2s)");
        let error = anyhow!("inner").context("outer");
        assert_eq!(
            step_line(12, "pipeline", 0.0, Some(&error)),
            "FAIL 12 pipeline\n     | outer: inner"
        );
    }

    #[test]
    fn the_shipped_script_matches_the_readme() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let script = std::fs::read_to_string(root.join(SCRIPT_PATH)).expect("script");
        let readme = std::fs::read_to_string(root.join("README.md")).expect("readme");
        check_anchor(&script, &readme).expect("anchor matches");
        let error = check_anchor(&script, "# changed\n").expect_err("stale");
        assert!(error.to_string().contains("script anchor stale"), "{error}");
    }

    #[test]
    fn the_profile_keeps_permissions_and_points_every_role_at_the_fake() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let real = std::fs::read_to_string(root.join(PROFILE_PATH)).expect("profile");
        let text = selftest_profile(&real).expect("rewrites");
        let table: toml::Table = toml::from_str(&text).expect("toml");
        assert_eq!(table["roles"]["coder"].as_str(), Some(FAKE_PROVIDER));
        assert_eq!(table["roles"]["reviewer"].as_str(), Some(FAKE_PROVIDER));
        assert_eq!(table["roles"]["summarizer"].as_str(), Some(FAKE_SUMMARIZER));
        assert_eq!(table["tools"]["check"].as_str(), Some("true"));
        let real: toml::Table = toml::from_str(&real).expect("toml");
        assert_eq!(table["permissions"], real["permissions"]);
    }

    #[test]
    fn the_anvil_config_validates_and_names_the_canary() {
        let config = anvil_config().expect("valid");
        assert_eq!(config.providers[FAKE_PROVIDER].api_key_env, CANARY_ENV);
        assert_eq!(config.tools.check.as_deref(), Some("true"));
    }

    #[test]
    fn the_scan_finds_a_secret_in_a_nested_file_and_nothing_in_clean_ones() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("a/b")).expect("mkdir");
        std::fs::write(dir.path().join("a/b/leak.txt"), "x sk-secret-123 y").expect("write");
        std::fs::write(dir.path().join("clean.txt"), "nothing").expect("write");
        let mut hits = Vec::new();
        scan_tree(dir.path(), &["sk-secret-123".to_owned()], &mut hits).expect("scans");
        assert_eq!(hits, vec![dir.path().join("a/b/leak.txt")]);
    }
}
