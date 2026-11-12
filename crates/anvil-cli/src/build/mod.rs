//! `anvil build`: work through a specification across many sessions.
//!
//! The planner splits the specification into work items; each item runs as
//! its own fresh session; an item is done only when anvil itself ran its
//! acceptance command and the project's check and both exited 0; the
//! changes are then committed. State lives in `.anvil/builds/<id>/`.
//!
//! `.anvil/` is never part of the clean-tree rule or of a build's commits
//! (see [`git`]); a build never pushes and only touches the branch it
//! started on and its own `anvil-build/<id>/...` side branches.

mod env;
mod git;
mod plan;
mod run;
mod state;

#[cfg(test)]
mod tests;

use std::path::Path;

use anvil_core::agent::{AutoApprove, PlanGate};
use anvil_types::config::AnvilConfig;

use self::env::WiringEnv;
use self::run::{Budgets, Settings};
use crate::cli::{BuildArgs, BuildCommand};
use crate::terminal::TerminalGate;
use crate::wiring::Wiring;

/// Prints a build's items; needs no providers.
///
/// # Arguments
///
/// * `cwd` - The project root.
/// * `id` - The build id; the newest build when `None`.
///
/// # Errors
///
/// When there is no such build.
pub fn status(cwd: &Path, id: Option<&str>) -> anyhow::Result<()> {
    run::print_status(cwd, id)
}

/// Starts or resumes a build.
///
/// # Arguments
///
/// * `args` - The `anvil build` arguments; `status` is handled by [`status`].
/// * `wiring` - Providers and paths.
/// * `yes` - Skip the approval of the item list and every permission prompt.
///
/// # Returns
///
/// The exit code: 0 when every item is done, 2 when the build stopped, 1
/// when the owner declined the plan.
///
/// # Errors
///
/// When the build cannot start or resume; see [`run::start`] and [`run::resume`].
pub async fn execute(args: BuildArgs, wiring: Wiring, yes: bool) -> anyhow::Result<i32> {
    let budgets = Budgets {
        usd: args.budget_usd,
        tokens: args.budget_tokens,
    };
    let settings = settings(&wiring);
    let env = WiringEnv::new(wiring, yes);
    match (args.command, args.spec) {
        (Some(BuildCommand::Resume { id }), _) => {
            run::resume(&env, &settings, id.as_deref(), budgets).await
        }
        (Some(BuildCommand::Status { .. }), _) => {
            anyhow::bail!("`status` does not start a build")
        }
        (None, Some(spec)) => {
            let gate: Box<dyn PlanGate> = if yes {
                Box::new(AutoApprove)
            } else {
                Box::new(TerminalGate)
            };
            run::start(&env, &settings, &spec, budgets, gate.as_ref()).await
        }
        (None, None) => anyhow::bail!(
            "pass a specification file, or `resume` or `status` (see `anvil build --help`)"
        ),
    }
}

/// The run settings the configuration gives.
fn settings(wiring: &Wiring) -> Settings {
    let config: &AnvilConfig = &wiring.config;
    Settings {
        cwd: wiring.cwd.clone(),
        shell: anvil_core::tools::Shell::from_config(config.tools.shell.as_deref()),
        check: config.tools.check.clone().map_or(
            anvil_core::agent::CheckCommand::Detect,
            anvil_core::agent::CheckCommand::Command,
        ),
        session_tokens: config.limits.session_tokens,
        price: config
            .providers
            .get(&config.roles.coder)
            .and_then(|provider| provider.price_per_mtoken),
    }
}
