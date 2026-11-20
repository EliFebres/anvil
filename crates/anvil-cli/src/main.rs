//! `anvil`: the CLI coding agent. Argument parsing and wiring only.

mod build;
mod chat;
mod cli;
mod commands;
mod config_init;
mod console;
mod doctor;
mod frontend;
mod logging;
mod memory;
mod policy;
mod render;
mod rules;
mod secrets;
mod setup;
mod skills;
mod stats;
mod status;
mod telemetry;
mod templates;
mod terminal;
mod trust;
mod tui;
mod wiring;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anvil_types::config::load_profile;
use anvil_types::dotenv;
use anvil_types::policy::LoadedPolicy;
use clap::Parser;

use crate::cli::{
    BuildCommand, Cli, Command, CompatCommand, ConfigCommand, PolicyCommand, TemplateCommand,
};
use crate::frontend::UiKind;
use crate::wiring::{ProjectAccess, Wiring, home_dir};

/// Exit code for configuration and setup errors.
const SETUP_FAILURE: i32 = 1;

/// Where the process runs and which `.env` files it loaded.
struct Env {
    /// The project root.
    cwd: PathBuf,
    /// The home directory, if known.
    home: Option<PathBuf>,
    /// The `.env` files anvil loads, present or not: only the user's own.
    env_files: Vec<PathBuf>,
    /// The `.env` files that existed and were loaded.
    loaded_env: Vec<PathBuf>,
    /// The managed policy, when one is installed.
    policy: Option<Arc<LoadedPolicy>>,
}

fn main() {
    let cli = Cli::parse();
    let code = match start(cli) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error:#}");
            SETUP_FAILURE
        }
    };
    std::process::exit(code);
}

/// Loads the managed policy, loads the user's `.env` file, picks the front end, then starts the runtime and
/// runs the command.
///
/// The policy comes first: when it exists but cannot be used, anvil stops
/// before it touches anything. The `.env` file is loaded before any thread
/// exists: filling the environment
/// is only sound while the process is single-threaded, and the log filter
/// (`ANVIL_LOG`) may come from a `.env` file. The front end is chosen
/// before logging starts, because the full-screen UI must not share
/// stderr with the console log.
fn start(cli: Cli) -> anyhow::Result<i32> {
    let policy = policy::load_system()?.map(Arc::new);
    let cwd = std::env::current_dir()?;
    let home = home_dir();
    let env_files = dotenv::env_files(home.as_deref());
    let loaded_env = dotenv::load(&env_files)?;
    let interactive = matches!(
        cli.command,
        Some(Command::Chat | Command::Run { .. } | Command::Resume { .. })
    ) || (cli.command.is_none() && !cli.task.is_empty());
    let ui = UiKind::choose(cli.plain, cli.yes, interactive);
    let _log_guard = logging::init(&cwd, ui == UiKind::Plain);
    let env = Env {
        cwd,
        home,
        env_files,
        loaded_env,
        policy,
    };
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(dispatch(cli, env, ui))
}

/// The wiring for a command that works in the repository: the user's
/// configuration, the repository's tightening rules, and what the
/// repository ships when the user trusts it.
///
/// # Arguments
///
/// * `env` - Where the process runs.
/// * `profile` - The `--profile` name, if any.
/// * `ask_trust` - Whether a person is at the terminal to answer the trust
///   prompt; without one an undecided repository stays untrusted.
///
/// # Returns
///
/// The wiring.
///
/// # Errors
///
/// When the configuration, the repository's rules or the trust file are
/// malformed, or when the egress rules refuse an endpoint the
/// configuration names.
fn project_wiring(env: &Env, profile: Option<&str>, ask_trust: bool) -> anyhow::Result<Wiring> {
    let wiring = unchecked_wiring(env, profile, ask_trust)?;
    wiring.check_endpoints()?;
    Ok(wiring)
}

/// [`project_wiring`] without the endpoint check, for `anvil doctor`, which
/// reports a refused endpoint as a row instead of stopping.
fn unchecked_wiring(env: &Env, profile: Option<&str>, ask_trust: bool) -> anyhow::Result<Wiring> {
    let home = env.home.as_deref();
    let loaded = load_profile(home, profile)?;
    if let Some(policy) = &env.policy {
        policy::check_config(policy, &loaded)?;
    }
    let rules = anvil_types::project::load(&env.cwd)?;
    for line in &rules.ignored {
        eprintln!("warning: {line}");
    }
    let allow_trust = anvil_types::policy::allows_project_trust(env.policy.as_deref());
    let trusted = trust::decide(&env.cwd, home, ask_trust, allow_trust)?;
    let project = ProjectAccess { trusted, rules };
    let wiring = Wiring::new_in(env.cwd.clone(), env.home.clone(), loaded.config, project)?;
    match &env.policy {
        Some(policy) => wiring.with_policy(Arc::clone(policy)),
        None => Ok(wiring),
    }
}

