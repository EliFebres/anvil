//! Applying the managed policy at startup: loading it from the system path,
//! refusing the flags it disables, placing and pruning session folders,
//! and the policy block `anvil doctor` prints.
//!
//! What the policy changes inside a session (endpoints, hooks, MCP servers,
//! skills, herdr, the forced profile) is applied in one place,
//! [`crate::wiring::Wiring::with_policy`].

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anvil_core::context::project_sessions_root;
use anvil_types::policy::{self, LoadedPolicy};

/// Loads the policy from the one system path. A file that exists but
/// cannot be used stops anvil.
///
/// # Returns
///
/// The policy, or `None` in personal mode.
///
/// # Errors
///
/// When the file exists but cannot be read or is invalid.
pub fn load_system() -> anyhow::Result<Option<LoadedPolicy>> {
    load_from(&policy::system_path())
}

/// [`load_system`] for a given path, so tests can exercise startup.
///
/// # Arguments
///
/// * `path` - The policy file.
///
/// # Returns
///
/// The policy, or `None` when the file does not exist.
///
/// # Errors
///
/// When the file exists but cannot be read or is invalid; the message
/// says that anvil will not start.
pub fn load_from(path: &Path) -> anyhow::Result<Option<LoadedPolicy>> {
    policy::load(path).map_err(|error| {
        anyhow::anyhow!("{error}; anvil does not start until an administrator fixes or removes it")
    })
}

/// The flags of this invocation that skip prompts or replace the rules.
#[derive(Debug, Clone, Copy, Default)]
pub struct SkipFlags {
    /// `--yes`.
    pub yes: bool,
    /// Any `--allow-tool`.
    pub allow_tool: bool,
    /// `--profile`.
    pub profile: bool,
}

/// Refuses the flags the policy disables.
///
/// # Arguments
///
/// * `policy` - The policy, if any.
/// * `flags` - What was passed.
///
/// # Errors
///
/// Naming the first disabled flag and the policy file.
pub fn check_flags(policy: Option<&LoadedPolicy>, flags: SkipFlags) -> anyhow::Result<()> {
    let Some(loaded) = policy else {
        return Ok(());
    };
    if loaded.policy.permissions.allow_skip_prompts {
        return Ok(());
    }
    let used = [
        (flags.yes, "--yes"),
        (flags.allow_tool, "--allow-tool"),
        (flags.profile, "--profile"),
    ];
    match used.iter().find(|(set, _)| *set) {
        Some((_, flag)) => anyhow::bail!(
            "{flag} is disabled by the managed policy {}",
            loaded.path.display()
        ),
        None => Ok(()),
    }
}

/// The folder a repository's sessions live in.
///
/// # Arguments
///
/// * `project` - The repository root.
/// * `policy` - The policy, if any.
///
/// # Returns
///
/// The policy's `transcript_dir` subfolder for the repository, or
/// `<project>/.anvil/sessions`.
pub fn sessions_root(project: &Path, policy: Option<&LoadedPolicy>) -> PathBuf {
    policy
        .and_then(|loaded| loaded.sessions_root(project))
        .unwrap_or_else(|| project_sessions_root(project))
}

/// Deletes session folders older than the policy's `retention_days`.
///
/// Only folders whose name is a session id (26 letters and digits) are
/// touched, so nothing else in the folder can be removed by mistake.
///
/// # Arguments
///
/// * `root` - The sessions folder.
/// * `days` - The retention.
/// * `now` - The current time.
///
/// # Returns
///
/// How many folders were removed.
pub fn prune_sessions(root: &Path, days: u32, now: SystemTime) -> usize {
    let Ok(entries) = std::fs::read_dir(root) else {
        return 0;
    };
    let keep = Duration::from_secs(u64::from(days) * 24 * 60 * 60);
    let mut removed = 0;
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_session = name.len() == 26 && name.chars().all(|c| c.is_ascii_alphanumeric());
        let old = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > keep);
        if is_session && old && entry.path().is_dir() {
            match std::fs::remove_dir_all(entry.path()) {
                Ok(()) => removed += 1,
                Err(error) => tracing::warn!(%error, session = name, "cannot prune session"),
            }
        }
    }
    removed
}

