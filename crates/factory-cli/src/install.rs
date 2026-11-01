//! `factory install-service`: the systemd user unit from `deploy/nuc/`.

use std::path::{Path, PathBuf};

use anyhow::Context;

/// The unit file in the repository.
pub const UNIT_SOURCE: &str = "deploy/nuc/factory.service";

/// The unit name.
pub const UNIT_NAME: &str = "factory.service";

/// What the install needs.
pub struct InstallOptions {
    /// The repository root, which holds [`UNIT_SOURCE`].
    pub repo_root: PathBuf,
    /// `$XDG_CONFIG_HOME`, normally `~/.config`.
    pub config_home: PathBuf,
    /// Whether to run `systemctl --user`; tests copy the file only.
    pub run_systemctl: bool,
}

/// Copies the unit into the user's systemd folder and enables it.
///
/// # Arguments
///
/// * `options` - Where the unit comes from and goes to.
///
/// # Returns
///
/// The lines to print: where the unit went and what to run next.
///
/// # Errors
///
/// When the unit is missing from the repository, cannot be copied, or
/// `systemctl` fails.
pub fn install(options: &InstallOptions) -> anyhow::Result<Vec<String>> {
    let source = options.repo_root.join(UNIT_SOURCE);
    let text = std::fs::read_to_string(&source)
        .with_context(|| format!("reading {}; run from the repository root", source.display()))?;
    let target_dir = options.config_home.join("systemd").join("user");
    std::fs::create_dir_all(&target_dir)
        .with_context(|| format!("creating {}", target_dir.display()))?;
    let target = target_dir.join(UNIT_NAME);
    std::fs::write(&target, text).with_context(|| format!("writing {}", target.display()))?;
    let mut lines = vec![format!("wrote {}", target.display())];
    if options.run_systemctl {
        systemctl(&["--user", "daemon-reload"])?;
        systemctl(&["--user", "enable", "--now", UNIT_NAME])?;
        lines.push("enabled and started factory.service".to_owned());
    }
    lines.push(
        "run `loginctl enable-linger $USER` once so the service starts without a login session"
            .to_owned(),
    );
    lines.push("follow the log with `journalctl --user -u factory -f`".to_owned());
    Ok(lines)
}

/// Runs one `systemctl` command.
fn systemctl(args: &[&str]) -> anyhow::Result<()> {
    let out = std::process::Command::new("systemctl")
        .args(args)
        .output()
        .context("running systemctl")?;
    if !out.status.success() {
        anyhow::bail!(
            "systemctl {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// `$XDG_CONFIG_HOME`, else `<home>/.config`.
///
/// # Arguments
///
/// * `home` - The home directory.
pub fn config_home(home: &Path) -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_the_unit_from_the_repository() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let dir = tempfile::tempdir().expect("tempdir");
        let lines = install(&InstallOptions {
            repo_root: repo.clone(),
            config_home: dir.path().to_path_buf(),
            run_systemctl: false,
        })
        .expect("installs");
        let target = dir.path().join("systemd/user/factory.service");
        assert!(target.is_file());
        let text = std::fs::read_to_string(&target).expect("read");
        assert_eq!(
            text,
            std::fs::read_to_string(repo.join(UNIT_SOURCE)).expect("source")
        );
        assert!(text.contains("ExecStart=%h/.local/bin/factory run"));
        assert!(text.contains("WorkingDirectory=%h/factory-repo"));
        assert!(
            !text.contains("EnvironmentFile"),
            "secrets never come from the unit"
        );
        assert!(lines[0].starts_with("wrote "));
        assert!(lines.iter().any(|l| l.contains("enable-linger")));
    }

    #[test]
    fn a_missing_unit_names_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let error = install(&InstallOptions {
            repo_root: dir.path().to_path_buf(),
            config_home: dir.path().to_path_buf(),
            run_systemctl: false,
        })
        .expect_err("missing");
        assert!(error.to_string().contains(UNIT_SOURCE), "{error}");
    }
}
