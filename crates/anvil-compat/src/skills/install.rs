//! `anvil skill add`, `remove`, `list` and `show`: copying skill folders
//! from a directory on this machine.

use std::fmt;
use std::path::{Path, PathBuf};

use super::{SKILL_FILE, Skill, load_skill};
use crate::error::CompatError;
use crate::paths::Origin;

/// How deep to look for `SKILL.md` files when suggesting candidates.
const CANDIDATE_DEPTH: usize = 4;

/// Where an installed skill goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// `~/.anvil/skills/<name>`.
    Global,
    /// `<project>/.anvil/skills/<name>`.
    Project,
}

impl Scope {
    /// The `skills/` directory for this scope.
    ///
    /// # Arguments
    ///
    /// * `home` - The home directory, if known.
    /// * `cwd` - The project root.
    ///
    /// # Returns
    ///
    /// The directory (it may not exist yet).
    ///
    /// # Errors
    ///
    /// [`CompatError::Install`] when the global scope is asked for and the
    /// home directory is unknown.
    pub fn root(self, home: Option<&Path>, cwd: &Path) -> Result<PathBuf, CompatError> {
        match self {
            Self::Project => Ok(cwd.join(".anvil").join("skills")),
            Self::Global => home
                .map(|home| home.join(".anvil").join("skills"))
                .ok_or_else(|| CompatError::Install {
                    detail: "cannot determine the home directory; use --project".to_owned(),
                }),
        }
    }

    /// The source label skills installed here load under.
    fn source(self) -> Origin {
        match self {
            Self::Global => Origin::HomeAnvil,
            Self::Project => Origin::ProjectAnvil,
        }
    }
}

/// Where a skill comes from: a directory on this machine.
///
/// anvil does not download skills. To install one from a repository, clone
/// it yourself and pass the directory, so you can read what you install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    /// The directory that holds `SKILL.md`.
    pub dir: PathBuf,
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.dir.display())
    }
}

impl Source {
    /// Interprets what the user typed after `anvil skill add`.
    ///
    /// # Arguments
    ///
    /// * `text` - The argument.
    /// * `cwd` - What relative paths resolve against.
    ///
    /// # Returns
    ///
    /// The source.
    ///
    /// # Errors
    ///
    /// [`CompatError::Install`] when the text is not an existing directory,
    /// with a hint when it looks like a URL.
    pub fn parse(text: &str, cwd: &Path) -> Result<Self, CompatError> {
        let text = text.trim();
        let dir = cwd.join(text);
        if dir.is_dir() {
            return Ok(Self { dir });
        }
        let looks_remote = text.contains("://") || text.starts_with("git@");
        let hint = if looks_remote {
            "; anvil does not download skills, so clone the repository yourself and pass the directory"
        } else {
            ""
        };
        Err(CompatError::Install {
            detail: format!("`{text}` is not a directory{hint}"),
        })
    }
}

/// What `install` produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    /// The skill, loaded from its new location.
    pub skill: Skill,
    /// Whether an existing copy was replaced.
    pub replaced: bool,
}

/// Copies a skill into a scope's `skills/` directory.
///
/// # Arguments
///
/// * `source` - Where the skill comes from.
/// * `scope` - Global or project.
/// * `home` - The home directory, if known.
/// * `cwd` - The project root.
/// * `force` - Replace an existing skill of the same name.
///
/// # Returns
///
/// The installed skill.
///
/// # Errors
///
/// [`CompatError::Install`] when the source has no `SKILL.md` or the
/// destination exists and `force` is off; the loader's errors when the
/// frontmatter is invalid; [`CompatError::Io`] when copying fails.
pub fn install(
    source: &Source,
    scope: Scope,
    home: Option<&Path>,
    cwd: &Path,
    force: bool,
) -> Result<Installed, CompatError> {
    let dest_root = scope.root(home, cwd)?;
    let skill_dir = &source.dir;
    let skill_file = skill_dir.join(SKILL_FILE);
    if !skill_file.is_file() {
        return Err(no_skill_here(skill_dir));
    }
    let skill = load_skill(&skill_file, scope.source())?;
    let dest = dest_root.join(skill.name());
    let replaced = dest.exists();
    if replaced && !force {
        return Err(CompatError::Install {
            detail: format!(
                "skill `{}` is already installed at {}; pass --force to replace it",
                skill.name(),
                dest.display()
            ),
        });
    }
    if replaced {
        std::fs::remove_dir_all(&dest).map_err(|error| CompatError::io(&dest, &error))?;
    }
    copy_dir(skill_dir, &dest)?;
    let skill = load_skill(&dest.join(SKILL_FILE), scope.source())?;
    Ok(Installed { skill, replaced })
}

/// Deletes an installed skill.
///
/// # Arguments
///
/// * `name` - The skill name (its directory name).
/// * `scope` - Global or project.
/// * `home` - The home directory, if known.
/// * `cwd` - The project root.
///
/// # Returns
///
/// The directory that was removed.
///
/// # Errors
///
/// [`CompatError::Install`] when no such skill is installed in that scope,
/// [`CompatError::Io`] when it cannot be deleted.
pub fn remove(
    name: &str,
    scope: Scope,
    home: Option<&Path>,
    cwd: &Path,
) -> Result<PathBuf, CompatError> {
    let dir = scope.root(home, cwd)?.join(name);
    if !dir.join(SKILL_FILE).is_file() {
        return Err(CompatError::Install {
            detail: format!("no skill `{name}` is installed at {}", dir.display()),
        });
    }
    std::fs::remove_dir_all(&dir).map_err(|error| CompatError::io(&dir, &error))?;
    Ok(dir)
}

