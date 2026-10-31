//! Where anvil and Claude Code keep per-project and per-user files.
//!
//! Every kind of file (skills, commands, agents) lives in the same four
//! places, searched in the same order: the project's `.anvil`, the
//! project's `.claude`, the home `.anvil`, the home `.claude`.

use std::fmt;
use std::path::{Path, PathBuf};

/// Which configuration directory a file was found in, highest priority first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Origin {
    /// `<project>/.anvil`.
    ProjectAnvil,
    /// `<project>/.claude`.
    ProjectClaude,
    /// `~/.anvil`.
    HomeAnvil,
    /// `~/.claude`.
    HomeClaude,
    /// Shipped with anvil.
    Bundled,
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ProjectAnvil => ".anvil",
            Self::ProjectClaude => ".claude",
            Self::HomeAnvil => "~/.anvil",
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
        (cwd.join(".anvil").join(kind), Origin::ProjectAnvil),
        (cwd.join(".claude").join(kind), Origin::ProjectClaude),
    ];
    if let Some(home) = home {
        dirs.push((home.join(".anvil").join(kind), Origin::HomeAnvil));
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
                (PathBuf::from("p/.anvil/skills"), Origin::ProjectAnvil),
                (PathBuf::from("p/.claude/skills"), Origin::ProjectClaude),
                (PathBuf::from("h/.anvil/skills"), Origin::HomeAnvil),
                (PathBuf::from("h/.claude/skills"), Origin::HomeClaude),
            ]
        );
        assert_eq!(config_dirs(Path::new("p"), None, "x").len(), 2);
        assert_eq!(Origin::HomeClaude.to_string(), "~/.claude");
    }
}