/// The egress rules `anvil setup --endpoint` probes under: the policy's,
/// or unmanaged mode allowing the URL the user typed, with their own http
/// hosts if a configuration already exists.
fn setup_egress(env: &Env, url: &str) -> anvil_core::provider::Egress {
    use anvil_core::provider::Egress;
    if let Some(loaded) = &env.policy {
        return Egress::from_policy(&loaded.policy.network);
    }
    let hosts = load_profile(env.home.as_deref(), None)
        .map(|loaded| loaded.config.network.allow_http_hosts)
        .unwrap_or_default();
    Egress::personal([url], hosts)
}

/// Prepares a session: says which policy is in force (on stderr, for the
/// line renderer; the full-screen UI shows it on its startup screen), and
/// deletes this repository's session folders older than the policy's
/// `retention_days`.
fn before_session(wiring: &Wiring, ui: UiKind) {
    if ui == UiKind::Plain {
        eprintln!("{}", policy::startup_line(wiring.policy.as_deref()));
    }
    let Some(days) = wiring
        .policy
        .as_ref()
        .and_then(|loaded| loaded.policy.data.retention_days)
    else {
        return;
    };
    let removed =
        policy::prune_sessions(&wiring.sessions_root(), days, std::time::SystemTime::now());
    if removed > 0 {
        eprintln!("note: removed {removed} session folder(s) older than {days} day(s)");
    }
}

/// Runs the chosen command and returns its exit code.
async fn dispatch(cli: Cli, env: Env, ui: UiKind) -> anyhow::Result<i32> {
    let cwd = &env.cwd;
    let home = env.home.as_deref();
    let profile = cli.profile.as_deref();
    let load_config = |home: Option<&Path>| load_profile(home, profile);
    let ask_trust = ui == UiKind::Tui;
    policy::check_flags(
        env.policy.as_deref(),
        policy::SkipFlags {
            yes: cli.yes,
            allow_tool: !cli.allow_tools.is_empty(),
        },
    )?;
    match cli.command {
        Some(Command::Config {
            command: ConfigCommand::Init { global },
        }) => {
            let path = config_init::run(global)?;
            println!("wrote {}", path.display());
            Ok(0)
        }
        Some(Command::Config {
            command: ConfigCommand::Show,
        }) => {
            let loaded = load_config(home)?;
            commands::config_show(&loaded);
            Ok(0)
        }
        Some(Command::Setup {
            endpoint,
            model,
            key_env,
        }) => match endpoint {
            Some(url) => {
                let egress = setup_egress(&env, &url);
                let args = setup::endpoint::EndpointArgs {
                    url: &url,
                    model: model.as_deref(),
                    key_env: key_env
                        .as_deref()
                        .unwrap_or(setup::endpoint::DEFAULT_KEY_ENV),
                };
                setup::endpoint::run(home, args, &egress).await
            }
            None if env.policy.is_some() => anyhow::bail!(
                "a managed policy is installed; run `anvil setup --endpoint <url>` with an allowed endpoint instead of writing the example provider"
            ),
            None => setup::run(home),
        },
        Some(Command::ScanKeys) => {
            let mut files = env.env_files.clone();
            files.push(dotenv::project_env_file(cwd));
            secrets::run(cwd, &files)
        }
        Some(Command::Trust { revoke }) => trust::run(
            cwd,
            home,
            revoke,
            anvil_types::policy::allows_project_trust(env.policy.as_deref()),
        ),
        Some(Command::Policy {
            command: PolicyCommand::Show,
        }) => {
            let loaded = load_config(home).ok();
            print!(
                "{}",
                policy::show(env.policy.as_deref(), loaded.as_ref(), profile)
            );
            Ok(0)
        }
        Some(Command::Policy {
            command: PolicyCommand::Check { file, ownership },
        }) => {
            let (text, code) = policy::check_file(&file, ownership);
            print!("{text}");
            Ok(code)
        }
        Some(Command::Sessions) => {
            commands::sessions(&policy::sessions_root(cwd, env.policy.as_deref()))?;
            Ok(0)
        }
        Some(Command::Skill { command }) => skills::run(command, home, cwd).await,
        Some(Command::Rules { command }) => rules::run(command, home, cwd),
        Some(Command::New { template, dir }) => templates::new_project(home, cwd, &template, &dir),
        Some(Command::Template {
            command:
                TemplateCommand::Save {
                    name,
                    dir,
                    description,
                    force,
                },
        }) => templates::save(
            home,
            cwd,
            &name,
            dir.as_deref(),
            description.as_deref(),
            force,
        ),
        Some(Command::Stats { since }) => {
            let config = load_config(home).ok().map(|loaded| loaded.config);
            stats::run(config.as_ref(), cwd, &since)
        }
        Some(Command::Status { json, all }) => {
            status::list::run(
                cwd,
                home,
                all,
                json,
                chrono::Utc::now(),
                env.policy.as_deref(),
                &mut std::io::stdout(),
            )?;
            Ok(0)
        }
        Some(Command::Compat {
            command: CompatCommand::Check { path },
        }) => {
            let config = load_config(home).ok().map(|loaded| loaded.config);
            commands::compat_check(&path, config.as_ref())
        }
        Some(Command::Doctor) => {
            let wiring = unchecked_wiring(&env, profile, false)?;
            let options = doctor::Options {
                env_files: &env.env_files,
                loaded_env: &env.loaded_env,
            };
            doctor::run(&wiring, options).await
        }
        Some(Command::Build(args)) => {
            if let Some(BuildCommand::Status { id }) = &args.command {
                build::status(cwd, id.as_deref())?;
                return Ok(0);
            }
            let mut args = args;
            let caps = policy::check_build(
                env.policy.as_deref(),
                policy::BuildCaps {
                    tokens: args.budget_tokens,
                    usd: args.budget_usd,
                },
            )?;
            args.budget_tokens = caps.tokens;
            args.budget_usd = caps.usd;
            let mut wiring = project_wiring(&env, profile, ask_trust)?;
            wiring.allow_tools(&cli.allow_tools)?;
            before_session(&wiring, UiKind::Plain);
            wiring.load_mcp().await;
            build::execute(args, wiring, cli.yes).await
        }
        Some(Command::Chat) => {
            let mut wiring = project_wiring(&env, profile, ask_trust)?;
            wiring.allow_tools(&cli.allow_tools)?;
            before_session(&wiring, ui);
            wiring.load_mcp().await;
            chat::chat(&wiring, cli.yes, ui).await
        }
        Some(Command::Resume { id }) => {
            let mut wiring = project_wiring(&env, profile, ask_trust)?;
            wiring.allow_tools(&cli.allow_tools)?;
            before_session(&wiring, ui);
            wiring.load_mcp().await;
            commands::resume(&wiring, &id, cli.yes, ui).await
        }
        Some(Command::Run { task }) => {
            run(&env, profile, task, &cli.allow_tools, cli.yes, ui).await
        }
        None if !cli.task.is_empty() => {
            run(&env, profile, cli.task, &cli.allow_tools, cli.yes, ui).await
        }
        None => anyhow::bail!("nothing to do; pass a task or a subcommand (see --help)"),
    }
}

