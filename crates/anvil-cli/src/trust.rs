//! Whether to load what a repository ships, and `anvil trust`.
//!
//! A repository can ship hooks and MCP servers, which run programs, and
//! skills, slash commands, agents and instruction files, which steer the
//! model, and `.anvil/permissions.toml` can carry `allow` rules. None of
//! them take effect until the user has trusted the repository. The user is
//! asked with the full list in front of them, and the answer is kept in
//! `~/.anvil/trust.toml` with a SHA-256 digest of those files, so any
//! change to them asks again. Without a terminal to ask on, an undecided
//! or changed repository is treated as untrusted, never as trusted.

use std::collections::BTreeSet;
use std::io::{BufRead, Write};
use std::path::Path;

use anvil_compat::mcp::Transport;
use anvil_compat::{AgentSet, CommandSet, HookConfig, Origin, ServerSet, SkillRoots, SkillSet};
use anvil_core::context::discover_instructions;
use anvil_core::tools::workspace::command_files;
use anvil_types::project::PROJECT_PERMISSIONS_FILE;
use anvil_types::trust::{TrustDecision, TrustStore};
use anyhow::Context;
use sha2::{Digest, Sha256};

/// The line shown under the list, for instruction files found only later.
const NESTED_NOTE: &str =
    "and any CLAUDE.md in a subfolder, once the session reads a file in that folder";

/// Single files whose content the trust digest covers, relative to the
/// repository root: permission rules, settings that define hooks and MCP
/// servers, and the instruction files read at the root.
const DIGEST_FILES: &[&str] = &[
    PROJECT_PERMISSIONS_FILE,
    ".anvil/settings.json",
    ".anvil/settings.local.json",
    ".claude/settings.json",
    ".claude/settings.local.json",
    ".mcp.json",
    "ANVIL.md",
    "CLAUDE.md",
    "CLAUDE.local.md",
    ".claude/CLAUDE.md",
];

/// Folders whose every file the trust digest covers: skills (scripts
/// included), slash commands and agents.
const DIGEST_DIRS: &[&str] = &[
    ".claude/skills",
    ".claude/commands",
    ".claude/agents",
    ".anvil/skills",
    ".anvil/commands",
    ".anvil/agents",
];

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
    if let Ok(rules) = anvil_types::project::load(root) {
        for rule in rules.allow {
            items.push(format!("allow rule: {rule} ({PROJECT_PERMISSIONS_FILE})"));
        }
    }
    let base = discover_instructions(None, Some(root), &BTreeSet::new());
    let extra = anvil_compat::instructions::discover_extra(None, Some(root), &base);
    for instruction in base.iter().chain(&extra) {
        items.push(format!("instructions: {}", instruction.source));
    }
    let skills = SkillSet::discover(&SkillRoots {
        project: Some(root.to_path_buf()),
        home: None,
        bundled: None,
        managed: Vec::new(),
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
        let (command, args, env) = match &server.transport {
            Transport::Stdio { command, args, env } => (command, args, env),
            Transport::Http { url, headers } => {
                let names: Vec<&str> = headers.keys().map(String::as_str).collect();
                let headers = if names.is_empty() {
                    String::new()
                } else {
                    format!(" with the headers {}", names.join(", "))
                };
                items.push(format!(
                    "MCP server {}: contacts `{url}`{headers} ({})",
                    server.name,
                    server.source.display()
                ));
                continue;
            }
        };
        let env: String = env
            .iter()
            .map(|(key, value)| format!(" {key}={value}"))
            .collect();
        let env = if env.is_empty() {
            String::new()
        } else {
            format!(" with{env}")
        };
        items.push(format!(
            "MCP server {}: starts `{}`{env} ({})",
            server.name,
            format!("{command} {}", args.join(" ")).trim_end(),
            server.source.display()
        ));
    }
    items
}

