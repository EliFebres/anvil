//! The machine probe the dashboard reads through: the worker image, the
//! anvil binary, disk and cargo cache, the eval baseline, provider latency,
//! the container running a ticket's step, and live usage.
//!
//! Every answer is best effort. A command that fails or prints something
//! unexpected leaves its field empty; nothing here can stop the service.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anvil_types::{ContainerRuntime, ProviderConfig};
use chrono::{DateTime, Utc};
use factory_core::board::{
    BaselineView, ContainerView, HealthView, ImageView, ProviderLatency, UsageView,
};
use factory_core::sandbox::{CACHE_VOLUME, TICKET_LABEL};
use factory_core::{BoxFuture, Git, Probe, usage};
use tokio::process::Command;

use super::eval_gate::newest_baseline;

/// How long one probing command may take.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(20);

/// How long one provider ping may take.
const PING_TIMEOUT: Duration = Duration::from_secs(30);

/// What the probe needs to know about this factory.
#[derive(Debug, Clone)]
pub struct MachineProbeConfig {
    /// podman, docker or none.
    pub runtime: ContainerRuntime,
    /// The worker image, such as `localhost/anvil-factory:latest`.
    pub image: String,
    /// The anvil binary the sandbox mounts.
    pub anvil_bin: PathBuf,
    /// A folder on the disk whose free space is shown: `~/.factory`.
    pub disk_path: PathBuf,
    /// `[providers]` from the anvil configuration.
    pub providers: BTreeMap<String, ProviderConfig>,
    /// The providers to ping: the coder's and the reviewers'.
    pub ping: Vec<String>,
    /// The coder provider, whose context window scales the chart.
    pub coder: Option<String>,
    /// The factory's clone, for the eval baseline on main.
    pub repo: Git,
    /// The remote, normally `origin`.
    pub remote: String,
}

/// The probe of this machine.
#[derive(Debug, Clone)]
pub struct MachineProbe {
    config: MachineProbeConfig,
}

impl MachineProbe {
    /// A probe for this factory.
    ///
    /// # Arguments
    ///
    /// * `config` - What to look at.
    ///
    /// # Returns
    ///
    /// The probe.
    pub fn new(config: MachineProbeConfig) -> Self {
        Self { config }
    }

    /// The container runtime's executable, or none without containers.
    fn runtime(&self) -> Option<&'static str> {
        match self.config.runtime {
            ContainerRuntime::Podman => Some("podman"),
            ContainerRuntime::Docker => Some("docker"),
            ContainerRuntime::None => None,
        }
    }

    /// The worker image's id and build time.
    async fn image(&self) -> Option<ImageView> {
        let runtime = self.runtime()?;
        let text = output(runtime, &["image", "inspect", &self.config.image]).await?;
        parse_image(&self.config.image, &text)
    }

    /// Bytes in the cargo cache volume.
    async fn cargo_cache(&self) -> Option<u64> {
        let runtime = self.runtime()?;
        let mountpoint = output(
            runtime,
            &[
                "volume",
                "inspect",
                CACHE_VOLUME,
                "--format",
                "{{.Mountpoint}}",
            ],
        )
        .await?;
        // du exits non-zero on one unreadable file but still prints the total.
        let text = output_any("du", &["-sb", mountpoint.trim()]).await?;
        text.split_whitespace().next()?.parse().ok()
    }
}

impl Probe for MachineProbe {
    fn health(&self) -> BoxFuture<'_, HealthView> {
        Box::pin(async move {
            let image = self.image().await;
            let sandbox_ready = match self.config.runtime {
                ContainerRuntime::None => Some(true),
                _ => Some(image.is_some()),
            };
            let anvil = self.config.anvil_bin.to_string_lossy().into_owned();
            let binary_version = output(&anvil, &["--version"])
                .await
                .map(|text| text.trim().to_owned())
                .filter(|text| !text.is_empty());
            let disk = self.config.disk_path.to_string_lossy().into_owned();
            let disk_free_bytes = output("df", &["-B1", "--output=avail", &disk])
                .await
                .and_then(|text| text.lines().nth(1)?.trim().parse().ok());
            let baseline = match newest_baseline(&self.config.repo, &self.config.remote).await {
                Ok(result) => result.map(|result| BaselineView {
                    passed: result.totals.tasks_passing,
                    total: result.totals.tasks,
                    model: result.model.or(result.provider),
                }),
                Err(error) => {
                    tracing::debug!(%error, "cannot read the eval baseline");
                    None
                }
            };
            HealthView {
                baseline,
                image,
                binary_version,
                disk_free_bytes,
                cargo_cache_bytes: self.cargo_cache().await,
                sandbox_ready,
                ..HealthView::default()
            }
        })
    }

    fn ping_providers(&self) -> BoxFuture<'_, Vec<ProviderLatency>> {
        Box::pin(async move {
            let mut latencies = Vec::new();
            for name in &self.config.ping {
                let Some(provider) = self.config.providers.get(name) else {
                    continue;
                };
                let millis = match std::env::var(&provider.api_key_env) {
                    Ok(key) => {
                        match factory_review::ping(name, provider, &key, PING_TIMEOUT).await {
                            Ok(millis) => Some(millis),
                            Err(error) => {
                                tracing::warn!(provider = name, %error, "provider ping failed");
                                None
                            }
                        }
                    }
                    Err(_) => None,
                };
                latencies.push(ProviderLatency {
                    name: name.clone(),
                    millis,
                });
            }
            latencies
        })
    }

    fn container<'a>(&'a self, ticket_id: &'a str) -> BoxFuture<'a, Option<ContainerView>> {
        Box::pin(async move {
            let runtime = self.runtime()?;
            let filter = format!("label={TICKET_LABEL}={ticket_id}");
            let ids = output(runtime, &["ps", "-q", "--filter", &filter]).await?;
            let id = ids.lines().next()?.trim().to_owned();
            if id.is_empty() {
                return None;
            }
            let stats = output(runtime, &["stats", "--no-stream", "--format", "json", &id]).await;
            let (cpu_pct, mem_bytes) = stats.as_deref().map(parse_stats).unwrap_or((None, None));
            Some(ContainerView {
                id: id.chars().take(7).collect(),
                cpu_pct,
                mem_bytes,
            })
        })
    }

    fn usage(&self, worktree: &Path) -> Option<UsageView> {
        let used = usage::read(worktree, &self.config.providers);
        let context_limit = self
            .config
            .coder
            .as_ref()
            .and_then(|name| self.config.providers.get(name))
            .map(|provider| u64::from(provider.context_window));
        Some(UsageView {
            input: used.tokens.input,
            output: used.tokens.output,
            cached: used.tokens.cached,
            cost_usd: used.cost_usd,
            context_per_turn: used.context_per_turn,
            context_limit,
        })
    }
}

