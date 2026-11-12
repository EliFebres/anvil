//! Project templates: `~/.anvil/templates/<name>/`.
//!
//! A template is a directory tree plus a short `template.toml` naming it.
//! `anvil new` copies the tree into a new project; `anvil template save`
//! makes a template from an existing directory. A directory without a
//! valid `template.toml` is not a template and is reported, not used.

use std::path::{Path, PathBuf};

use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};

use crate::error::MemoryError;
use crate::languages::detect_languages;
use crate::paths::templates_dir;

/// The manifest file inside a template directory; it is never copied.
pub const MANIFEST_FILE: &str = "template.toml";

/// Directories that are never copied into or out of a template.
const NEVER_COPIED: &[&str] = &[".git", ".anvil"];

/// The contents of a `template.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TemplateManifest {
    /// The template's name; it must equal its directory name.
    pub name: String,
    /// One line saying what the template is for.
    pub description: String,
    /// The languages the template's code is written in.
    pub languages: Vec<String>,
}

impl TemplateManifest {
    /// Checks the manifest's fields.
    ///
    /// # Returns
    ///
    /// `Ok(())` when the name is valid and the description is one
    /// non-empty line.
    ///
    /// # Errors
    ///
    /// [`MemoryError::InvalidTemplateName`] or [`MemoryError::InvalidTemplate`].
    pub fn validate(&self) -> Result<(), MemoryError> {
        validate_name(&self.name)?;
        if self.description.trim().is_empty() || self.description.contains('\n') {
            return Err(MemoryError::InvalidTemplate {
                name: self.name.clone(),
                detail: "description must be one non-empty line".to_owned(),
            });
        }
        Ok(())
    }
}

/// A template found on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    /// The validated manifest.
    pub manifest: TemplateManifest,
    /// The template directory.
    pub dir: PathBuf,
}

/// Checks a template name, which also names its directory.
///
/// # Arguments
///
/// * `name` - The name.
///
/// # Errors
///
/// [`MemoryError::InvalidTemplateName`] unless it is non-empty and uses
/// only a-z, 0-9, `-` and `_`.
pub fn validate_name(name: &str) -> Result<(), MemoryError> {
    let valid = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if valid {
        Ok(())
    } else {
        Err(MemoryError::InvalidTemplateName {
            name: name.to_owned(),
        })
    }
}

