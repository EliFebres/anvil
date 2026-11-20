//! The real [`Env`]: providers from the configuration, sessions through the
//! same function `anvil run` uses.

use std::path::Path;
use std::sync::{Arc, Mutex};

use anvil_core::agent::Status;
use anvil_core::provider::{Provider, TokenBudget};
use anvil_core::tools::{PermissionPrompt, PromptAnswer, StaticPrompt};

use super::run::{Env, SessionReport, SessionRequest};
use crate::audit::SessionAudit;
use crate::commands::{SessionOptions, end_status, run_session_in};
use crate::frontend::UiKind;
use crate::status::{Setup, StatusReporter};
use crate::wiring::Wiring;

/// What a build needs from the configuration.
pub struct WiringEnv {
    wiring: Wiring,
    yes: bool,
    /// The build's own audit log, once [`Env::open_build_log`] opened it.
    audit: Mutex<Option<Arc<SessionAudit>>>,
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
        Self {
            wiring,
            yes,
            audit: Mutex::new(None),
        }
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
        end_status(&status, &result.map(|result| result.status.exit_code()));
        report
    }

    fn open_build_log(&self, id: &str, dir: &Path) -> anyhow::Result<()> {
        let audit = SessionAudit::open(&self.wiring, id, dir)?;
        audit.start(&self.wiring, "build", self.yes);
        if let Some(detail) = audit.failure() {
            anyhow::bail!(
                "the build does not start: the managed policy's [audit] required = true and its audit log cannot be written: {detail}"
            );
        }
        if let Ok(mut slot) = self.audit.lock() {
            *slot = Some(Arc::new(audit));
        }
        Ok(())
    }

    fn close_build_log(&self, outcome: &str) {
        let audit = self.audit.lock().ok().and_then(|mut slot| slot.take());
        if let Some(audit) = audit {
            audit.end(outcome);
        }
    }

    fn permit_command(&self, command: &str) -> Result<(), String> {
        let prompt: Box<dyn PermissionPrompt> = if self.yes {
            Box::new(StaticPrompt(PromptAnswer::Yes))
        } else {
            Wiring::prompt()
        };
        let audit = self.audit.lock().ok().and_then(|slot| slot.clone());
        self.wiring.permit_shell(command, prompt, audit)
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

#[cfg(test)]
mod tests {
    use anvil_audit::audit::{AUDIT_FILE, AuditEvent, verify};

    use super::*;
    use crate::usage::SessionUsage;
    use crate::wiring::ProjectAccess;

    #[tokio::test]
    async fn acceptance_decisions_go_to_the_build_log_not_the_last_items() {
        let dir = tempfile::tempdir().expect("dir");
        let config: anvil_types::AnvilConfig =
            toml::from_str(anvil_types::config::CONFIG_TEMPLATE).expect("config parses");
        let project = dir.path().join("project");
        let home = dir.path().join("home");
        std::fs::create_dir_all(&project).expect("project");
        std::fs::create_dir_all(&home).expect("home");
        let wiring =
            Wiring::new_in(project, Some(home), config, ProjectAccess::default()).expect("wiring");
        // An item session that ended before the acceptance command is checked.
        let item_dir = dir.path().join("item");
        let item = SessionUsage::start(
            &wiring.with_session("item", &item_dir),
            "item",
            anvil_audit::SessionMode::Run,
            true,
        )
        .expect("item");
        item.observe_permissions(&wiring);
        item.finish(anvil_audit::SessionOutcome::Done);

        let env = WiringEnv::new(wiring, true);
        let build_dir = dir.path().join("build");
        env.open_build_log("b1", &build_dir).expect("build log");
        env.permit_command("echo hi").expect("permitted");
        env.close_build_log("done");

        let tags = |path: &Path| -> Vec<String> {
            let text = std::fs::read_to_string(path).expect("log");
            verify(&text)
                .expect("verifies")
                .records
                .iter()
                .map(|record| record.body.event.tag().to_owned())
                .collect()
        };
        let item_tags = tags(&item_dir.join(AUDIT_FILE));
        assert_eq!(item_tags.last().map(String::as_str), Some("session_end"));
        assert!(
            !item_tags.iter().any(|tag| tag == "permission_decision"),
            "{item_tags:?}"
        );
        let build = std::fs::read_to_string(build_dir.join(AUDIT_FILE)).expect("log");
        let records = verify(&build).expect("verifies").records;
        assert!(matches!(
            &records[0].body.event,
            AuditEvent::SessionStart { mode, .. } if mode == "build"
        ));
        assert!(records.iter().any(|record| matches!(
            &record.body.event,
            AuditEvent::PermissionDecision { tool, source, .. } if tool == "shell" && source == "yes_flag"
        )), "{records:#?}");
        assert_eq!(
            records.last().map(|r| r.body.event.tag()),
            Some("session_end")
        );
    }
}
