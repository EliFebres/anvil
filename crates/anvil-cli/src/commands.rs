//! The subcommands: run, resume, sessions, config show, compat check, and
//! the compat reports `anvil doctor` prints.

use std::path::Path;
use std::time::Instant;

use anvil_core::agent::{AgentLoop, LoopResult, State};
use anvil_core::context;
use anvil_telemetry::SessionMode;
use anyhow::{Context, bail};
use tokio::task::JoinHandle;

use crate::frontend::{Frontend, UiKind, session_info};
use crate::memory;
use crate::telemetry::SessionTelemetry;
use crate::tui::SessionKind;
use crate::wiring::{Wiring, home_dir};
use anvil_types::LoadedConfig;

/// Runs a task through the loop and returns the process exit code.
///
/// # Arguments
///
/// * `wiring` - Providers and paths.
/// * `task` - The task.
/// * `yes` - Skip plan approval.
///
/// # Returns
///
/// The exit code for the run's status.
///
/// # Errors
///
/// When providers or permissions cannot be set up.
pub async fn run_task(wiring: &Wiring, task: String, yes: bool) -> anyhow::Result<i32> {
    let (session_dir, id) = wiring.new_session_dir();
    eprintln!("session {id}");
    let wiring = wiring.with_session(&id, &session_dir);
    let telemetry = SessionTelemetry::start(&wiring, &id, SessionMode::Run);
    let task = wiring.expand_slash(&task).unwrap_or(task);
    let registry = wiring.registry(telemetry.prompt(yes))?;
    telemetry.record_skill_requires(&wiring, &registry);
    let mut config = wiring.loop_config(task, session_dir, &registry)?;
    let hooks = wiring.hook_runner();
    if let Some(hooks) = &hooks {
        config
            .extra_instructions
            .extend(Wiring::session_start_instruction(
                hooks.session_start().await,
            ));
    }
    let (frontend, events) = Frontend::start(
        UiKind::Plain,
        session_info(&wiring, SessionKind::Run, yes, State::Plan),
    );
    let (sink, tee) = telemetry.sink(Some(events));
    let mut agent = AgentLoop::new(
        config,
        wiring.loop_providers()?,
        registry,
        wiring.tool_context(),
        Wiring::gate(yes),
        sink,
    );
    if let Some(checks) = wiring.change_checks()? {
        agent = agent.with_change_checks(checks);
    }
    let result = drive(agent, tee, frontend).await?;
    run_stop_hooks(hooks.as_deref(), &result).await;
    remember(&wiring, &id, yes, &telemetry).await;
    telemetry.finish_run(&result.status);
    Ok(result.status.exit_code())
}

/// Runs the memory extraction over the session log of a finished run.
async fn remember(wiring: &Wiring, id: &str, yes: bool, telemetry: &SessionTelemetry) {
    match context::load_session(&wiring.cwd, id) {
        Ok(snapshot) => {
            memory::finish_session(wiring, id, &snapshot.messages, yes, Some(telemetry)).await;
        }
        Err(error) => eprintln!("warning: memory extraction skipped: {error}"),
    }
}

/// Runs the Stop hooks after a loop finished. A hook that objects is
/// reported; the loop is not resumed.
async fn run_stop_hooks(hooks: Option<&anvil_compat::HookRunner>, result: &LoopResult) {
    if let Some(hooks) = hooks
        && let Some(reason) = hooks.stop(&result.summary, false).await
    {
        eprintln!("a Stop hook objected: {reason}");
    }
}

