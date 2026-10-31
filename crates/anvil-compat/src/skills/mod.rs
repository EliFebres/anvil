//! Skills: directories holding a Claude Code-format `SKILL.md`.
//!
//! Disclosure is progressive, the way Claude Code does it. The system prompt
//! lists every skill's name and description ([`SkillSet::prompt_section`]);
//! the body enters the conversation only when the model calls the `skill`
//! tool ([`SkillTool`]) or the user types `/<name>`.

pub mod install;
pub mod tool;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anvil_core::agent::Instruction;
use anvil_types::SkillManifest;

pub use tool::SkillTool;

use crate::error::{CompatError, LoadError};
use crate::frontmatter;
use crate::names::to_anvil_name;
use crate::paths::{Origin, config_dirs};

/// The file that makes a directory a skill.
pub const SKILL_FILE: &str = "SKILL.md";

/// The subdirectory skills live in.
pub const SKILLS_DIR: &str = "skills";

/// How deep below a root a `SKILL.md` may sit. Claude Code's synced skills
/// live at `skills/synced/<id>/<name>/SKILL.md`.
const MAX_DEPTH: usize = 3;

/// Most supporting files listed for one skill.
const MAX_LISTED_FILES: usize = 200;

/// The directories skills are discovered from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillRoots {
    /// The project root; `.anvil/skills` and `.claude/skills` sit under it.
    pub project: PathBuf,
    /// The home directory, if known; `.anvil/skills` and `.claude/skills`
    /// sit under it.
    pub home: Option<PathBuf>,
    /// anvil's own `skills/` directory, when it exists on this machine.
    pub bundled: Option<PathBuf>,
}

impl SkillRoots {
    /// The roots in priority order, whether or not they exist.
    ///
    /// # Returns
    ///
    /// Each root with its origin.
    pub fn ordered(&self) -> Vec<(PathBuf, Origin)> {
        let mut roots = config_dirs(&self.project, self.home.as_deref(), SKILLS_DIR);
        if let Some(bundled) = &self.bundled {
            roots.push((bundled.clone(), Origin::Bundled));
        }
        roots
    }
}

/// One loaded skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    /// The frontmatter.
    pub manifest: SkillManifest,
    /// The directory holding `SKILL.md`.
    pub dir: PathBuf,
    /// The instruction text below the frontmatter.
    pub body: String,
    /// Supporting files, relative to `dir` with forward slashes, sorted.
    pub files: Vec<String>,
    /// Which root it came from.
    pub source: Origin,
}

impl Skill {
    /// The skill's name.
    ///
    /// # Returns
    ///
    /// The `name` frontmatter key.
    pub fn name(&self) -> &str {
        &self.manifest.name
    }

    /// The `requires` entries no available tool satisfies.
    ///
    /// A requirement is met by a tool of that exact name (Claude Code
    /// spellings such as `Bash` are translated) or by any tool of an MCP
    /// server of that name (`mcp__<name>__*`).
    ///
    /// # Arguments
    ///
    /// * `available_tools` - Registered tool names.
    ///
    /// # Returns
    ///
    /// The unmet requirements, as written.
    pub fn missing_requires(&self, available_tools: &[&str]) -> Vec<String> {
        self.manifest
            .requires
            .iter()
            .filter(|requirement| {
                let anvil_name = to_anvil_name(requirement);
                let server_prefix = format!("mcp__{requirement}__");
                !available_tools
                    .iter()
                    .any(|tool| *tool == anvil_name || tool.starts_with(&server_prefix))
            })
            .cloned()
            .collect()
    }

