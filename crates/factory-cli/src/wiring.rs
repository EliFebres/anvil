//! Builds the service from the repository, the configuration and the environment.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anvil_types::factory_config::FACTORY_FILE;
use anvil_types::{AnvilConfig, FactoryConfig, config};
use anyhow::Context;
use factory_core::github::{FALLBACK_TOKEN_ENV, TOKEN_ENV};
use factory_core::{
    FactoryHome, Git, GitHub, LogNotifier, Notifier, Sandbox, SandboxConfig, Service, ServiceDeps,
    TicketQueue, TicketWorker, WebhookNotifier, WorkerDeps, hostname, system_clock,
};
use factory_review::{ModelReviewer, load_rules};
use tokio::process::Command;

use crate::adapters::eval_gate::{HarnessGate, RunnerSource};
use crate::adapters::review_swarm::Swarm;

/// The remote the factory fetches from and pushes to.
pub const REMOTE: &str = "origin";

/// The profile the implementer runs with, read from `main` so a pull request
/// cannot loosen its own permissions.
pub const PROFILE_PATH: &str = "deploy/nuc/anvil-factory.toml";

/// The eval task folders, relative to the repository root.
pub const TASKS_DIR: &str = "eval/tasks";

/// The repository and its two configuration files.
pub struct Loaded {
    /// The repository root.
    pub root: PathBuf,
    /// The home directory, if known.
    pub home: Option<PathBuf>,
    /// `factory.toml`.
    pub factory: FactoryConfig,
    /// The merged anvil configuration.
    pub anvil: AnvilConfig,
}

/// Finds the repository root from `cwd` and reads both configurations.
///
/// # Arguments
///
/// * `cwd` - Any folder inside the repository.
/// * `home` - The home directory, if known.
///
/// # Returns
///
/// The loaded configuration.
///
/// # Errors
///
/// When `factory.toml` or the anvil configuration is missing or invalid.
pub async fn load(cwd: &Path, home: Option<&Path>) -> anyhow::Result<Loaded> {
    let root = repo_root(cwd).await;
    let factory = read_factory(&root)?;
    let anvil = config::load(home, &root)?.config;
    Ok(Loaded {
        root,
        home: home.map(Path::to_path_buf),
        factory,
        anvil,
    })
}

/// The git toplevel of `cwd`, or `cwd` itself outside a repository.
pub async fn repo_root(cwd: &Path) -> PathBuf {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .output()
        .await;
    match output {
        Ok(output) if output.status.success() => {
            PathBuf::from(String::from_utf8_lossy(&output.stdout).trim())
        }
        _ => cwd.to_path_buf(),
    }
}

/// Reads and validates `<root>/factory.toml`.
pub fn read_factory(root: &Path) -> anyhow::Result<FactoryConfig> {
    let path = root.join(FACTORY_FILE);
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}; run `anvil setup` first", path.display()))?;
    let factory: FactoryConfig =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    factory
        .validate()
        .with_context(|| format!("validating {}", path.display()))?;
    Ok(factory)
}

/// The notifier for `[notify] webhook_env`: the webhook when the variable is
/// set, the log otherwise.
///
/// # Arguments
///
/// * `factory` - `factory.toml`.
///
/// # Errors
///
/// When the HTTP client cannot be built.
pub fn notifier(factory: &FactoryConfig) -> anyhow::Result<Arc<dyn Notifier>> {
    match std::env::var(&factory.notify.webhook_env) {
        Ok(url) if !anvil_types::dotenv::is_placeholder(&url) => {
            Ok(Arc::new(WebhookNotifier::new(&url)?))
        }
        _ => {
            tracing::warn!(
                env = factory.notify.webhook_env,
                "no webhook URL; notifications go to the log"
            );
            Ok(Arc::new(LogNotifier))
        }
    }
}

/// Every secret value this process holds: provider keys, GitHub tokens and
/// the webhook URL. The worker scans the worktree for them before any push.
///
/// # Arguments
///
/// * `loaded` - The configuration naming the variables.
pub fn secret_values(loaded: &Loaded) -> Vec<String> {
    let mut names: Vec<&str> = loaded
        .anvil
        .providers
        .values()
        .map(|provider| provider.api_key_env.as_str())
        .collect();
    names.push(TOKEN_ENV);
    names.push(FALLBACK_TOKEN_ENV);
    names.push(loaded.factory.notify.webhook_env.as_str());
    names
        .into_iter()
        .filter_map(|name| std::env::var(name).ok())
        .filter(|value| !anvil_types::dotenv::is_placeholder(value))
        .collect()
}

