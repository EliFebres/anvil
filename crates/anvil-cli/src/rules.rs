//! `anvil rules`: list, add, remove and check standing rules.

use std::path::{Path, PathBuf};
use std::process::Command;

use anvil_memory::{Check, Rule, paths, run_checks};
use anyhow::{Context, bail};

use crate::cli::RulesCommand;
use crate::memory;

/// Runs one `anvil rules` subcommand.
///
/// # Arguments
///
/// * `command` - The parsed subcommand.
/// * `home` - The home directory, if known.
/// * `cwd` - The project root.
///
/// # Returns
///
/// The process exit code: zero on success, one when a check failed.
///
/// # Errors
///
/// When there is no home directory, the rules file is malformed, an id is
/// unknown or taken, or the changed files cannot be listed.
pub fn run(command: RulesCommand, home: Option<&Path>, cwd: &Path) -> anyhow::Result<i32> {
    let home = home.context("cannot determine the home directory")?;
    let path = paths::rules_path(home);
    match command {
        RulesCommand::List => {
            let set = memory::open_rules(home)?;
            if set.is_empty() {
                println!("no rules in {}", path.display());
            }
            for rule in &set.rules {
                println!("{}", describe(rule));
            }
            Ok(0)
        }
        RulesCommand::Add {
            id,
            rule,
            languages,
            regex,
            command,
        } => {
            let mut set = memory::open_rules(home)?;
            let check = match (regex, command) {
                (Some(pattern), _) => Some(Check::Regex(pattern)),
                (None, Some(command)) => Some(Check::Command(command)),
                (None, None) => None,
            };
            set.add(Rule {
                id: id.clone(),
                languages: languages
                    .split(',')
                    .map(|language| language.trim().to_owned())
                    .filter(|language| !language.is_empty())
                    .collect(),
                rule: rule.join(" "),
                check,
            })?;
            set.save(&path)
                .with_context(|| format!("writing {}", path.display()))?;
            println!("added rule `{id}` to {}", path.display());
            Ok(0)
        }
        RulesCommand::Remove { id } => {
            let mut set = memory::open_rules(home)?;
            set.remove(&id)?;
            set.save(&path)
                .with_context(|| format!("writing {}", path.display()))?;
            println!("removed rule `{id}` from {}", path.display());
            Ok(0)
        }
        RulesCommand::Check { paths: files } => {
            let set = memory::open_rules(home)?;
            let files: Vec<PathBuf> = if files.is_empty() {
                git_changed_files(cwd)?
            } else {
                files
                    .into_iter()
                    .map(|file| {
                        if file.is_absolute() {
                            file
                        } else {
                            cwd.join(file)
                        }
                    })
                    .collect()
            };
            if files.is_empty() {
                println!("no changed files to check");
                return Ok(0);
            }
            let failures = run_checks(&set, cwd, Some(home), &files);
            if failures.is_empty() {
                println!("all rule checks passed on {} file(s)", files.len());
                return Ok(0);
            }
            for failure in &failures {
                println!("[{}] check failed:\n{}\n", failure.rule_id, failure.output);
            }
            Ok(1)
        }
    }
}

/// One rule as `anvil rules list` prints it.
fn describe(rule: &Rule) -> String {
    let mut text = format!(
        "{:<22} {:<16} {}",
        rule.id,
        rule.languages.join(","),
        rule.rule
    );
    match &rule.check {
        Some(Check::Regex(pattern)) => {
            text.push_str(&format!("\n{:<22} check: regex `{pattern}`", ""))
        }
        Some(Check::Command(command)) => {
            text.push_str(&format!("\n{:<22} check: command `{command}`", ""));
        }
        None => {}
    }
    text
}

/// Files changed since `HEAD`, plus untracked files, as git sees them.
fn git_changed_files(cwd: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for args in [
        vec!["diff", "--name-only", "--diff-filter=ACMR", "HEAD"],
        vec!["ls-files", "--others", "--exclude-standard"],
    ] {
        let output = Command::new("git")
            .args(["-c", "core.quotepath=false"])
            .args(&args)
            .current_dir(cwd)
            .output()
            .context("running git; pass file paths to check without it")?;
        if !output.status.success() {
            bail!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        files.extend(
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(|line| cwd.join(line)),
        );
    }
    files.sort();
    files.dedup();
    Ok(files)
}
