//! The real [`Env`]: providers from the configuration, sessions through the
//! same function `anvil run` uses.

use std::path::Path;
use std::sync::Arc;

use anvil_core::agent::Status;
use anvil_core::provider::{Provider, TokenBudget};

use super::run::{Env, SessionReport, SessionRequest};
use crate::commands::{SessionOptions, end_status, run_session_in};
use crate::frontend::UiKind;
use crate::status::{Setup, StatusReporter};
use crate::wiring::Wiring;

/// What a build needs from the configuration.
pub struct WiringEnv {
    wiring: Wiring,
    yes: bool,
}

impl WiringEnv {
    /// Wraps a wiring.
    ///
    /// # Arguments
    ///
    /// * `wiring` - Providers and paths.
    /// * `yes` - Whether `--yes` was given, which approves permission prompts.
    ///
    /// # Returns
    ///
    /// The environment.
    pub fn new(wiring: Wiring, yes: bool) -> Self {
        Self { wiring, yes }
    }
}

impl Env for WiringEnv {
    fn planner(&self) -> anyhow::Result<Arc<dyn Provider>> {
        let providers = self.wiring.loop_providers()?;
        Ok(providers.planner.unwrap_or(providers.coder))
    }

    async fn run_session(&self, request: SessionRequest) -> SessionReport {
        let SessionRequest {
            task,
            session_dir,
            token_limit,
        } = request;
        let id = session_dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        // A fresh budget makes the limit this item's share, through the
        // session limit the loop already enforces.
        let mut wiring = self.wiring.with_session(&id, &session_dir);
        wiring.budget = TokenBudget::shared(token_limit);
        wiring.config.limits.session_tokens = token_limit;
        eprintln!("session {id}");
        let status = StatusReporter::start(Setup::from_wiring(&wiring, &id, &session_dir, &task));
        let options = SessionOptions {
            yes: self.yes,
            ui: UiKind::Plain,
            in_build: true,
        };
        let result = run_session_in(&wiring, &id, session_dir, task, options, &status).await;
        let tokens = wiring.budget.lock().map_or(0, |budget| budget.used());
        let cost_usd = status.spend().cost_usd;
        let report = match &result {
            Ok(result) => SessionReport {
                reported_done: result.status == Status::Done,
                detail: result.status.to_string(),
                tokens,
                cost_usd,
            },
            Err(error) => SessionReport {
                reported_done: false,
                detail: format!("{error:#}"),
                tokens,
                cost_usd,
            },
        };
        end_status(&status, &result.map(|result| result.status.exit_code())).await;
        report
    }

    fn build_status(&self, id: &str, dir: &Path, spec: &str) -> Option<StatusReporter> {
        let task = format!("build {id} from {spec}");
        Some(StatusReporter::start(Setup::from_wiring(
            &self.wiring,
            id,
            dir,
            &task,
        )))
    }
}