/// The `factory` profile text, from `main` when it is there and from the
/// working copy otherwise (before the first merge that adds it).
async fn profile_text(root: &Path) -> anyhow::Result<String> {
    let repo = Git::new(root);
    if let Ok(text) = repo.show(&format!("{REMOTE}/main"), PROFILE_PATH).await {
        return Ok(text);
    }
    tracing::warn!("{PROFILE_PATH} is not on {REMOTE}/main yet; using the working copy");
    std::fs::read_to_string(root.join(PROFILE_PATH))
        .with_context(|| format!("reading {PROFILE_PATH}"))
}

/// Builds the sandbox for a configuration.
///
/// # Arguments
///
/// * `loaded` - The configuration.
/// * `home` - `~/.factory`.
///
/// # Errors
///
/// When the profile cannot be read or the configuration cannot be serialized.
pub async fn sandbox(loaded: &Loaded, home: &FactoryHome) -> anyhow::Result<Sandbox> {
    let anvil_config =
        toml::to_string_pretty(&loaded.anvil).context("serializing the anvil configuration")?;
    Ok(Sandbox::new(SandboxConfig {
        runtime: loaded.factory.worker.runtime,
        image: loaded.factory.worker.image.clone(),
        anvil_bin: loaded.factory.worker.anvil_bin.clone(),
        just_bin: "just".to_owned(),
        cargo_bin: "cargo".to_owned(),
        egress_allow: loaded.factory.worker.egress_allow.clone(),
        providers: loaded.anvil.providers.clone(),
        anvil_config,
        profile: profile_text(&loaded.root).await?,
        home: home.clone(),
    }))
}

/// Builds the service with the real GitHub, sandbox, harness, swarm and notifier.
///
/// # Arguments
///
/// * `loaded` - The configuration.
/// * `home` - `~/.factory`.
///
/// # Returns
///
/// The service.
///
/// # Errors
///
/// When the state branch is missing, a token or key is unset, the reviewers
/// are not independent, or the configuration refuses to run.
pub async fn build_service(loaded: &Loaded, home: FactoryHome) -> anyhow::Result<Service> {
    let repo = Git::new(&loaded.root);
    let queue = TicketQueue::open(&repo, &home.state_dir(), REMOTE).await?;
    let host = Arc::new(GitHub::from_env(&loaded.factory.github.repo)?);
    loaded
        .factory
        .check_independence(&loaded.anvil)
        .context("the reviewers in factory.toml are not independent")?;
    let rules = load_rules(loaded.home.as_deref())?;
    let reviewers = ModelReviewer::from_config(&loaded.factory, &loaded.anvil, &rules)?;
    let sandbox = Arc::new(sandbox(loaded, &home).await?);
    let eval_gate = HarnessGate::new(
        RunnerSource::Sandbox(Arc::clone(&sandbox)),
        Git::new(&loaded.root),
        REMOTE,
        &loaded.root.join(TASKS_DIR),
        loaded.factory.eval.parallel,
        &loaded.factory.roles.coder,
    );
    let worker = TicketWorker::new(WorkerDeps {
        repo: Git::new(&loaded.root),
        remote: REMOTE.to_owned(),
        home: home.clone(),
        config: loaded.factory.clone(),
        implementer: sandbox,
        eval_gate: Arc::new(eval_gate),
        review: Arc::new(Swarm::new(reviewers)),
        host: Arc::clone(&host) as Arc<_>,
        secrets: secret_values(loaded),
        clock: Arc::new(Instant::now),
    })?;
    let service = Service::new(ServiceDeps {
        home,
        config: loaded.factory.clone(),
        queue,
        worker: Arc::new(worker),
        host,
        notifier: notifier(&loaded.factory)?,
        hostname: hostname(),
        clock: system_clock(),
    })?;
    Ok(service)
}