/// A command's stdout when it succeeds within the timeout.
async fn output(program: &str, args: &[&str]) -> Option<String> {
    let out = tokio::time::timeout(COMMAND_TIMEOUT, Command::new(program).args(args).output())
        .await
        .ok()?
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A command's stdout whatever its exit code, within the timeout.
async fn output_any(program: &str, args: &[&str]) -> Option<String> {
    let out = tokio::time::timeout(COMMAND_TIMEOUT, Command::new(program).args(args).output())
        .await
        .ok()?
        .ok()?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The image view from `<runtime> image inspect` JSON.
fn parse_image(image: &str, text: &str) -> Option<ImageView> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let first = value.get(0)?;
    let id = first.get("Id")?.as_str()?;
    let id: String = id.trim_start_matches("sha256:").chars().take(7).collect();
    let created: DateTime<Utc> = first.get("Created")?.as_str()?.parse().ok()?;
    let name = image
        .rsplit('/')
        .next()
        .unwrap_or(image)
        .split(':')
        .next()
        .unwrap_or(image)
        .to_owned();
    Some(ImageView { name, id, created })
}

/// CPU percent and memory bytes from `podman stats --format json`.
fn parse_stats(text: &str) -> (Option<f64>, Option<u64>) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return (None, None);
    };
    let Some(first) = value.get(0) else {
        return (None, None);
    };
    let cpu = first
        .get("cpu_percent")
        .and_then(|cpu| cpu.as_str())
        .and_then(|cpu| cpu.trim_end_matches('%').trim().parse().ok());
    let mem = first
        .get("mem_usage")
        .and_then(|mem| mem.as_str())
        .and_then(|mem| parse_size(mem.split('/').next()?.trim()));
    (cpu, mem)
}

/// A size such as `94.21kB`, `1.4GB` or `512MiB` in bytes.
fn parse_size(text: &str) -> Option<u64> {
    let split = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let number: f64 = number.parse().ok()?;
    let factor: f64 = match unit.trim() {
        "" | "B" => 1.0,
        "kB" | "KB" => 1e3,
        "MB" => 1e6,
        "GB" => 1e9,
        "TB" => 1e12,
        "KiB" => 1024.0,
        "MiB" => 1024.0 * 1024.0,
        "GiB" => 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((number * factor) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_podman_output() {
        let image = parse_image(
            "localhost/anvil-factory:latest",
            r#"[{"Id":"216f20d9e203446e1b7d","Created":"2026-11-01T18:19:57.329708103Z"}]"#,
        )
        .expect("image");
        assert_eq!(image.name, "anvil-factory");
        assert_eq!(image.id, "216f20d");
        assert_eq!(
            image.created.to_rfc3339(),
            "2026-11-01T18:19:57.329708103+00:00"
        );

        let (cpu, mem) = parse_stats(
            r#"[{"id":"c6bd20386bb0","cpu_percent":"66.76%","mem_usage":"1.4GB / 32.92GB"}]"#,
        );
        assert_eq!(cpu, Some(66.76));
        assert_eq!(mem, Some(1_400_000_000));
        assert_eq!(parse_stats("not json"), (None, None));
        assert_eq!(parse_size("94.21kB"), Some(94_210));
        assert_eq!(parse_size("512MiB"), Some(512 * 1024 * 1024));
        assert_eq!(parse_size("12 parsecs"), None);
    }
}
