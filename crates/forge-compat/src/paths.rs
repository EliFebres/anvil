//! Where forge and Claude Code keep per-project and per-user files.
//!
//! Every kind of file (skills, commands, agents) lives in the same four
//! places, searched in the same order: the project's `.forge`, the
//! project's `.claude`, the home `.forge`, the home `.claude`.

use std::fmt;
use std::path::{Path, PathBuf};

/// Which configuration directory a file was found in, highest priority first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Origin {
    /// `<project>/.forge`.
    ProjectForge,
    /// `<project>/.claude`.
    ProjectClaude,
    /// `~/.forge`.
    HomeForge,
    /// `~/.claude`.
    HomeClaude,
    /// Shipped with forge.
    Bundled,
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ProjectForge => ".forge",
            Self::ProjectClaude => ".claude",
            Self::HomeForge => "~/.forge",
            Self::HomeClaude => "~/.claude",
            Self::Bundled => "bundled",
        })
    }
}

/// The four configuration directories that may hold `kind`, in priority
/// order, whether or not they exist.
///
/// # Arguments
///
/// * `cwd` - The project root.
/// * `home` - The home directory, if known.
/// * `kind` - The subdirectory name, such as `skills` or `commands`.
///
/// # Returns
///
/// Each directory with its origin.
pub fn config_dirs(cwd: &Path, home: Option<&Path>, kind: &str) -> Vec<(PathBuf, Origin)> {
    let mut dirs = vec![
        (cwd.join(".forge").join(kind), Origin::ProjectForge),
        (cwd.join(".claude").join(kind), Origin::ProjectClaude),
    ];
    if let Some(home) = home {
        dirs.push((home.join(".forge").join(kind), Origin::HomeForge));
        dirs.push((home.join(".claude").join(kind), Origin::HomeClaude));
    }
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_project_then_home_and_skips_home_when_unknown() {
        let dirs = config_dirs(Path::new("p"), Some(Path::new("h")), "skills");
        assert_eq!(
            dirs,
            vec![
                (PathBuf::from("p/.forge/skills"), Origin::ProjectForge),
                (PathBuf::from("p/.claude/skills"), Origin::ProjectClaude),
                (PathBuf::from("h/.forge/skills"), Origin::HomeForge),
                (PathBuf::from("h/.claude/skills"), Origin::HomeClaude),
            ]
        );
        assert_eq!(config_dirs(Path::new("p"), None, "x").len(), 2);
        assert_eq!(Origin::HomeClaude.to_string(), "~/.claude");
    }
}