/// The lines `anvil doctor` prints about the policy.
///
/// # Arguments
///
/// * `policy` - The policy, if any.
///
/// # Returns
///
/// Where it was loaded from and what it disables, or that none is installed.
pub fn report(policy: Option<&LoadedPolicy>) -> Vec<String> {
    let Some(loaded) = policy else {
        return vec![format!(
            "policy: none installed at {} (personal mode)",
            policy::system_path().display()
        )];
    };
    let mut lines = vec![format!("policy: loaded from {}", loaded.path.display())];
    if let Some(profile) = &loaded.policy.permissions.profile {
        lines.push(format!("policy: forced permission profile {profile}"));
    }
    if let Some(dir) = &loaded.policy.sessions.transcript_dir {
        lines.push(format!("policy: transcripts in {}", dir.display()));
    }
    if let Some(days) = loaded.policy.sessions.retention_days {
        lines.push(format!("policy: sessions kept {days} day(s)"));
    }
    for line in loaded.disables() {
        lines.push(format!("policy disables: {line}"));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = "[endpoints]\nallowed_base_urls = [\"https://models.corp.example/v1\"]\n";

    fn policy(dir: &Path, extra: &str) -> LoadedPolicy {
        let path = dir.join("policy.toml");
        std::fs::write(&path, format!("{MINIMAL}{extra}")).expect("write");
        load_from(&path).expect("loads").expect("present")
    }

    #[test]
    fn a_broken_policy_stops_startup_with_the_reason() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("policy.toml");
        std::fs::write(&path, "[endpoints\n").expect("write");
        let error = load_from(&path).expect_err("broken");
        let text = format!("{error:#}");
        assert!(text.contains("does not start"), "{text}");
        assert!(text.contains("policy.toml"), "{text}");
        assert!(
            load_from(&dir.path().join("absent.toml"))
                .expect("ok")
                .is_none()
        );
    }

    #[test]
    fn skip_flags_are_refused_unless_the_policy_allows_them() {
        let dir = tempfile::tempdir().expect("dir");
        let strict = policy(dir.path(), "");
        for flags in [
            SkipFlags {
                yes: true,
                ..SkipFlags::default()
            },
            SkipFlags {
                allow_tool: true,
                ..SkipFlags::default()
            },
            SkipFlags {
                profile: true,
                ..SkipFlags::default()
            },
        ] {
            let error = check_flags(Some(&strict), flags).expect_err("refused");
            assert!(format!("{error}").contains("disabled by the managed policy"));
        }
        assert!(check_flags(Some(&strict), SkipFlags::default()).is_ok());
        let open = policy(dir.path(), "[permissions]\nallow_skip_prompts = true\n");
        let all = SkipFlags {
            yes: true,
            allow_tool: true,
            profile: true,
        };
        assert!(check_flags(Some(&open), all).is_ok());
        assert!(check_flags(None, all).is_ok(), "personal mode");
    }

    #[test]
    fn transcripts_go_where_the_policy_says() {
        let dir = tempfile::tempdir().expect("dir");
        let project = dir.path().join("repo");
        assert_eq!(
            sessions_root(&project, None),
            project.join(".anvil").join("sessions")
        );
        let moved = policy(
            dir.path(),
            &format!(
                "[sessions]\ntranscript_dir = '{}'\n",
                dir.path().join("transcripts").display()
            ),
        );
        assert!(sessions_root(&project, Some(&moved)).starts_with(dir.path().join("transcripts")));
    }

    #[test]
    fn pruning_removes_only_old_session_folders() {
        let dir = tempfile::tempdir().expect("dir");
        let old = dir.path().join("01ARZ3NDEKTSV4RRFFQ69G5FAV");
        let other = dir.path().join("notes");
        std::fs::create_dir_all(&old).expect("mkdir");
        std::fs::create_dir_all(&other).expect("mkdir");
        let later = SystemTime::now() + Duration::from_secs(40 * 24 * 60 * 60);
        assert_eq!(
            prune_sessions(dir.path(), 30, SystemTime::now()),
            0,
            "still fresh"
        );
        assert_eq!(prune_sessions(dir.path(), 30, later), 1);
        assert!(!old.exists());
        assert!(other.exists(), "a folder that is not a session stays");
    }

    #[test]
    fn the_report_names_the_file_and_what_it_disables() {
        let dir = tempfile::tempdir().expect("dir");
        let loaded = policy(dir.path(), "");
        let text = report(Some(&loaded)).join("\n");
        assert!(text.contains("loaded from"), "{text}");
        assert!(text.contains("policy disables: hooks"), "{text}");
        assert!(report(None)[0].contains("personal mode"));
    }
}