/// Runs a task given as words on the command line.
async fn run(
    env: &Env,
    profile: Option<&str>,
    words: Vec<String>,
    allow_tools: &[String],
    yes: bool,
    ui: UiKind,
) -> anyhow::Result<i32> {
    let task = words.join(" ");
    let mut wiring = project_wiring(env, profile, ui == UiKind::Tui)?;
    wiring.allow_tools(allow_tools)?;
    before_session(&wiring, ui);
    wiring.load_mcp().await;
    commands::run_task(&wiring, task, yes, ui).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, text).expect("write");
    }

    /// A repository with a deny rule and an instruction file, trusted by
    /// the user, and a user configuration; plus a policy with `extra`.
    fn env(dir: &Path, extra: &str) -> Env {
        let project = dir.join("repo");
        let home = dir.join("home");
        write(
            &project.join(".anvil").join("permissions.toml"),
            "deny = [\"shell\"]\n",
        );
        write(&project.join("CLAUDE.md"), "rules");
        write(
            &home.join(".anvil").join("config.toml"),
            anvil_types::config::CONFIG_TEMPLATE,
        );
        let mut store = anvil_types::trust::TrustStore::default();
        store.set(
            &project,
            anvil_types::trust::TrustDecision::Trusted,
            &trust::fingerprint(&project),
        );
        store.save(&home).expect("trust saved");
        let policy_path = dir.join("policy.toml");
        write(
            &policy_path,
            &format!(
                "version = 1\n[network]\nallowed_hosts = [\"models.internal.example:443\"]\n{extra}"
            ),
        );
        let policy = policy::load_from(&policy_path)
            .expect("valid")
            .map(Arc::new);
        Env {
            cwd: project,
            home: Some(home),
            env_files: Vec::new(),
            loaded_env: Vec::new(),
            policy,
        }
    }

    #[test]
    fn without_allow_trust_nothing_the_repository_ships_loads_but_its_rules_apply() {
        let dir = tempfile::tempdir().expect("dir");
        let open = project_wiring(
            &env(dir.path(), "[projects]\nallow_trust = true\n"),
            None,
            false,
        )
        .expect("wiring");
        assert!(open.project.trusted, "the user's trust decision counts");
        assert_eq!(open.project.rules.deny, ["shell"]);

        let dir = tempfile::tempdir().expect("dir");
        let off = project_wiring(&env(dir.path(), ""), None, false).expect("wiring");
        assert!(!off.project.trusted, "the policy forbids trust by default");
        assert_eq!(off.trusted_root(), None);
        assert_eq!(
            off.project.rules.deny,
            ["shell"],
            "the repository's tightening rules still apply"
        );
    }

    #[test]
    fn a_user_configuration_that_conflicts_with_the_policy_stops_the_session() {
        let dir = tempfile::tempdir().expect("dir");
        let env = env(dir.path(), "[models]\nproviders = [\"other\"]\n");
        let error = project_wiring(&env, None, false).err().expect("refused");
        assert!(
            format!("{error:#}").contains("[models] providers"),
            "{error:#}"
        );
    }
}
