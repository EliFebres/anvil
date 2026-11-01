//! Builds the service from the repository, the configuration and the environment.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anvil_types::factory_config::FACTORY_FILE;
use anvil_types::{AnvilConfig, FactoryConfig, Ticket, config};
use anyhow::Context;
use factory_core::{
    BoxFuture, FactoryHome, Git, GitHub, LogNotifier, Notifier, Outcome, Progress, Service,
    ServiceDeps, TicketQueue, WebhookNotifier, Worker, hostname, system_clock,
};
use tokio::process::Command;

/// The remote the factory fetches from and pushes to.
pub const REMOTE: &str = "origin";

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

/// Builds the service with the real GitHub, notifier and queue.
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
/// When the state branch is missing, GitHub cannot be reached, or the
/// configuration refuses to run.
pub async fn build_service(loaded: &Loaded, home: FactoryHome) -> anyhow::Result<Service> {
    let repo = Git::new(&loaded.root);
    let queue = TicketQueue::open(&repo, &home.state_dir(), REMOTE).await?;
    let host = Arc::new(GitHub::from_env(&loaded.factory.github.repo)?);
    let service = Service::new(ServiceDeps {
        home,
        config: loaded.factory.clone(),
        queue,
        worker: Arc::new(PendingWorker),
        host,
        notifier: notifier(&loaded.factory)?,
        hostname: hostname(),
        clock: system_clock(),
    })?;
    Ok(service)
}

/// The worker loop is not built yet; every claimed ticket is handed back
/// and the service pauses, so no ticket is lost or half-worked.
struct PendingWorker;

impl Worker for PendingWorker {
    fn work<'a>(
        &'a self,
        _ticket: &'a Ticket,
        _queue: &'a TicketQueue,
        _progress: Progress,
    ) -> BoxFuture<'a, factory_core::Result<Outcome>> {
        Box::pin(std::future::ready(Ok(Outcome::Paused {
            reason:
                "the worker loop is not built yet; this factory can queue tickets but not work them"
                    .to_owned(),
        })))
    }
}
