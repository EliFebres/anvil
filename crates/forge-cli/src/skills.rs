//! `forge skill`: add, list, show and remove skills.

use std::path::Path;

use anyhow::Context;
use forge_compat::skills::install::{self, Scope, Source};
use forge_compat::{Origin, Skill, SkillSet};

use crate::cli::SkillCommand;
use crate::wiring::skill_roots;

/// Runs one `forge skill` subcommand.
///
/// # Arguments
///
/// * `command` - The parsed subcommand.
/// * `home` - The home directory, if known.
/// * `cwd` - The project root.
///
/// # Returns
///
/// The process exit code: zero on success.
///
/// # Errors
///
/// When the skill cannot be fetched, parsed, copied or found.
pub async fn run(command: SkillCommand, home: Option<&Path>, cwd: &Path) -> anyhow::Result<i32> {
    match command {
        SkillCommand::Add {
            source,
            project,
            force,
        } => {
            let source = Source::parse(&source, cwd)?;
            let scope = scope(project);
            let installed = install::install(&source, scope, home, cwd, force)
                .await
                .with_context(|| format!("installing {source}"))?;
            let verb = if installed.replaced {
                "replaced"
            } else {
                "installed"
            };
            println!(
                "{verb} skill `{}` at {}",
                installed.skill.name(),
                installed.skill.dir.display()
            );
            print_summary(&installed.skill);
            Ok(0)
        }
        SkillCommand::List => {
            let set = SkillSet::discover(&skill_roots(cwd, home));
            if set.is_empty() {
                println!("no skills found; `forge skill add <path | git URL | owner/repo>`");
            }
            for skill in set.skills() {
                println!(
                    "{:<24} {:<18} {}",
                    skill.name(),
                    source_label(skill),
                    one_line(&skill.manifest.description, 80)
                );
            }
            for (name, path) in set.shadowed() {
                println!("(shadowed) {name:<13} {}", path.display());
            }
            for error in set.errors() {
                println!("(error) {}", error.error);
            }
            Ok(0)
        }
        SkillCommand::Remove { name, project } => {
            let dir = install::remove(&name, scope(project), home, cwd)?;
            println!("removed {}", dir.display());
            Ok(0)
        }
        SkillCommand::Show { name } => {
            let set = SkillSet::discover(&skill_roots(cwd, home));
            let skill = set.get(&name).with_context(|| {
                format!("no skill named `{name}`; `forge skill list` shows them")
            })?;
            println!("name: {}", skill.name());
            println!("source: {} ({})", source_label(skill), skill.dir.display());
            print_summary(skill);
            println!();
            print!("{}", skill.body);
            Ok(0)
        }
    }
}

/// The scope for `--project`.
fn scope(project: bool) -> Scope {
    if project {
        Scope::Project
    } else {
        Scope::Global
    }
}

/// The directory a skill came from, as `forge skill list` shows it.
pub fn source_label(skill: &Skill) -> String {
    match skill.source {
        Origin::Bundled => "bundled skills/".to_owned(),
        origin => format!("{origin}/skills"),
    }
}

/// Prints the description, tool lists and supporting files.
fn print_summary(skill: &Skill) {
    println!(
        "description: {}",
        one_line(&skill.manifest.description, 200)
    );
    if !skill.manifest.allowed_tools.is_empty() {
        println!("allowed-tools: {}", skill.manifest.allowed_tools.join(", "));
    }
    if !skill.manifest.requires.is_empty() {
        println!("requires: {}", skill.manifest.requires.join(", "));
    }
    if !skill.files.is_empty() {
        println!("files: {}", skill.files.join(", "));
    }
}

/// Collapses whitespace and truncates to `max` characters.
fn one_line(text: &str, max: usize) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if joined.chars().count() <= max {
        return joined;
    }
    let cut: String = joined.chars().take(max.saturating_sub(3)).collect();
    format!("{cut}...")
}
