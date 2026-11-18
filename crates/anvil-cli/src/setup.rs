//! `anvil setup`: writes every file a key or setting lives in, never
//! overwriting a value that is already filled in.
//!
//! Files: `~/.anvil/.env` (keys) and `~/.anvil/config.toml` (providers and
//! roles).

use std::path::Path;
#[cfg(not(unix))]
use std::process::Command;

use anvil_types::AnvilConfig;
use anvil_types::config::{self, CONFIG_DIR, OPTIONAL_SECTIONS, SETUP_PROVIDERS};
use anvil_types::dotenv::{ANVIL_ENV_FILE, is_placeholder, read_values};
use anyhow::Context;
use toml_edit::{DocumentMut, Item, Table};

/// Exit code while placeholders remain unfilled.
const UNFILLED: i32 = 2;

/// The `~/.anvil/.env` template. Placeholders are quoted because a bare
/// value with spaces is not valid `.env` syntax.
pub const ENV_TEMPLATE: &str = r#"# anvil providers
ANTHROPIC_API_KEY="<console.anthropic.com -> API keys>"
OPENAI_API_KEY="<platform.openai.com -> API keys>"
GEMINI_API_KEY="<aistudio.google.com -> Get API key>"

# Prompt 7, leave blank for now
# RUNPOD_API_KEY=
# HF_TOKEN=
"#;

/// What happened to one file.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    /// The file did not exist and was written.
    Written,
    /// The file existed and these entries were added to it.
    Extended(Vec<String>),
    /// The file existed and already had everything.
    Kept,
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Written => write!(f, "written"),
            Self::Extended(added) => write!(f, "kept, added {}", added.join(", ")),
            Self::Kept => write!(f, "kept"),
        }
    }
}

/// Writes or extends every setup file and reports what is still unfilled.
///
/// # Arguments
///
/// * `home` - The home directory.
///
/// # Returns
///
/// Zero when every placeholder is filled, two otherwise.
///
/// # Errors
///
/// When the home directory is unknown, a file cannot be written, or an
/// existing file is malformed.
pub fn run(home: Option<&Path>) -> anyhow::Result<i32> {
    let home = home.context("cannot determine the home directory")?;
    let anvil_dir = home.join(CONFIG_DIR);
    std::fs::create_dir_all(&anvil_dir)
        .with_context(|| format!("creating {}", anvil_dir.display()))?;

    let env_path = anvil_dir.join(ANVIL_ENV_FILE);
    let outcome = write_env(&env_path, ENV_TEMPLATE)?;
    println!("{}: {outcome}", env_path.display());
    for warning in make_private(&env_path)? {
        println!("warning: {warning}");
    }

    let config_path = anvil_dir.join(config::CONFIG_FILE);
    let outcome = write_config(&config_path)?;
    println!("{}: {outcome}", config_path.display());

    print_models();

    let unfilled = unfilled(&env_path)?;
    if unfilled.is_empty() {
        println!("every placeholder is filled; run `anvil doctor`");
        return Ok(0);
    }
    println!("still unfilled:");
    for item in &unfilled {
        println!("  {item}");
    }
    println!("fill them in, then run `anvil doctor`");
    Ok(UNFILLED)
}

/// Writes the template, or appends the variables an existing file lacks.
///
/// A variable counts as present when any line, commented or not, starts
/// with its name and `=`. Blocks are separated by blank lines; a block's
/// comment lines are appended with its first missing variable.
fn write_env(path: &Path, template: &str) -> anyhow::Result<Outcome> {
    if !path.exists() {
        std::fs::write(path, template).with_context(|| format!("writing {}", path.display()))?;
        return Ok(Outcome::Written);
    }
    let existing =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let present: Vec<String> = existing.lines().filter_map(variable_name).collect();
    let mut additions = String::new();
    let mut added = Vec::new();
    for block in template.split("\n\n") {
        let mut lines = Vec::new();
        for line in block.lines() {
            match variable_name(line) {
                Some(name) if present.contains(&name) => {}
                Some(name) => {
                    added.push(name);
                    lines.push(line);
                }
                None => lines.push(line),
            }
        }
        if lines.iter().any(|line| variable_name(line).is_some()) {
            additions.push('\n');
            additions.push_str(&lines.join("\n"));
            additions.push('\n');
        }
    }
    if added.is_empty() {
        return Ok(Outcome::Kept);
    }
    let separator = if existing.ends_with('\n') || existing.is_empty() {
        ""
    } else {
        "\n"
    };
    std::fs::write(path, format!("{existing}{separator}{additions}"))
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(Outcome::Extended(added))
}