/// Continues a saved session.
///
/// # Arguments
///
/// * `wiring` - Providers and paths.
/// * `id` - The session id.
/// * `yes` - Skip plan approval if planning resumes.
///
/// # Returns
///
/// The exit code for the run's status.
///
/// # Errors
///
/// When the session cannot be loaded or providers cannot be built.
pub async fn resume(wiring: &Wiring, id: &str, yes: bool) -> anyhow::Result<i32> {
    let snapshot = context::load_session(&wiring.cwd, id).context("loading the session")?;
    eprintln!("resuming session {id}: {}", snapshot.meta.task);
    let wiring = wiring.with_session(id, &snapshot.dir);
    let telemetry = SessionTelemetry::start(&wiring, id, SessionMode::Resume);
    let registry = wiring.registry(telemetry.prompt(yes))?;
    telemetry.record_skill_requires(&wiring, &registry);
    let mut config =
        wiring.loop_config(snapshot.meta.task.clone(), snapshot.dir.clone(), &registry)?;
    let hooks = wiring.hook_runner();
    if let Some(hooks) = &hooks {
        config
            .extra_instructions
            .extend(Wiring::session_start_instruction(
                hooks.session_start().await,
            ));
    }
    let (frontend, events) = Frontend::start(
        UiKind::Plain,
        session_info(&wiring, SessionKind::Run, yes, State::Plan),
    );
    let (sink, tee) = telemetry.sink(Some(events));
    let mut agent = AgentLoop::resume(
        snapshot,
        config,
        wiring.loop_providers()?,
        registry,
        wiring.tool_context(),
        Wiring::gate(yes),
        sink,
    );
    if let Some(checks) = wiring.change_checks()? {
        agent = agent.with_change_checks(checks);
    }
    let result = drive(agent, tee, frontend).await?;
    run_stop_hooks(hooks.as_deref(), &result).await;
    remember(&wiring, id, yes, &telemetry).await;
    telemetry.finish_run(&result.status);
    Ok(result.status.exit_code())
}

/// Runs the loop, then waits for every event to be recorded and rendered.
async fn drive(
    agent: AgentLoop,
    tee: JoinHandle<()>,
    frontend: Frontend,
) -> anyhow::Result<LoopResult> {
    let result = agent.run().await;
    let _ = tee.await;
    frontend.shutdown(false).await?;
    Ok(result)
}

/// Prints the saved sessions, newest first.
///
/// # Arguments
///
/// * `cwd` - The project root.
///
/// # Errors
///
/// When the sessions folder cannot be read.
pub fn sessions(cwd: &Path) -> anyhow::Result<()> {
    let sessions = context::list_sessions(cwd).context("listing sessions")?;
    if sessions.is_empty() {
        println!(
            "no sessions under {}",
            cwd.join(context::session::SESSIONS_DIR).display()
        );
        return Ok(());
    }
    for session in sessions {
        let status = session
            .status
            .map(|status| status.to_string())
            .or_else(|| {
                session
                    .last_state
                    .map(|state| format!("stopped in {state}"))
            })
            .unwrap_or_else(|| "empty".to_owned());
        println!(
            "{}  {}  {}  {}",
            session.meta.id,
            session.meta.created_at.format("%Y-%m-%d %H:%M"),
            status,
            session.meta.task
        );
    }
    Ok(())
}

/// Prints the merged configuration as TOML.
///
/// # Arguments
///
/// * `loaded` - The merged configuration.
pub fn config_show(loaded: &LoadedConfig) {
    for source in &loaded.sources {
        println!("# from {}", source.display());
    }
    println!(
        "{}",
        toml::to_string_pretty(&loaded.merged).unwrap_or_default()
    );
}

/// Connects to every MCP server, lists its tools and refreshes the cache.
///
/// # Arguments
///
/// * `wiring` - The MCP server definitions.
///
/// # Returns
///
/// How many servers failed.
pub(crate) async fn report_mcp(wiring: &Wiring) -> usize {
    let servers = &wiring.mcp_servers;
    println!("mcp servers: {} defined", servers.servers().len());
    for error in servers.errors() {
        println!("mcp error: {}", error.error);
    }
    if servers.is_empty() {
        return 0;
    }
    let started = Instant::now();
    let tools = anvil_compat::McpTools::load(servers, &wiring.cwd, true).await;
    let millis = started.elapsed().as_millis();
    let mut failures = 0;
    for report in tools.reports() {
        match &report.tools {
            Ok(count) => println!("mcp {}: ok, {count} tool(s)", report.name),
            Err(error) => {
                failures += 1;
                println!("mcp {}: FAILED: {error}", report.name);
            }
        }
    }
    for name in tools.names() {
        println!("mcp tool: {name}");
    }
    println!("mcp: connected and listed in {millis} ms");
    failures
}