    /// Renders the body and file list for the model.
    ///
    /// # Arguments
    ///
    /// * `args` - What the caller passed along with the skill name, if anything.
    ///
    /// # Returns
    ///
    /// The text the `skill` tool returns.
    pub fn render(&self, args: Option<&str>) -> String {
        let mut text = format!("# Skill: {}\n\n{}", self.name(), self.body.trim_end());
        if !self.files.is_empty() {
            text.push_str("\n\n## Supporting files (read them with read_file when needed)\n");
            let dir = self.dir.to_string_lossy().replace('\\', "/");
            for file in &self.files {
                text.push_str(&format!("\n- {dir}/{file}"));
            }
        }
        if let Some(args) = args.map(str::trim).filter(|args| !args.is_empty()) {
            text.push_str(&format!("\n\n## Arguments\n\n{args}"));
        }
        text.push('\n');
        text
    }
}

/// Every skill found, deduplicated by name in root priority order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkillSet {
    /// Loaded skills, sorted by name.
    skills: Vec<Skill>,
    /// Directories that looked like skills but did not load.
    errors: Vec<LoadError>,
    /// Skills hidden by a same-named skill in a higher-priority root.
    shadowed: Vec<(String, PathBuf)>,
}

impl SkillSet {
    /// Walks the roots and loads every skill.
    ///
    /// Roots that do not exist are skipped. A skill that fails to load is
    /// recorded in [`errors`](Self::errors) and logged; it never stops the
    /// others from loading.
    ///
    /// # Arguments
    ///
    /// * `roots` - Where to look.
    ///
    /// # Returns
    ///
    /// The set.
    pub fn discover(roots: &SkillRoots) -> Self {
        let mut set = Self::default();
        let mut seen = BTreeSet::new();
        for (root, source) in roots.ordered() {
            if !root.is_dir() {
                continue;
            }
            for skill_file in find_skill_files(&root) {
                match load_skill(&skill_file, source) {
                    Ok(skill) => {
                        if seen.insert(skill.manifest.name.clone()) {
                            set.skills.push(skill);
                        } else {
                            tracing::debug!(path = %skill_file.display(), name = skill.name(), "skill shadowed by a higher-priority root");
                            set.shadowed.push((skill.manifest.name, skill_file));
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%error, "skipping skill");
                        set.errors.push(LoadError {
                            path: skill_file,
                            error,
                        });
                    }
                }
            }
        }
        set.skills
            .sort_by(|a, b| a.manifest.name.cmp(&b.manifest.name));
        set
    }

    /// A set holding the given skills, for callers that load their own.
    ///
    /// # Arguments
    ///
    /// * `skills` - The skills; later duplicates by name are dropped.
    ///
    /// # Returns
    ///
    /// The set, sorted by name.
    pub fn from_skills(skills: Vec<Skill>) -> Self {
        let mut set = Self::default();
        let mut seen = BTreeSet::new();
        for skill in skills {
            if seen.insert(skill.manifest.name.clone()) {
                set.skills.push(skill);
            }
        }
        set.skills
            .sort_by(|a, b| a.manifest.name.cmp(&b.manifest.name));
        set
    }

    /// The loaded skills, sorted by name.
    ///
    /// # Returns
    ///
    /// The skills.
    pub fn skills(&self) -> &[Skill] {
        &self.skills
    }

    /// The directories that failed to load.
    ///
    /// # Returns
    ///
    /// The errors, in discovery order.
    pub fn errors(&self) -> &[LoadError] {
        &self.errors
    }

    /// Skills hidden by a same-named skill in a higher-priority root.
    ///
    /// # Returns
    ///
    /// Pairs of skill name and the hidden `SKILL.md`.
    pub fn shadowed(&self) -> &[(String, PathBuf)] {
        &self.shadowed
    }