/// The error for a source that has no `SKILL.md`, listing the skills that
/// were found nearby so the user can pick one.
fn no_skill_here(skill_dir: &Path) -> CompatError {
    let mut candidates = Vec::new();
    find_candidates(skill_dir, skill_dir, 0, &mut candidates);
    candidates.sort();
    let hint = if candidates.is_empty() {
        String::new()
    } else {
        format!(
            "\nskills found under it: {}; point at one of them",
            candidates.join(", ")
        )
    };
    CompatError::Install {
        detail: format!("{} has no {SKILL_FILE}{hint}", skill_dir.display()),
    }
}

/// Collects directories holding `SKILL.md` below `dir`, relative to `base`.
fn find_candidates(base: &Path, dir: &Path, depth: usize, found: &mut Vec<String>) {
    if depth > CANDIDATE_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if !path.is_dir() || entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        if path.join(SKILL_FILE).is_file()
            && let Ok(relative) = path.strip_prefix(base)
        {
            found.push(relative.to_string_lossy().replace('\\', "/"));
            continue;
        }
        find_candidates(base, &path, depth + 1, found);
    }
}

/// Copies a directory tree, skipping `.git`.
fn copy_dir(from: &Path, to: &Path) -> Result<(), CompatError> {
    std::fs::create_dir_all(to).map_err(|error| CompatError::io(to, &error))?;
    let entries = std::fs::read_dir(from).map_err(|error| CompatError::io(from, &error))?;
    for entry in entries {
        let entry = entry.map_err(|error| CompatError::io(from, &error))?;
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        let source = entry.path();
        let target = to.join(&name);
        if source.is_dir() {
            copy_dir(&source, &target)?;
        } else {
            std::fs::copy(&source, &target).map_err(|error| CompatError::io(&source, &error))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_local_directory_is_a_source() {
        let cwd = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(cwd.path().join("local")).expect("mkdir");
        assert_eq!(
            Source::parse("local", cwd.path()),
            Ok(Source {
                dir: cwd.path().join("local")
            })
        );
        for remote in [
            "https://github.com/owner/repo",
            "git@github.com:owner/repo.git",
            "ssh://host/repo.git",
        ] {
            let error = Source::parse(remote, cwd.path()).expect_err(remote);
            assert!(error.to_string().contains("does not download"), "{error}");
        }
        assert!(Source::parse("owner/repo", cwd.path()).is_err());
        assert!(Source::parse("./missing", cwd.path()).is_err());
    }

    #[test]
    fn installs_from_a_directory_and_refuses_to_overwrite() {
        let cwd = tempfile::tempdir().expect("tempdir");
        let source_dir = cwd.path().join("src-skill");
        std::fs::create_dir_all(source_dir.join("scripts")).expect("mkdir");
        std::fs::write(
            source_dir.join(SKILL_FILE),
            "---\n---\nname: caveman\ndescription: >\n  Terse voice.\n---\n\nBody.\n",
        )
        .expect("write");
        std::fs::write(source_dir.join("scripts/x.sh"), "echo\n").expect("write");

        let source = Source::parse("src-skill", cwd.path()).expect("source");
        let installed =
            install(&source, Scope::Project, None, cwd.path(), false).expect("installs");
        assert_eq!(installed.skill.name(), "caveman");
        assert!(!installed.replaced);
        assert_eq!(
            installed.skill.dir,
            cwd.path().join(".anvil/skills/caveman")
        );
        assert_eq!(installed.skill.files, ["scripts/x.sh"]);

        let again = install(&source, Scope::Project, None, cwd.path(), false);
        assert!(
            matches!(again, Err(CompatError::Install { detail }) if detail.contains("--force"))
        );
        let forced = install(&source, Scope::Project, None, cwd.path(), true).expect("replaces");
        assert!(forced.replaced);

        let removed = remove("caveman", Scope::Project, None, cwd.path()).expect("removes");
        assert!(!removed.exists());
        assert!(remove("caveman", Scope::Project, None, cwd.path()).is_err());
    }

    #[test]
    fn a_directory_without_a_skill_lists_candidates() {
        let cwd = tempfile::tempdir().expect("tempdir");
        let repo = cwd.path().join("repo");
        for name in ["skills/caveman", "skills/ultracave"] {
            std::fs::create_dir_all(repo.join(name)).expect("mkdir");
            std::fs::write(
                repo.join(name).join(SKILL_FILE),
                "---\nname: x\ndescription: y\n---\n",
            )
            .expect("write");
        }
        let error = install(
            &Source { dir: repo },
            Scope::Project,
            None,
            cwd.path(),
            false,
        )
        .expect_err("no skill at the root");
        let CompatError::Install { detail } = error else {
            panic!("unexpected error {error:?}");
        };
        assert!(
            detail.contains("skills/caveman, skills/ultracave"),
            "{detail}"
        );
    }

    #[test]
    fn global_scope_needs_a_home() {
        assert!(Scope::Global.root(None, Path::new(".")).is_err());
        assert_eq!(
            Scope::Global.root(Some(Path::new("/h")), Path::new(".")),
            Ok(PathBuf::from("/h").join(".anvil").join("skills"))
        );
    }
}