/// The first symbolic link (or Windows junction) found in the folders
/// whose files load as skills, commands and agents, the folders
/// themselves included.
///
/// The loaders follow links, and a link's target can change without any
/// change to the repository's own files, so a repository with one is
/// never trusted. Links are not followed while looking.
///
/// # Arguments
///
/// * `root` - The repository root.
///
/// # Returns
///
/// The link's path, or `None` when there is none.
pub fn linked_item(root: &Path) -> Option<std::path::PathBuf> {
    for dir in DIGEST_DIRS {
        let dir = root.join(dir);
        let is_link = |path: &Path| {
            std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink())
        };
        let parent_link = dir.parent().filter(|parent| is_link(parent));
        if let Some(parent) = parent_link {
            return Some(parent.to_path_buf());
        }
        if is_link(&dir) {
            return Some(dir);
        }
        if !dir.is_dir() {
            continue;
        }
        let walk = ignore::WalkBuilder::new(&dir)
            .standard_filters(false)
            .follow_links(false)
            .build();
        if let Some(entry) = walk.flatten().find(ignore::DirEntry::path_is_symlink) {
            return Some(entry.into_path());
        }
    }
    None
}

/// The reason a repository with a link in `link` is not trusted.
fn link_reason(link: &Path) -> String {
    format!(
        "{} is a symbolic link; anvil does not trust a repository that links a skill, command or agent, because the target can change without the repository changing; replace the link with a copy",
        link.display()
    )
}

/// The command lines hooks and MCP servers run: each hook command, and
/// each server's program followed by its arguments, one per line.
///
/// # Arguments
///
/// * `hooks` - The loaded hooks.
/// * `servers` - The loaded MCP servers.
///
/// # Returns
///
/// The lines, for [`anvil_core::tools::workspace::command_files`].
pub fn commands_run(hooks: &HookConfig, servers: &ServerSet) -> Vec<String> {
    let mut lines: Vec<String> = hooks
        .events()
        .into_iter()
        .flat_map(|event| hooks.groups(event).iter())
        .flat_map(|group| group.commands.iter().map(|c| c.command.clone()))
        .collect();
    for server in servers.servers() {
        if let Transport::Stdio { command, args, .. } = &server.transport {
            lines.push(command.clone());
            lines.extend(args.iter().cloned());
        }
    }
    lines
}