/// The variable a `.env` line defines, commented out or not.
fn variable_name(line: &str) -> Option<String> {
    let line = line.trim_start();
    let line = line.strip_prefix('#').map_or(line, str::trim_start);
    let (name, _) = line.split_once('=')?;
    let name = name.trim();
    let valid = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    valid.then(|| name.to_owned())
}

/// Writes the setup config, or adds the providers and roles an existing one lacks.
///
/// Existing entries, comments and ordering are untouched; only missing
/// `[providers.<name>]` tables and `[roles]` keys are appended.
fn write_config(path: &Path) -> anyhow::Result<Outcome> {
    if !path.exists() {
        std::fs::write(path, format!("{SETUP_PROVIDERS}{OPTIONAL_SECTIONS}"))
            .with_context(|| format!("writing {}", path.display()))?;
        return Ok(Outcome::Written);
    }
    let existing =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut doc: DocumentMut = existing
        .parse()
        .with_context(|| format!("parsing {}", path.display()))?;
    let template: DocumentMut = SETUP_PROVIDERS
        .parse()
        .context("parsing the built-in provider template")?;
    let mut added = Vec::new();
    for section in ["providers", "roles"] {
        let Some(wanted) = template.get(section).and_then(Item::as_table) else {
            continue;
        };
        let target = doc
            .entry(section)
            .or_insert_with(|| {
                let mut table = Table::new();
                table.set_implicit(section == "providers");
                Item::Table(table)
            })
            .as_table_mut()
            .with_context(|| format!("[{section}] in {} is not a table", path.display()))?;
        for (key, item) in wanted {
            if !target.contains_key(key) {
                target.insert(key, item.clone());
                added.push(format!("{section}.{key}"));
            }
        }
    }
    if added.is_empty() {
        return Ok(Outcome::Kept);
    }
    let text = doc.to_string();
    let merged: AnvilConfig = toml::from_str(&text)
        .with_context(|| format!("{} would not parse after the merge", path.display()))?;
    merged
        .validate()
        .with_context(|| format!("{} would not validate after the merge", path.display()))?;
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(Outcome::Extended(added))
}

/// Makes a secrets file private, or says how to.
///
/// On Unix the mode is set to 600. On Windows the ACL is inspected with
/// `icacls`; a grant to a group of other users yields a warning with the
/// command that fixes it.
///
/// # Arguments
///
/// * `path` - The file.
///
/// # Returns
///
/// Warnings for the user; empty when the file is private.
///
/// # Errors
///
/// When the Unix permissions cannot be set.
pub fn make_private(path: &Path) -> anyhow::Result<Vec<String>> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("setting mode 600 on {}", path.display()))?;
        Ok(Vec::new())
    }
    #[cfg(not(unix))]
    {
        let Ok(out) = Command::new("icacls").arg(path).output() else {
            return Ok(Vec::new());
        };
        let listing = String::from_utf8_lossy(&out.stdout);
        Ok(shared_grants(&listing)
            .into_iter()
            .map(|group| {
                let user = std::env::var("USERNAME").unwrap_or_else(|_| "<you>".to_owned());
                format!(
                    "{} is readable by {group}; run: icacls \"{}\" /inheritance:r /grant:r \"{user}:(F)\"",
                    path.display(),
                    path.display()
                )
            })
            .collect())
    }
}

/// The groups of other users that an `icacls` listing grants access to.
#[cfg_attr(unix, allow(dead_code))]
fn shared_grants(listing: &str) -> Vec<&'static str> {
    const GROUPS: &[&str] = &["BUILTIN\\Users", "Everyone", "Authenticated Users"];
    const ACCESS: &[&str] = &["(R", "(M", "(F", "(GR"];
    GROUPS
        .iter()
        .copied()
        .filter(|group| {
            listing.lines().any(|line| {
                line.contains(group) && ACCESS.iter().any(|access| line.contains(access))
            })
        })
        .collect()
}

/// Prints the models and prices the template uses, so they can be changed.
fn print_models() {
    let Ok(config) = toml::from_str::<AnvilConfig>(SETUP_PROVIDERS) else {
        return;
    };
    println!("models (edit ~/.anvil/config.toml to change):");
    for (name, provider) in &config.providers {
        let price = provider
            .price_per_mtoken
            .map(|p| format!("${:.2} in / ${:.2} out per MTok", p.input, p.output))
            .unwrap_or_else(|| "price unknown".to_owned());
        println!(
            "  {name}: {} ({}, {price})",
            provider.model, provider.base_url
        );
    }
    println!(
        "  roles: coder={} reviewer={} summarizer={}{}",
        config.roles.coder,
        config.roles.reviewer.as_deref().unwrap_or("(coder)"),
        config.roles.summarizer,
        config
            .roles
            .planner
            .as_deref()
            .map(|planner| format!(" planner={planner}"))
            .unwrap_or_default()
    );
}

