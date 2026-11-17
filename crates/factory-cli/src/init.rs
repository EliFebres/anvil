//! `factory init`: writes `factory.toml` and `deploy/nuc/env.template` at the
//! repository root, never overwriting a file that exists.
//!
//! `anvil setup` writes anvil's own files (`~/.anvil/.env` and
//! `~/.anvil/config.toml`); the factory's reviewers name providers from that
//! configuration, so a factory host runs both.

use std::path::Path;
use std::process::Command;

use anyhow::Context;
use factory_types::FactoryConfig;
use factory_types::factory_config::{FACTORY_FILE, REPO_PLACEHOLDER};

/// Exit code while placeholders remain unfilled.
const UNFILLED: i32 = 2;

/// What the NUC copies to `~/.factory/env`: the provider keys anvil needs,
/// then the GitHub tokens and the webhook only the factory reads.
/// Placeholders are quoted because a bare value with spaces is not valid
/// `.env` syntax.
pub const ENV_TEMPLATE: &str = r#"# anvil providers
ANTHROPIC_API_KEY="<console.anthropic.com -> API keys>"
OPENAI_API_KEY="<platform.openai.com -> API keys>"
GEMINI_API_KEY="<aistudio.google.com -> Get API key>"

# GitHub, your personal fine-grained token, scoped to the monorepo only:
# Contents read, Pull requests read and write
GITHUB_TOKEN="<github.com -> Settings -> Developer settings -> Fine-grained tokens>"

# Optional: Discord, Slack or ntfy webhook for factory notifications
# WEBHOOK_URL=

# Prompt 7, leave blank for now
# RUNPOD_API_KEY=
# HF_TOKEN=

# The factory's own token, separate from GITHUB_TOKEN so it can be revoked alone.
FACTORY_GITHUB_TOKEN="<fine-grained token for the factory, scoped to the monorepo only: Contents read and write, Pull requests read and write, Workflows read>"
"#;

/// Where the env template goes inside the repository.
pub const NUC_ENV_TEMPLATE: &str = "deploy/nuc/env.template";

/// Writes whichever of the two files is missing and reports what is unfilled.
///
/// # Arguments
///
/// * `root` - The repository root.
///
/// # Returns
///
/// Zero when `factory.toml` has no placeholder left, two otherwise.
///
/// # Errors
///
/// When a file cannot be written or `factory.toml` does not parse.
pub fn run(root: &Path) -> anyhow::Result<i32> {
    let repo = github_repo(root);
    let factory_path = root.join(FACTORY_FILE);
    let written = write_if_absent(
        &factory_path,
        &FactoryConfig::template(repo.as_deref().unwrap_or(REPO_PLACEHOLDER)),
    )?;
    println!("{}: {}", factory_path.display(), describe(written));

    let template_path = root.join(NUC_ENV_TEMPLATE);
    let written = write_if_absent(&template_path, ENV_TEMPLATE)?;
    println!("{}: {}", template_path.display(), describe(written));

    let text = std::fs::read_to_string(&factory_path)
        .with_context(|| format!("reading {}", factory_path.display()))?;
    let factory: FactoryConfig =
        toml::from_str(&text).with_context(|| format!("parsing {}", factory_path.display()))?;
    let unfilled = factory.unfilled();
    if unfilled.is_empty() {
        println!("every placeholder is filled; run `factory doctor`");
        return Ok(0);
    }
    println!("still unfilled:");
    for field in unfilled {
        println!("  {field} in {}", factory_path.display());
    }
    println!("fill them in, then run `factory doctor`");
    Ok(UNFILLED)
}

/// `written` or `kept`, for the line printed per file.
fn describe(written: bool) -> &'static str {
    if written { "written" } else { "kept" }
}

/// Writes `text` to `path` unless the file exists; true when it wrote.
fn write_if_absent(path: &Path, text: &str) -> anyhow::Result<bool> {
    if path.exists() {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(true)
}

/// The `owner/repo` of the `origin` remote, when it is on GitHub.
fn github_repo(root: &Path) -> Option<String> {
    let out = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_github_remote(String::from_utf8_lossy(&out.stdout).trim())
}

/// Extracts `owner/repo` from an https, ssh or scp-style GitHub URL.
fn parse_github_remote(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))
        .or_else(|| url.strip_prefix("git@github.com:"))?;
    let rest = rest.trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let mut parts = rest.split('/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(owner), Some(repo), None) if !owner.is_empty() && !repo.is_empty() => {
            Some(format!("{owner}/{repo}"))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_remotes_parse() {
        for url in [
            "https://github.com/EliFebres/anvil.git",
            "https://github.com/EliFebres/anvil",
            "git@github.com:EliFebres/anvil.git",
            "ssh://git@github.com/EliFebres/anvil.git",
        ] {
            assert_eq!(
                parse_github_remote(url).as_deref(),
                Some("EliFebres/anvil"),
                "{url}"
            );
        }
        assert_eq!(parse_github_remote("https://gitlab.com/a/b.git"), None);
        assert_eq!(parse_github_remote("https://github.com/only"), None);
    }

    #[test]
    fn init_writes_both_files_once_and_reports_the_repo_placeholder() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(run(dir.path()).expect("runs"), UNFILLED);
        let factory = std::fs::read_to_string(dir.path().join(FACTORY_FILE)).expect("written");
        assert!(factory.contains(REPO_PLACEHOLDER), "{factory}");
        let env = std::fs::read_to_string(dir.path().join(NUC_ENV_TEMPLATE)).expect("written");
        assert!(env.contains("FACTORY_GITHUB_TOKEN="), "{env}");
        assert!(env.contains("ANTHROPIC_API_KEY="), "{env}");

        std::fs::write(
            dir.path().join(FACTORY_FILE),
            factory.replace(REPO_PLACEHOLDER, "owner/repo"),
        )
        .expect("fill in");
        assert_eq!(run(dir.path()).expect("runs again"), 0);
        assert!(
            std::fs::read_to_string(dir.path().join(FACTORY_FILE))
                .expect("kept")
                .contains("owner/repo")
        );
    }
}