/// The SHA-256 digest, in hex, of everything a trust decision covers.
///
/// It covers the content of [`DIGEST_FILES`], every file under
/// [`DIGEST_DIRS`], every file in the folder (recursively, skipping
/// ignored files and `.git`) of each file inside the repository that one
/// of its hooks or MCP servers names ([`commands_run`]), every `CLAUDE.md` in a
/// subfolder (honoring `.gitignore`), and the instruction text as loaded,
/// imports included.
/// Each file enters with its path, so adding, removing or renaming one
/// changes the digest as much as editing it.
///
/// # Arguments
///
/// * `root` - The repository root.
///
/// # Returns
///
/// 64 hex characters.
pub fn fingerprint(root: &Path) -> String {
    let mut files: BTreeSet<std::path::PathBuf> = DIGEST_FILES
        .iter()
        .map(|relative| root.join(relative))
        .filter(|path| path.is_file())
        .collect();
    for dir in DIGEST_DIRS {
        let dir = root.join(dir);
        if dir.is_dir() {
            let walk = ignore::WalkBuilder::new(&dir)
                .standard_filters(false)
                .follow_links(false)
                .build();
            files.extend(
                walk.flatten()
                    .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
                    .map(ignore::DirEntry::into_path),
            );
        }
    }
    let lines = commands_run(
        &HookConfig::load(Some(root), None),
        &ServerSet::load(Some(root), None),
    );
    let canonical_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let script_dirs: BTreeSet<std::path::PathBuf> =
        command_files(root, lines.iter().map(String::as_str))
            .into_iter()
            .filter_map(|path| path.parent().map(Path::to_path_buf))
            .map(|dir| match dir.strip_prefix(&canonical_root) {
                Ok(relative) => root.join(relative),
                Err(_) => dir,
            })
            .collect();
    // A script loads its neighbours, so its whole folder is what runs.
    // Ignored files (build output) are skipped and `.git` is never read.
    for dir in &script_dirs {
        let walk = ignore::WalkBuilder::new(dir)
            .standard_filters(false)
            .git_ignore(true)
            .follow_links(false)
            .filter_entry(|entry| entry.file_name() != ".git")
            .build();
        files.extend(
            walk.flatten()
                .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
                .map(ignore::DirEntry::into_path),
        );
    }
    let nested = ignore::WalkBuilder::new(root).follow_links(false).build();
    files.extend(
        nested
            .flatten()
            .filter(|entry| entry.file_name().eq_ignore_ascii_case("CLAUDE.md"))
            .map(ignore::DirEntry::into_path),
    );
    let mut hasher = Sha256::new();
    hasher.update(b"anvil trust digest v1\0");
    for path in &files {
        let relative = path.strip_prefix(root).unwrap_or(path);
        hasher.update(relative.to_string_lossy().replace('\\', "/").as_bytes());
        hasher.update([0]);
        let content = std::fs::read(path).unwrap_or_default();
        hasher.update((content.len() as u64).to_le_bytes());
        hasher.update(&content);
    }
    let base = discover_instructions(None, Some(root), &BTreeSet::new());
    let extra = anvil_compat::instructions::discover_extra(None, Some(root), &base);
    for instruction in base.iter().chain(&extra) {
        hasher.update(b"instruction\0");
        hasher.update(instruction.source.as_bytes());
        hasher.update([0]);
        hasher.update((instruction.text.len() as u64).to_le_bytes());
        hasher.update(instruction.text.as_bytes());
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Decides whether what the repository ships takes effect in this run.
///
/// A repository with a [`linked_item`] is never trusted.
///
/// A stored refusal is used as it is, and so is trust recorded for exactly
/// the content the repository ships now. Otherwise, when `ask` is true and
/// the repository ships something, the user is asked and the answer is
/// stored with the digest. When `ask` is false (plain output, `--yes`, no
/// terminal, or a command that never prompts) an undecided or changed
/// repository stays untrusted, and one line on stderr and a warning in the
/// log say so. When `allow_trust` is false (a managed policy forbids
/// trusting repositories) nothing is asked or recorded and the repository
/// is untrusted.
///
/// # Arguments
///
/// * `root` - The repository root.
/// * `home` - The home directory, where decisions are kept.
/// * `ask` - Whether a person is at the terminal to answer.
/// * `allow_trust` - Whether a repository may be trusted at all.
///
/// # Returns
///
/// Whether the repository is trusted.
///
/// # Errors
///
/// When `~/.anvil/trust.toml` cannot be read or parsed, or the answer
/// cannot be saved.
pub fn decide(
    root: &Path,
    home: Option<&Path>,
    ask: bool,
    allow_trust: bool,
) -> anyhow::Result<bool> {
    let Some(home) = home else {
        return Ok(false);
    };
    if !allow_trust {
        if !shipped(root).is_empty() {
            tracing::warn!(root = %root.display(), "repository not trusted: the managed policy forbids trusting repositories");
            eprintln!(
                "note: the managed policy does not allow trusting repositories; nothing this repository ships takes effect"
            );
        }
        return Ok(false);
    }
    if let Some(link) = linked_item(root) {
        let reason = link_reason(&link);
        tracing::warn!(root = %root.display(), "repository not trusted: {reason}");
        eprintln!("note: nothing this repository ships takes effect: {reason}");
        return Ok(false);
    }
    let mut store = TrustStore::load(home)?;
    let digest = fingerprint(root);
    match store.decision(root, &digest) {
        Some(TrustDecision::Trusted) => return Ok(true),
        Some(TrustDecision::Untrusted) => return Ok(false),
        None => {}
    }
    let changed = store.changed_since_trusted(root, &digest);
    let items = shipped(root);
    if items.is_empty() && !changed {
        return Ok(false);
    }
    if !ask {
        let why = if changed {
            "changed since you trusted it"
        } else {
            "is not trusted"
        };
        tracing::warn!(root = %root.display(), changed, items = items.len(), "repository settings ignored: {why}");
        eprintln!(
            "note: this repository {why}; its {} item(s) (hooks, MCP servers, allow rules, skills, commands, agents, instructions) do not take effect; run `anvil trust` to review them",
            items.len()
        );
        return Ok(false);
    }
    if changed {
        eprintln!(
            "{} changed since you trusted it, so anvil asks again.",
            root.display()
        );
    }
    let trusted = ask_on_terminal(root, &items)?;
    record(&mut store, home, root, trusted, &digest)?;
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
/// * `allow_trust` - Whether a repository may be trusted at all; when not,
///   only `revoke` works.
///
/// # Returns
///
/// The process exit code: zero.
///
/// # Errors
///
/// When the home directory is unknown, the trust file cannot be read or
/// written, or trust is asked for while `allow_trust` is false.
pub fn run(
    root: &Path,
    home: Option<&Path>,
    revoke: bool,
    allow_trust: bool,
) -> anyhow::Result<i32> {
    let home = home.context("cannot determine the home directory to keep the decision in")?;
    let mut store = TrustStore::load(home)?;
    let digest = fingerprint(root);
    if revoke {
        record(&mut store, home, root, false, &digest)?;
        return Ok(0);
    }
    anyhow::ensure!(
        allow_trust,
        "the managed policy does not allow trusting repositories"
    );
    if let Some(link) = linked_item(root) {
        anyhow::bail!("{}", link_reason(&link));
    }
    let items = shipped(root);
    if items.is_empty() {
        println!(
            "{} ships no hooks, MCP servers, allow rules, skills, commands, agents or instruction files.",
            root.display()
        );
    }
    let trusted = ask_on_terminal(root, &items)?;
    record(&mut store, home, root, trusted, &digest)?;
    Ok(0)
}

/// Stores the decision and says where.
fn record(
    store: &mut TrustStore,
    home: &Path,
    root: &Path,
    trusted: bool,
    digest: &str,
) -> anyhow::Result<()> {
    let decision = if trusted {
        TrustDecision::Trusted
    } else {
        TrustDecision::Untrusted
    };
    store.set(root, decision, digest);
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
        "{} ships files that run programs, allow tool calls or steer the model:",
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
    fn mcp_servers_are_listed_as_written_with_their_environment() {
        let root = tempfile::tempdir().expect("root");
        write(
            root.path(),
            ".mcp.json",
            r#"{"mcpServers": {"x": {"command": "node", "args": ["srv.js", "--k=${PATH}"], "env": {"STOLEN": "${PATH}", "MODE": "fast"}}}}"#,
        );
        let items = shipped(root.path()).join(
            "
",
        );
        assert!(
            items.contains("starts `node srv.js --k=${PATH}` with MODE=fast STOLEN=${PATH}"),
            "listing:
{items}"
        );
        let path = std::env::var("PATH").unwrap_or_default();
        assert!(
            path.is_empty() || !items.contains(&path),
            "the listing expanded a variable:
{items}"
        );
    }

    #[test]
    fn without_a_terminal_an_undecided_repository_is_not_trusted_or_recorded() {
        let root = tempfile::tempdir().expect("root");
        let home = tempfile::tempdir().expect("home");
        write(root.path(), "CLAUDE.md", "rules");
        assert!(!decide(root.path(), Some(home.path()), false, true).expect("decides"));
        let store = TrustStore::load(home.path()).expect("loads");
        assert_eq!(
            store.decision(root.path(), &fingerprint(root.path())),
            None,
            "nothing recorded"
        );
    }

    #[test]
    fn when_trusting_is_forbidden_a_trusted_repository_is_not_trusted() {
        let root = tempfile::tempdir().expect("root");
        let home = tempfile::tempdir().expect("home");
        write(root.path(), "CLAUDE.md", "rules");
        let mut store = TrustStore::default();
        store.set(
            root.path(),
            TrustDecision::Trusted,
            &fingerprint(root.path()),
        );
        store.save(home.path()).expect("saves");
        assert!(!decide(root.path(), Some(home.path()), true, false).expect("decides"));
        assert!(run(root.path(), Some(home.path()), false, false).is_err());
        let other = tempfile::tempdir().expect("root");
        write(other.path(), "CLAUDE.md", "rules");
        assert!(!decide(other.path(), Some(home.path()), true, false).expect("decides"));
        let store = TrustStore::load(home.path()).expect("loads");
        assert_eq!(
            store.decision(other.path(), &fingerprint(other.path())),
            None,
            "nothing asked or recorded"
        );
    }

    #[test]
    fn any_change_to_what_the_repository_ships_asks_again() {
        let changes: &[(&str, &str)] = &[
            (".anvil/permissions.toml", "allow = [\"shell\"]\n"),
            (".claude/settings.json", "{}"),
            (".anvil/settings.json", "{}"),
            (".mcp.json", "{}"),
            ("CLAUDE.md", "other rules"),
            ("sub/CLAUDE.md", "nested"),
            (".claude/skills/s/run.sh", "echo"),
            (".anvil/commands/go.md", "go"),
            (".claude/agents/a.md", "a"),
            ("docs/imported.md", "imported"),
        ];
        for (file, text) in changes {
            let root = tempfile::tempdir().expect("root");
            let home = tempfile::tempdir().expect("home");
            write(root.path(), "CLAUDE.md", "rules\n@docs/imported.md\n");
            write(root.path(), "docs/imported.md", "first");
            let mut store = TrustStore::default();
            store.set(
                root.path(),
                TrustDecision::Trusted,
                &fingerprint(root.path()),
            );
            store.save(home.path()).expect("saves");
            assert!(decide(root.path(), Some(home.path()), false, true).expect("decides"));
            write(root.path(), file, text);
            assert!(
                !decide(root.path(), Some(home.path()), false, true).expect("decides"),
                "a change to {file} still trusted"
            );
        }
    }

    /// Links the repository's agent `a.md` to `docs/agents/a.md`: the file
    /// itself on Unix, the `.claude/agents` folder as a junction on
    /// Windows, where a file link needs a privilege a junction does not.
    /// `false` when the link cannot be made.
    fn link_agent(root: &Path) -> bool {
        let target = root.join("docs").join("agents");
        let agents = root.join(".claude").join("agents");
        std::fs::create_dir_all(&agents).expect("mkdir");
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(target.join("a.md"), agents.join("a.md")).is_ok();
        #[cfg(windows)]
        let made = std::fs::remove_dir(&agents).is_ok()
            && std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(&agents)
                .arg(&target)
                .output()
                .is_ok_and(|out| out.status.success());
        made
    }

    #[test]
    fn a_repository_that_links_an_agent_is_never_trusted() {
        let root = tempfile::tempdir().expect("root");
        let home = tempfile::tempdir().expect("home");
        let agent = "docs/agents/a.md";
        write(
            root.path(),
            agent,
            "---
name: a
description: d
---
body
",
        );
        assert!(link_agent(root.path()), "the link could not be made");
        assert!(
            AgentSet::discover(Some(root.path()), None)
                .get("a")
                .is_some(),
            "the loader follows the link"
        );
        let mut store = TrustStore::default();
        store.set(
            root.path(),
            TrustDecision::Trusted,
            &fingerprint(root.path()),
        );
        store.save(home.path()).expect("saves");
        assert!(
            !decide(root.path(), Some(home.path()), false, true).expect("decides"),
            "trust recorded over a linked agent counts"
        );
        write(
            root.path(),
            agent,
            "---
name: a
description: d
---
other
",
        );
        assert!(
            !decide(root.path(), Some(home.path()), false, true).expect("decides"),
            "the link's target changed and the repository is still trusted"
        );
        let error = run(root.path(), Some(home.path()), false, true)
            .expect_err("anvil trust refuses a linked agent");
        assert!(error.to_string().contains("symbolic link"), "{error}");
    }

    #[test]
    fn an_unrelated_change_keeps_trust() {
        let root = tempfile::tempdir().expect("root");
        let home = tempfile::tempdir().expect("home");
        write(root.path(), "CLAUDE.md", "rules");
        let mut store = TrustStore::default();
        store.set(
            root.path(),
            TrustDecision::Trusted,
            &fingerprint(root.path()),
        );
        store.save(home.path()).expect("saves");
        write(root.path(), "src/main.rs", "fn main() {}");
        assert!(decide(root.path(), Some(home.path()), false, true).expect("decides"));
    }

    #[test]
    fn a_stored_decision_is_used_without_asking() {
        let root = tempfile::tempdir().expect("root");
        let home = tempfile::tempdir().expect("home");
        write(root.path(), "CLAUDE.md", "rules");
        let digest = fingerprint(root.path());
        let mut store = TrustStore::default();
        store.set(root.path(), TrustDecision::Trusted, &digest);
        store.save(home.path()).expect("saves");
        assert!(decide(root.path(), Some(home.path()), false, true).expect("decides"));
        store.set(root.path(), TrustDecision::Untrusted, &digest);
        store.save(home.path()).expect("saves");
        assert!(!decide(root.path(), Some(home.path()), true, true).expect("decides"));
    }
}