    /// Whether nothing loaded.
    ///
    /// # Returns
    ///
    /// `true` when there are no skills.
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// Looks a skill up by name.
    ///
    /// # Arguments
    ///
    /// * `name` - The skill name.
    ///
    /// # Returns
    ///
    /// The skill, if loaded.
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|skill| skill.name() == name)
    }

    /// The names of every loaded skill.
    ///
    /// # Returns
    ///
    /// Sorted names.
    pub fn names(&self) -> Vec<&str> {
        self.skills.iter().map(Skill::name).collect()
    }

    /// Every unmet `requires` entry across the set.
    ///
    /// # Arguments
    ///
    /// * `available_tools` - Registered tool names.
    ///
    /// # Returns
    ///
    /// Pairs of skill name and the missing requirement.
    pub fn missing_requires(&self, available_tools: &[&str]) -> Vec<(String, String)> {
        self.skills
            .iter()
            .flat_map(|skill| {
                skill
                    .missing_requires(available_tools)
                    .into_iter()
                    .map(move |requirement| (skill.manifest.name.clone(), requirement))
            })
            .collect()
    }

    /// The system prompt section: names and descriptions only.
    ///
    /// # Arguments
    ///
    /// * `available_tools` - Registered tool names, to flag unmet `requires`.
    ///
    /// # Returns
    ///
    /// An instruction block, or `None` when there are no skills.
    pub fn prompt_section(&self, available_tools: &[&str]) -> Option<Instruction> {
        if self.skills.is_empty() {
            return None;
        }
        let mut text = String::from(
            "Skills are instruction sets loaded on demand. When a task matches a skill's \
             description, or the user asks for it by name, call the `skill` tool with its \
             name (and any arguments) and follow the instructions it returns. Do not guess \
             at a skill's contents.\n",
        );
        for skill in &self.skills {
            let description = skill
                .manifest
                .description
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            text.push_str(&format!("\n- {}: {description}", skill.name()));
            let missing = skill.missing_requires(available_tools);
            if !missing.is_empty() {
                text.push_str(&format!(
                    " (requires {}, not available in this session)",
                    missing
                        .iter()
                        .map(|tool| format!("`{tool}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        }
        text.push('\n');
        Some(Instruction {
            source: "skills".to_owned(),
            text,
        })
    }

    /// Turns a `/<name> [args]` line into a message that invokes the skill.
    ///
    /// # Arguments
    ///
    /// * `line` - The user's line, starting with a slash.
    ///
    /// # Returns
    ///
    /// The message to send as the user turn, or `None` when the line does
    /// not start with a slash or the first word is not a loaded skill.
    pub fn expand_slash(&self, line: &str) -> Option<String> {
        let line = line.trim().strip_prefix('/')?;
        let (name, args) = match line.split_once(char::is_whitespace) {
            Some((name, args)) => (name, args.trim()),
            None => (line, ""),
        };
        let skill = self.get(name)?;
        Some(invocation_prompt(skill.name(), args))
    }

    /// Help lines for a REPL, one per skill.
    ///
    /// # Returns
    ///
    /// `/<name>` padded to a column, then a one-line description.
    pub fn help_lines(&self) -> Vec<String> {
        self.skills
            .iter()
            .map(|skill| {
                let description: String = skill
                    .manifest
                    .description
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                let short: String = description.chars().take(72).collect();
                let ellipsis = if short.len() < description.len() {
                    "..."
                } else {
                    ""
                };
                format!("/{:<16} {short}{ellipsis}", skill.name())
            })
            .collect()
    }
}

/// The user message that asks the model to load and follow a skill.
fn invocation_prompt(name: &str, args: &str) -> String {
    if args.is_empty() {
        format!(
            "Use the `{name}` skill: call the skill tool with name \"{name}\", then follow its instructions."
        )
    } else {
        format!(
            "Use the `{name}` skill with these arguments: {args}\n\nCall the skill tool with name \"{name}\" and args \"{args}\", then follow its instructions."
        )
    }
}

/// Loads one skill from its `SKILL.md`.
///
/// # Arguments
///
/// * `skill_file` - The `SKILL.md` path.
/// * `source` - Which root it belongs to.
///
/// # Returns
///
/// The skill, with its supporting files listed.
///
/// # Errors
///
/// [`CompatError::Io`] when the file cannot be read, a frontmatter error
/// when it cannot be parsed, [`CompatError::MissingField`] when `name` or
/// `description` is absent, or [`CompatError::Validation`] when the
/// manifest fails [`SkillManifest::validate`].
pub fn load_skill(skill_file: &Path, source: Origin) -> Result<Skill, CompatError> {
    let text =
        std::fs::read_to_string(skill_file).map_err(|error| CompatError::io(skill_file, &error))?;
    let parsed = frontmatter::parse(&text, skill_file)?;
    parsed.require_str("name", skill_file)?;
    parsed.require_str("description", skill_file)?;
    let manifest: SkillManifest = parsed.typed(skill_file)?;
    manifest
        .validate()
        .map_err(|source| CompatError::Validation {
            path: skill_file.to_path_buf(),
            source,
        })?;
    let dir = skill_file
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let files = list_files(&dir);
    Ok(Skill {
        manifest,
        dir,
        body: parsed.body,
        files,
        source,
    })
}

/// Every `SKILL.md` under `root`, at most [`MAX_DEPTH`] levels down,
/// sorted by path. Hidden directories are skipped.
pub(crate) fn find_skill_files(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    walk_for_skill_files(root, 0, &mut found);
    found.sort();
    found
}

/// Recursive worker for [`find_skill_files`].
fn walk_for_skill_files(dir: &Path, depth: usize, found: &mut Vec<PathBuf>) {
    let candidate = dir.join(SKILL_FILE);
    if depth > 0 && candidate.is_file() {
        found.push(candidate);
        return;
    }
    if depth >= MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            walk_for_skill_files(&path, depth + 1, found);
        }
    }
}

/// Regular files under a skill directory other than `SKILL.md`, relative
/// with forward slashes, sorted, capped at [`MAX_LISTED_FILES`].
fn list_files(dir: &Path) -> Vec<String> {
    let mut files = Vec::new();
    collect_files(dir, dir, &mut files);
    files.sort();
    files.truncate(MAX_LISTED_FILES);
    files
}

/// Recursive worker for [`list_files`].
fn collect_files(base: &Path, dir: &Path, files: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            collect_files(base, &path, files);
        } else if path.is_file() && !(dir == base && name == SKILL_FILE) {
            if let Ok(relative) = path.strip_prefix(base) {
                files.push(relative.to_string_lossy().replace('\\', "/"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BODY_SENTINEL: &str = "SECRET-BODY-TOKEN";

    fn write_skill(root: &Path, name: &str, extra_frontmatter: &str) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(dir.join("scripts")).expect("mkdir");
        std::fs::write(
            dir.join(SKILL_FILE),
            format!(
                "---\nname: {name}\ndescription: Does {name} things. Use when asked.\n{extra_frontmatter}---\n\n# {name}\n\n{BODY_SENTINEL} for {name}.\n"
            ),
        )
        .expect("write");
        std::fs::write(dir.join("scripts").join("run.py"), "print(1)\n").expect("write");
        std::fs::write(dir.join("REFERENCE.md"), "ref\n").expect("write");
        dir
    }

    fn roots(project: &Path, home: Option<&Path>) -> SkillRoots {
        SkillRoots {
            project: project.to_path_buf(),
            home: home.map(Path::to_path_buf),
            bundled: None,
        }
    }

    #[test]
    fn discovers_in_priority_order_and_dedupes_by_name() {
        let project = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        write_skill(&project.path().join(".anvil/skills"), "alpha", "");
        write_skill(&project.path().join(".claude/skills"), "alpha", "");
        write_skill(&project.path().join(".claude/skills"), "beta", "");
        write_skill(
            &home.path().join(".claude/skills/synced/abc-123"),
            "gamma",
            "",
        );

        let set = SkillSet::discover(&roots(project.path(), Some(home.path())));
        assert_eq!(set.names(), ["alpha", "beta", "gamma"]);
        assert_eq!(
            set.get("alpha").map(|skill| skill.source),
            Some(Origin::ProjectAnvil)
        );
        assert_eq!(
            set.get("gamma").map(|skill| skill.source),
            Some(Origin::HomeClaude)
        );
        assert_eq!(set.shadowed().len(), 1);
        assert!(set.errors().is_empty());
    }

    #[test]
    fn a_broken_skill_is_reported_without_stopping_the_others() {
        let project = tempfile::tempdir().expect("tempdir");
        let root = project.path().join(".claude/skills");
        write_skill(&root, "good", "");
        std::fs::create_dir_all(root.join("bad")).expect("mkdir");
        std::fs::write(
            root.join("bad").join(SKILL_FILE),
            "---\nname: bad\n---\nbody\n",
        )
        .expect("write");
        std::fs::create_dir_all(root.join("Bad-Name")).expect("mkdir");
        std::fs::write(
            root.join("Bad-Name").join(SKILL_FILE),
            "---\nname: Bad-Name\ndescription: x\n---\n",
        )
        .expect("write");

        let set = SkillSet::discover(&roots(project.path(), None));
        assert_eq!(set.names(), ["good"]);
        assert_eq!(set.errors().len(), 2);
        assert!(matches!(
            set.errors()[1].error,
            CompatError::MissingField {
                field: "description",
                ..
            }
        ));
        assert!(matches!(
            set.errors()[0].error,
            CompatError::Validation { .. }
        ));
    }

    #[test]
    fn prompt_section_lists_names_and_descriptions_only() {
        let project = tempfile::tempdir().expect("tempdir");
        write_skill(
            &project.path().join(".anvil/skills"),
            "browse",
            "requires: [playwright, Bash]\n",
        );
        let set = SkillSet::discover(&roots(project.path(), None));
        let section = set
            .prompt_section(&["shell", "read_file"])
            .expect("section");
        assert_eq!(section.source, "skills");
        assert!(
            section
                .text
                .contains("- browse: Does browse things. Use when asked.")
        );
        assert!(
            section
                .text
                .contains("requires `playwright`, not available")
        );
        assert!(!section.text.contains("Bash"), "{}", section.text);
        assert!(!section.text.contains(BODY_SENTINEL), "{}", section.text);
        insta::assert_snapshot!(section.text);

        let satisfied = set
            .prompt_section(&["shell", "mcp__playwright__navigate"])
            .expect("section");
        assert!(!satisfied.text.contains("not available"));
        assert!(SkillSet::default().prompt_section(&[]).is_none());
    }

    #[test]
    fn render_includes_body_files_and_args() {
        let project = tempfile::tempdir().expect("tempdir");
        write_skill(&project.path().join(".anvil/skills"), "alpha", "");
        let set = SkillSet::discover(&roots(project.path(), None));
        let skill = set.get("alpha").expect("alpha");
        assert_eq!(skill.files, ["REFERENCE.md", "scripts/run.py"]);
        let text = skill.render(Some("fast"));
        assert!(text.starts_with("# Skill: alpha\n\n# alpha\n\n"));
        assert!(text.contains(BODY_SENTINEL));
        assert!(text.contains("/scripts/run.py"));
        assert!(text.ends_with("## Arguments\n\nfast\n"));
    }

    #[test]
    fn slash_lines_expand_to_an_invocation_message() {
        let project = tempfile::tempdir().expect("tempdir");
        write_skill(&project.path().join(".anvil/skills"), "caveman", "");
        let set = SkillSet::discover(&roots(project.path(), None));
        let plain = set.expand_slash("/caveman").expect("expands");
        assert!(plain.contains("name \"caveman\""));
        let with_args = set.expand_slash("/caveman  be brief ").expect("expands");
        assert!(with_args.contains("args \"be brief\""));
        assert!(set.expand_slash("/nope").is_none());
        assert!(set.expand_slash("caveman without a slash").is_none());
        assert_eq!(set.help_lines().len(), 1);
        assert!(set.help_lines()[0].starts_with("/caveman"));
    }
}