/// Every usable template under the home directory.
///
/// # Arguments
///
/// * `home` - The home directory.
///
/// # Returns
///
/// The templates sorted by name, and one warning per directory that was
/// ignored (no `template.toml`, unreadable, invalid, or named differently
/// from its directory). A missing templates directory yields nothing.
pub fn list_templates(home: &Path) -> (Vec<Template>, Vec<String>) {
    let root = templates_dir(home);
    let mut templates = Vec::new();
    let mut warnings = Vec::new();
    let Ok(entries) = std::fs::read_dir(&root) else {
        return (templates, warnings);
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    for dir in dirs {
        match load_template(&dir) {
            Ok(template) => templates.push(template),
            Err(error) => warnings.push(format!("ignoring template {}: {error}", dir.display())),
        }
    }
    templates.sort_by(|a, b| a.manifest.name.cmp(&b.manifest.name));
    (templates, warnings)
}

/// Reads and validates the manifest of one template directory.
fn load_template(dir: &Path) -> Result<Template, MemoryError> {
    let dir_name = dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let path = dir.join(MANIFEST_FILE);
    let text = std::fs::read_to_string(&path).map_err(|error| MemoryError::io(&path, &error))?;
    let manifest: TemplateManifest = toml::from_str(&text).map_err(|error| MemoryError::Parse {
        path: path.clone(),
        detail: error.to_string(),
    })?;
    manifest.validate()?;
    if manifest.name != dir_name {
        return Err(MemoryError::InvalidTemplate {
            name: dir_name,
            detail: format!("template.toml names it `{}`", manifest.name),
        });
    }
    Ok(Template {
        manifest,
        dir: dir.to_path_buf(),
    })
}

/// The usable template with this name.
///
/// # Arguments
///
/// * `home` - The home directory.
/// * `name` - The template name.
///
/// # Returns
///
/// The template.
///
/// # Errors
///
/// [`MemoryError::UnknownTemplate`], which lists the usable names.
pub fn find_template(home: &Path, name: &str) -> Result<Template, MemoryError> {
    let (templates, _) = list_templates(home);
    let available = templates
        .iter()
        .map(|template| template.manifest.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    templates
        .into_iter()
        .find(|template| template.manifest.name == name)
        .ok_or_else(|| MemoryError::UnknownTemplate {
            name: name.to_owned(),
            available: if available.is_empty() {
                "none".to_owned()
            } else {
                available
            },
        })
}

/// Whether a directory has no files, ignoring dot-directories such as
/// `.git` and `.anvil`.
///
/// # Arguments
///
/// * `dir` - The directory. A missing or unreadable one counts as empty.
///
/// # Returns
///
/// `true` when every entry is a directory whose name starts with a dot.
pub fn is_empty_project(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return true;
    };
    entries
        .flatten()
        .all(|entry| entry.path().is_dir() && entry.file_name().to_string_lossy().starts_with('.'))
}

/// Copies a template's tree into a new project.
///
/// # Arguments
///
/// * `template` - The template; its `template.toml` is not copied.
/// * `dest` - The project directory. It is created when missing and must
///   otherwise be empty by [`is_empty_project`].
///
/// # Returns
///
/// How many files were copied.
///
/// # Errors
///
/// [`MemoryError::DestinationNotEmpty`], or [`MemoryError::Io`] when
/// something cannot be copied.
pub fn instantiate(template: &Template, dest: &Path) -> Result<usize, MemoryError> {
    if dest.is_dir() && !is_empty_project(dest) {
        return Err(MemoryError::DestinationNotEmpty {
            path: dest.to_path_buf(),
        });
    }
    copy_tree(&template.dir, dest, false)
}

/// Saves a directory as a template.
///
/// The copy honors `.gitignore` and leaves out `.git/`, `.anvil/` and any
/// `template.toml` at the root.
///
/// # Arguments
///
/// * `home` - The home directory.
/// * `name` - The new template's name.
/// * `source` - The directory to copy.
/// * `description` - One line about the template; a plain default when `None`.
/// * `force` - Replace a template of the same name.
///
/// # Returns
///
/// The saved template.
///
/// # Errors
///
/// [`MemoryError::InvalidTemplateName`], [`MemoryError::InvalidTemplate`]
/// for a bad description, [`MemoryError::TemplateExists`] without `force`,
/// or [`MemoryError::Io`].
pub fn save_template(
    home: &Path,
    name: &str,
    source: &Path,
    description: Option<&str>,
    force: bool,
) -> Result<Template, MemoryError> {
    validate_name(name)?;
    let manifest = TemplateManifest {
        name: name.to_owned(),
        description: description
            .map(|text| text.trim().to_owned())
            .unwrap_or_else(|| format!("Project template `{name}`")),
        languages: detect_languages(source)
            .into_iter()
            .map(str::to_owned)
            .collect(),
    };
    manifest.validate()?;
    let dir = templates_dir(home).join(name);
    if dir.exists() {
        if !force {
            return Err(MemoryError::TemplateExists {
                name: name.to_owned(),
            });
        }
        std::fs::remove_dir_all(&dir).map_err(|error| MemoryError::io(&dir, &error))?;
    }
    copy_tree(source, &dir, true)?;
    let manifest_path = dir.join(MANIFEST_FILE);
    let text = toml::to_string(&manifest).map_err(|error| MemoryError::Parse {
        path: manifest_path.clone(),
        detail: error.to_string(),
    })?;
    std::fs::write(&manifest_path, text)
        .map_err(|error| MemoryError::io(&manifest_path, &error))?;
    Ok(Template { manifest, dir })
}

/// The Plan prompt's list of templates.
///
/// # Arguments
///
/// * `templates` - The usable templates.
///
/// # Returns
///
/// The section body (name and description only), or `None` without templates.
pub fn plan_section_text(templates: &[Template]) -> Option<String> {
    if templates.is_empty() {
        return None;
    }
    let list = templates
        .iter()
        .map(|template| {
            format!(
                "- {}: {}",
                template.manifest.name,
                template.manifest.description.trim()
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    Some(format!(
        "This directory is empty. These project templates are saved; if one fits the task, \
         plan to start from it with `anvil new <name> .` and build on it, otherwise start \
         from scratch.\n\n{list}"
    ))
}

/// Copies every file under `source` into `dest`, byte for byte.
fn copy_tree(source: &Path, dest: &Path, honor_gitignore: bool) -> Result<usize, MemoryError> {
    std::fs::create_dir_all(dest).map_err(|error| MemoryError::io(dest, &error))?;
    let walker = WalkBuilder::new(source)
        .hidden(false)
        .require_git(false)
        .git_ignore(honor_gitignore)
        .git_exclude(honor_gitignore)
        .git_global(false)
        .ignore(false)
        .sort_by_file_name(|a, b| a.cmp(b))
        .filter_entry(|entry| {
            !(entry.depth() > 0
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| NEVER_COPIED.contains(&name)))
        })
        .build();
    let mut copied = 0;
    for entry in walker {
        let entry = entry.map_err(|error| MemoryError::Io {
            path: source.to_path_buf(),
            detail: error.to_string(),
        })?;
        let Ok(relative) = entry.path().strip_prefix(source) else {
            continue;
        };
        if relative.as_os_str().is_empty() {
            continue;
        }
        let target = dest.join(relative);
        let kind = entry.file_type();
        if kind.is_some_and(|kind| kind.is_dir()) {
            std::fs::create_dir_all(&target).map_err(|error| MemoryError::io(&target, &error))?;
        } else if kind.is_some_and(|kind| kind.is_file()) {
            if relative == Path::new(MANIFEST_FILE) {
                continue;
            }
            std::fs::copy(entry.path(), &target)
                .map_err(|error| MemoryError::io(entry.path(), &error))?;
            copied += 1;
        }
    }
    Ok(copied)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, relative: &str, bytes: &[u8]) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, bytes).expect("write");
    }

    fn read_tree(root: &Path) -> Vec<(String, Vec<u8>)> {
        let mut files: Vec<(String, Vec<u8>)> = WalkBuilder::new(root)
            .hidden(false)
            .ignore(false)
            .git_ignore(false)
            .git_exclude(false)
            .git_global(false)
            .build()
            .flatten()
            .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
            .map(|entry| {
                let relative = entry.path().strip_prefix(root).expect("inside");
                (
                    relative.to_string_lossy().replace('\\', "/"),
                    std::fs::read(entry.path()).expect("read"),
                )
            })
            .collect();
        files.sort();
        files
    }

    #[test]
    fn save_then_new_reproduces_the_tree_without_ignored_files() {
        let home = tempfile::tempdir().expect("home");
        let source = tempfile::tempdir().expect("source");
        write(source.path(), "src/main.rs", b"fn main() {}\r\n");
        write(source.path(), "data/blob.bin", &[0, 159, 146, 150, 255]);
        write(source.path(), ".gitignore", b"target/\n*.log\n");
        write(source.path(), "target/debug/x", b"junk");
        write(source.path(), "run.log", b"junk");
        write(source.path(), ".git/HEAD", b"ref: refs/heads/main\n");
        write(source.path(), ".anvil/sessions/s", b"junk");

        let saved = save_template(
            home.path(),
            "rust-cli",
            source.path(),
            Some("A small Rust CLI"),
            false,
        )
        .expect("saves");
        assert_eq!(saved.manifest.languages, vec!["rust"]);
        assert!(matches!(
            save_template(home.path(), "rust-cli", source.path(), None, false),
            Err(MemoryError::TemplateExists { .. })
        ));
        save_template(home.path(), "rust-cli", source.path(), None, true).expect("forced");

        let found = find_template(home.path(), "rust-cli").expect("found");
        assert_eq!(found.manifest.description, "Project template `rust-cli`");
        let dest = tempfile::tempdir().expect("dest");
        let copied = instantiate(&found, &dest.path().join("new")).expect("copies");
        assert_eq!(copied, 3);
        let expected = vec![
            (".gitignore".to_owned(), b"target/\n*.log\n".to_vec()),
            ("data/blob.bin".to_owned(), vec![0, 159, 146, 150, 255]),
            ("src/main.rs".to_owned(), b"fn main() {}\r\n".to_vec()),
        ];
        assert_eq!(read_tree(&dest.path().join("new")), expected);
    }

    #[test]
    fn directories_without_a_valid_manifest_are_ignored_with_a_warning() {
        let home = tempfile::tempdir().expect("home");
        let root = templates_dir(home.path());
        write(&root, "plain/a.txt", b"x");
        write(&root, "broken/template.toml", b"name = ");
        write(
            &root,
            "renamed/template.toml",
            b"name = \"other\"\ndescription = \"d\"\nlanguages = []\n",
        );
        write(
            &root,
            "good/template.toml",
            b"name = \"good\"\ndescription = \"A fine one\"\nlanguages = [\"python\"]\n",
        );
        write(&root, "good/app.py", b"print(1)\n");
        let (templates, warnings) = list_templates(home.path());
        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].manifest.name, "good");
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        assert!(warnings.iter().all(|w| w.starts_with("ignoring template ")));

        let error = find_template(home.path(), "nope").expect_err("unknown");
        assert_eq!(
            error.to_string(),
            "no template named `nope`; available: good"
        );
        let (none, _) = list_templates(tempfile::tempdir().expect("empty").path());
        assert!(none.is_empty());
    }

    #[test]
    fn a_non_empty_destination_is_refused() {
        let home = tempfile::tempdir().expect("home");
        let source = tempfile::tempdir().expect("source");
        write(source.path(), "a.txt", b"x");
        let template = save_template(home.path(), "t", source.path(), None, false).expect("saves");
        let dest = tempfile::tempdir().expect("dest");
        write(dest.path(), "keep.txt", b"mine");
        assert!(matches!(
            instantiate(&template, dest.path()),
            Err(MemoryError::DestinationNotEmpty { .. })
        ));
        assert_eq!(read_tree(dest.path()).len(), 1);
        let git_only = tempfile::tempdir().expect("git only");
        std::fs::create_dir(git_only.path().join(".git")).expect("mkdir");
        assert_eq!(instantiate(&template, git_only.path()).expect("copies"), 1);
    }

    #[test]
    fn names_and_empty_projects() {
        assert!(validate_name("rust-cli_2").is_ok());
        for bad in ["", "A", "a b", "../x", "a/b"] {
            assert!(validate_name(bad).is_err(), "{bad}");
        }
        let dir = tempfile::tempdir().expect("dir");
        assert!(is_empty_project(dir.path()));
        std::fs::create_dir(dir.path().join(".anvil")).expect("mkdir");
        assert!(is_empty_project(dir.path()));
        write(dir.path(), ".gitignore", b"x");
        assert!(!is_empty_project(dir.path()));
        assert!(is_empty_project(&dir.path().join("missing")));
    }

    #[test]
    fn plan_section_lists_names_and_descriptions_only() {
        assert_eq!(plan_section_text(&[]), None);
        let template = Template {
            manifest: TemplateManifest {
                name: "py-lib".to_owned(),
                description: "A Python library".to_owned(),
                languages: vec!["python".to_owned()],
            },
            dir: PathBuf::from("/x"),
        };
        let text = plan_section_text(&[template]).expect("section");
        assert!(text.ends_with("\n\n- py-lib: A Python library"), "{text}");
        assert!(!text.contains("python\n"));
    }
}