/// The placeholders still present in the env file.
fn unfilled(env_path: &Path) -> anyhow::Result<Vec<String>> {
    Ok(read_values(env_path)?
        .into_iter()
        .filter(|(_, value)| is_placeholder(value))
        .map(|(name, _)| format!("{name} in {}", env_path.display()))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anvil_types::config::CONFIG_TEMPLATE;

    #[test]
    fn env_template_parses_and_only_has_placeholders() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(".env");
        assert_eq!(
            write_env(&path, ENV_TEMPLATE).expect("writes"),
            Outcome::Written
        );
        let values = read_values(&path).expect("parses");
        let names: Vec<&str> = values.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            ["ANTHROPIC_API_KEY", "OPENAI_API_KEY", "GEMINI_API_KEY"]
        );
        assert!(values.iter().all(|(_, v)| is_placeholder(v)));
        assert_eq!(
            write_env(&path, ENV_TEMPLATE).expect("again"),
            Outcome::Kept
        );
    }

    #[test]
    fn env_merge_appends_missing_blocks_and_keeps_filled_lines() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(".env");
        std::fs::write(&path, "OPENAI_API_KEY=filled-value-1234\nWEBHOOK_URL=\n").expect("write");
        let outcome = write_env(&path, ENV_TEMPLATE).expect("merges");
        assert_eq!(
            outcome,
            Outcome::Extended(vec![
                "ANTHROPIC_API_KEY".to_owned(),
                "GEMINI_API_KEY".to_owned(),
                "RUNPOD_API_KEY".to_owned(),
                "HF_TOKEN".to_owned(),
            ])
        );
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.starts_with("OPENAI_API_KEY=filled-value-1234\nWEBHOOK_URL=\n"));
        assert!(text.contains("# anvil providers\nANTHROPIC_API_KEY="));
        assert!(text.contains("# Prompt 7, leave blank for now\n# RUNPOD_API_KEY=\n# HF_TOKEN="));
        assert_eq!(text.matches("OPENAI_API_KEY").count(), 1);
        assert_eq!(text.matches("WEBHOOK_URL").count(), 1);
        assert_eq!(
            write_env(&path, ENV_TEMPLATE).expect("again"),
            Outcome::Kept
        );
    }

    #[test]
    fn config_merge_keeps_comments_and_existing_entries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, CONFIG_TEMPLATE).expect("write");
        let outcome = write_config(&path).expect("merges");
        assert_eq!(
            outcome,
            Outcome::Extended(vec![
                "providers.anthropic".to_owned(),
                "providers.openai".to_owned(),
                "providers.google".to_owned(),
                "roles.reviewer".to_owned(),
            ])
        );
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains("# anvil configuration.\n"), "comments kept");
        assert!(text.contains("coder = \"runpod\""), "existing value kept");
        assert!(text.contains("[providers.runpod]"));
        assert!(text.contains("[providers.anthropic]"));
        let config: AnvilConfig = toml::from_str(&text).expect("parses");
        config.validate().expect("validates");
        assert_eq!(config.roles.reviewer.as_deref(), Some("openai"));
        assert_eq!(write_config(&path).expect("again"), Outcome::Kept);
        assert_eq!(std::fs::read_to_string(&path).expect("read"), text);
    }

    #[test]
    fn fresh_config_is_the_setup_template() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        assert_eq!(write_config(&path).expect("writes"), Outcome::Written);
        let config: AnvilConfig =
            toml::from_str(&std::fs::read_to_string(&path).expect("read")).expect("parses");
        assert_eq!(config.providers.len(), 3);
    }

    #[test]
    fn icacls_listings_reveal_shared_grants() {
        let private = "C:\\u\\.env NT AUTHORITY\\SYSTEM:(F)\n  DESKTOP\\eli:(F)\n";
        assert!(shared_grants(private).is_empty());
        let shared = "C:\\u\\.env BUILTIN\\Users:(RX)\n  Everyone:(R)\n  DESKTOP\\eli:(F)\n";
        assert_eq!(shared_grants(shared), vec!["BUILTIN\\Users", "Everyone"]);
    }
}
