//! `anvil new` and `anvil template save`, and the Plan prompt's template list.

use std::path::Path;

use anvil_core::agent::PromptSection;
use anvil_memory::templates::{
    find_template, instantiate, is_empty_project, list_templates, plan_section_text, save_template,
};
use anyhow::Context;

/// Heading of the template list in the Plan prompt.
pub const TEMPLATES_HEADING: &str = "Project templates";

/// Runs `anvil new`: copies a template into a project directory.
///
/// # Arguments
///
/// * `home` - The home directory, if known.
/// * `cwd` - The current directory, which relative `dir` paths resolve against.
/// * `template` - The template name.
/// * `dir` - The project directory.
///
/// # Returns
///
/// Zero.
///
/// # Errors
///
/// When there is no home directory, the template is unknown (the message
/// lists the available ones), the directory is not empty, or a file
/// cannot be copied.
pub fn new_project(
    home: Option<&Path>,
    cwd: &Path,
    template: &str,
    dir: &Path,
) -> anyhow::Result<i32> {
    let home = home.context("cannot determine the home directory")?;
    for warning in list_templates(home).1 {
        eprintln!("warning: {warning}");
    }
    let template = find_template(home, template)?;
    let dest = cwd.join(dir);
    let copied = instantiate(&template, &dest)?;
    println!(
        "created {} from template `{}`: {copied} file(s)",
        dest.display(),
        template.manifest.name
    );
    Ok(0)
}

/// Runs `anvil template save`: stores a directory as a template.
///
/// # Arguments
///
/// * `home` - The home directory, if known.
/// * `cwd` - The current directory, the default source.
/// * `name` - The template name.
/// * `dir` - The directory to save, if not the current one.
/// * `description` - One line about the template.
/// * `force` - Replace an existing template.
///
/// # Returns
///
/// Zero.
///
/// # Errors
///
/// When there is no home directory, the name or description is invalid,
/// the template exists and `force` is not set, or a file cannot be copied.
pub fn save(
    home: Option<&Path>,
    cwd: &Path,
    name: &str,
    dir: Option<&Path>,
    description: Option<&str>,
    force: bool,
) -> anyhow::Result<i32> {
    let home = home.context("cannot determine the home directory")?;
    let source = dir.map_or_else(|| cwd.to_path_buf(), |dir| cwd.join(dir));
    anyhow::ensure!(source.is_dir(), "{} is not a directory", source.display());
    let template = save_template(home, name, &source, description, force)?;
    println!(
        "saved template `{}` ({}) to {}",
        template.manifest.name,
        template.manifest.languages.join(", "),
        template.dir.display()
    );
    Ok(0)
}

/// The Plan prompt's template list, for a task that starts in an empty
/// directory.
///
/// # Arguments
///
/// * `home` - The home directory; without one there is no section.
/// * `cwd` - The project root.
///
/// # Returns
///
/// The section, or `None` when the directory holds files or no template
/// is saved. Ignored template directories are logged as warnings.
pub fn plan_section(home: Option<&Path>, cwd: &Path) -> Option<PromptSection> {
    let home = home?;
    if !is_empty_project(cwd) {
        return None;
    }
    let (templates, warnings) = list_templates(home);
    for warning in warnings {
        tracing::warn!("{warning}");
    }
    plan_section_text(&templates).map(|text| PromptSection {
        heading: TEMPLATES_HEADING.to_owned(),
        text,
    })
}

#[cfg(test)]
mod tests {
    use anvil_core::agent::State;
    use anvil_core::agent::prompts::{PromptInputs, system_prompt};

    use super::*;

    /// A home directory holding one saved template, `py-lib`.
    fn home_with_template() -> tempfile::TempDir {
        let home = tempfile::tempdir().expect("home");
        let source = tempfile::tempdir().expect("source");
        std::fs::write(source.path().join("app.py"), "print(1)\n").expect("write");
        save_template(
            home.path(),
            "py-lib",
            source.path(),
            Some("A small Python library"),
            false,
        )
        .expect("saves");
        home
    }

    #[test]
    fn the_plan_section_appears_only_for_an_empty_directory() {
        let home = home_with_template();
        let empty = tempfile::tempdir().expect("cwd");
        std::fs::create_dir(empty.path().join(".git")).expect("mkdir");
        let section = plan_section(Some(home.path()), empty.path()).expect("section");
        assert_eq!(section.heading, "Project templates");
        insta::assert_snapshot!(system_prompt(&PromptInputs {
            state: State::Plan,
            instructions: &[],
            sections: &[section],
            tools: &[],
            task: "build a small Python library",
        }));

        std::fs::write(empty.path().join("README.md"), "hi\n").expect("write");
        assert_eq!(plan_section(Some(home.path()), empty.path()), None);
        let no_templates = tempfile::tempdir().expect("home");
        let fresh = tempfile::tempdir().expect("cwd");
        assert_eq!(plan_section(Some(no_templates.path()), fresh.path()), None);
        assert_eq!(plan_section(None, fresh.path()), None);
    }

    #[test]
    fn new_creates_the_project_and_unknown_names_list_the_choices() {
        let home = home_with_template();
        let cwd = tempfile::tempdir().expect("cwd");
        let code = new_project(Some(home.path()), cwd.path(), "py-lib", Path::new("demo"))
            .expect("creates");
        assert_eq!(code, 0);
        assert_eq!(
            std::fs::read_to_string(cwd.path().join("demo").join("app.py")).expect("copied"),
            "print(1)\n"
        );
        assert!(!cwd.path().join("demo").join("template.toml").exists());
        let error = new_project(Some(home.path()), cwd.path(), "nope", Path::new("x"))
            .expect_err("unknown");
        assert!(
            format!("{error:#}").contains("available: py-lib"),
            "{error:#}"
        );
        let error = new_project(Some(home.path()), cwd.path(), "py-lib", Path::new("demo"))
            .expect_err("not empty");
        assert!(format!("{error:#}").contains("is not empty"), "{error:#}");
    }

    #[test]
    fn save_defaults_to_the_current_directory_and_refuses_to_overwrite() {
        let home = tempfile::tempdir().expect("home");
        let cwd = tempfile::tempdir().expect("cwd");
        std::fs::write(cwd.path().join("main.go"), "package main\n").expect("write");
        save(Some(home.path()), cwd.path(), "go-app", None, None, false).expect("saves");
        let error =
            save(Some(home.path()), cwd.path(), "go-app", None, None, false).expect_err("exists");
        assert!(format!("{error:#}").contains("--force"), "{error:#}");
        save(
            Some(home.path()),
            cwd.path(),
            "go-app",
            None,
            Some("Go"),
            true,
        )
        .expect("forced");
        assert!(save(None, cwd.path(), "x", None, None, false).is_err());
    }
}