/// Prints one line per skill, then unmet requirements and load errors.
pub(crate) fn report_skills(wiring: &Wiring) -> anyhow::Result<()> {
    let skills = &wiring.skills;
    println!("skills: {} loaded", skills.skills().len());
    for skill in skills.skills() {
        println!(
            "skill {}: {} ({})",
            skill.name(),
            crate::skills::source_label(skill),
            skill.dir.display()
        );
    }
    let registry = wiring.registry(Wiring::prompt())?;
    for (skill, requirement) in skills.missing_requires(&registry.names()) {
        println!("skill {skill}: requires `{requirement}`, which is not available");
    }
    for error in skills.errors() {
        println!("skill error: {}", error.error);
    }
    Ok(())
}

/// Prints one line per slash command, then skipped files and load errors.
pub(crate) fn report_commands(wiring: &Wiring) {
    let commands = &wiring.commands;
    println!("commands: {} loaded", commands.commands().len());
    for command in commands.commands() {
        println!(
            "command /{}: {} ({})",
            command.name,
            command.origin,
            command.path.display()
        );
    }
    for path in commands.skipped() {
        println!(
            "command skipped: {} (only Markdown commands are read)",
            path.display()
        );
    }
    for error in commands.errors() {
        println!("command error: {}", error.error);
    }
}

/// Prints one line per subagent with the provider it resolves to.
pub(crate) fn report_agents(wiring: &Wiring) {
    let agents = &wiring.agents;
    println!("agents: {} loaded", agents.agents().len());
    for agent in agents.agents() {
        let (provider, warning) = wiring.resolve_model(agent.model.as_deref());
        println!(
            "agent {}: {} provider={provider} ({})",
            agent.name,
            agent.origin,
            agent.path.display()
        );
        if let Some(warning) = warning {
            println!("agent {}: {warning}", agent.name);
        }
    }
    for error in agents.errors() {
        println!("agent error: {}", error.error);
    }
}

/// Prints the hook summary, then entries anvil does not run and bad files.
pub(crate) fn report_hooks(wiring: &Wiring) {
    let hooks = &wiring.hooks;
    let events: Vec<String> = hooks.events().iter().map(ToString::to_string).collect();
    println!(
        "hooks: {} command(s) for {}",
        hooks.command_count(),
        if events.is_empty() {
            "no events".to_owned()
        } else {
            events.join(", ")
        }
    );
    for source in hooks.sources() {
        println!("hook settings: {}", source.display());
    }
    for (name, source) in hooks.unsupported() {
        println!(
            "hook unsupported: {name} in {} (anvil runs PreToolUse, PostToolUse, Stop and SessionStart command hooks)",
            source.display()
        );
    }
    for error in hooks.errors() {
        println!("hook error: {}", error.error);
    }
}

/// Prints what a `.claude` folder would load in anvil.
///
/// # Arguments
///
/// * `path` - The folder.
/// * `config` - The anvil config, when one loads here, to resolve agent models.
///
/// # Returns
///
/// Zero when nothing would fail to load, one otherwise.
///
/// # Errors
///
/// When the path is not a directory.
pub fn compat_check(path: &Path, config: Option<&anvil_types::AnvilConfig>) -> anyhow::Result<i32> {
    let root = if path.starts_with("~") {
        let home = home_dir().context("cannot determine the home directory")?;
        home.join(path.strip_prefix("~").unwrap_or(path))
    } else {
        path.to_path_buf()
    };
    if !root.is_dir() {
        bail!("{} is not a directory", root.display());
    }
    let builtins = anvil_core::tools::ToolRegistry::with_builtins(
        anvil_core::tools::Permissions::empty(),
        Box::new(anvil_core::tools::StaticPrompt(
            anvil_core::tools::PromptAnswer::No,
        )),
    );
    let mut tools: Vec<&str> = builtins.names();
    tools.extend(["skill", "delegate"]);
    let report = anvil_compat::check::check(&root, config, &tools);
    print!("{}", report.render());
    Ok(i32::from(report.has_errors()))
}
