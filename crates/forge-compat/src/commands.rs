//! Slash commands: Markdown prompt templates in `commands/` directories.
//!
//! `/name args` sends the file's body as the user turn with `$ARGUMENTS`
//! replaced, exactly as Claude Code does for `.claude/commands/*.md`.

use std::path::{Path, PathBuf};

use crate::error::{CompatError, LoadError};
use crate::frontmatter;
use crate::paths::{Origin, config_dirs};

/// The subdirectory commands live in.
pub const COMMANDS_DIR: &str = "commands";

/// How deep below a `commands/` directory to look.
const MAX_DEPTH: usize = 3;

/// One slash command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashCommand {
    /// The word after the slash: the file stem.
    pub name: String,
    /// Frontmatter `description`, shown in `/help`.
    pub description: Option<String>,
    /// Frontmatter `argument-hint`, shown in `/help`.
    pub argument_hint: Option<String>,
    /// The prompt template.
    pub body: String,
    /// The file it was read from.
    pub path: PathBuf,
    /// Which directory it came from.
    pub origin: Origin,
}

impl SlashCommand {
    /// Fills the template with the text typed after the command.
    ///
    /// `$ARGUMENTS` takes the whole text and `$1` to `$9` take single
    /// words. When the template has no placeholder and there is text, it is
    /// appended under an `ARGUMENTS:` line so nothing the user typed is lost.
    ///
    /// # Arguments
    ///
    /// * `args` - What followed the command, possibly empty.
    ///
    /// # Returns
    ///
    /// The prompt to send.
    pub fn expand(&self, args: &str) -> String {
        let args = args.trim();
        let words: Vec<&str> = args.split_whitespace().collect();
        let has_placeholder = self.body.contains("$ARGUMENTS")
            || (1..=9).any(|n| self.body.contains(&format!("${n}")));
        let mut text = self.body.replace("$ARGUMENTS", args);
        for n in (1..=9).rev() {
            text = text.replace(&format!("${n}"), words.get(n - 1).copied().unwrap_or(""));
        }
        if !has_placeholder && !args.is_empty() {
            text = format!("{}\n\nARGUMENTS: {args}\n", text.trim_end());
        }
        text
    }

    /// One `/help` line.
    ///
    /// # Returns
    ///
    /// The name padded to a column, the argument hint and the description.
    pub fn help_line(&self) -> String {
        let hint = self
            .argument_hint
            .as_deref()
            .map(|hint| format!(" {hint}"))
            .unwrap_or_default();
        let description = self
            .description
            .as_deref()
            .map(|text| text.split_whitespace().collect::<Vec<_>>().join(" "))
            .unwrap_or_else(|| format!("({})", self.path.display()));
        format!("/{:<16} {description}", format!("{}{hint}", self.name))
    }
}

/// Every command found, deduplicated by name in directory priority order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandSet {
    /// Loaded commands, sorted by name.
    commands: Vec<SlashCommand>,
    /// Markdown files that did not parse.
    errors: Vec<LoadError>,
    /// Files in a `commands/` directory that are not Markdown (Claude Code
    /// plugins also ship `.toml` commands, which forge does not read).
    skipped: Vec<PathBuf>,
}

impl CommandSet {
    /// Walks the `commands/` directories and loads every `.md` file.
    ///
    /// # Arguments
    ///
    /// * `cwd` - The project root.
    /// * `home` - The home directory, if known.
    ///
    /// # Returns
    ///
    /// The set. Nothing here fails: broken files are listed in `errors`.
    pub fn discover(cwd: &Path, home: Option<&Path>) -> Self {
        let mut set = Self::default();
        for (dir, origin) in config_dirs(cwd, home, COMMANDS_DIR) {
            if !dir.is_dir() {
                continue;
            }
            let mut files = Vec::new();
            walk(&dir, 0, &mut files);
            files.sort();
            for path in files {
                if path.extension().is_none_or(|ext| ext != "md") {
                    set.skipped.push(path);
                    continue;
                }
                match load_command(&path, origin) {
                    Ok(command) => {
                        if set.get(&command.name).is_none() {
                            set.commands.push(command);
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%error, "skipping slash command");
                        set.errors.push(LoadError { path, error });
                    }
                }
            }
        }
        set.commands.sort_by(|a, b| a.name.cmp(&b.name));
        set
    }

    /// The loaded commands, sorted by name.
    ///
    /// # Returns
    ///
    /// The commands.
    pub fn commands(&self) -> &[SlashCommand] {
        &self.commands
    }

    /// Files that did not parse.
    ///
    /// # Returns
    ///
    /// The errors.
    pub fn errors(&self) -> &[LoadError] {
        &self.errors
    }

    /// Non-Markdown files found in `commands/` directories.
    ///
    /// # Returns
    ///
    /// Their paths.
    pub fn skipped(&self) -> &[PathBuf] {
        &self.skipped
    }

    /// Whether nothing loaded.
    ///
    /// # Returns
    ///
    /// `true` when there are no commands.
    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }

    /// Looks a command up by name.
    ///
    /// # Arguments
    ///
    /// * `name` - The word after the slash.
    ///
    /// # Returns
    ///
    /// The command, if loaded.
    pub fn get(&self, name: &str) -> Option<&SlashCommand> {
        self.commands.iter().find(|command| command.name == name)
    }

    /// Expands a `/<name> [args]` line.
    ///
    /// # Arguments
    ///
    /// * `line` - The user's line, starting with a slash.
    ///
    /// # Returns
    ///
    /// The prompt to send, or `None` when the line does not start with a
    /// slash or names no loaded command.
    pub fn expand_slash(&self, line: &str) -> Option<String> {
        let line = line.trim().strip_prefix('/')?;
        let (name, args) = match line.split_once(char::is_whitespace) {
            Some((name, args)) => (name, args),
            None => (line, ""),
        };
        Some(self.get(name)?.expand(args))
    }

    /// Help lines, one per command.
    ///
    /// # Returns
    ///
    /// The lines, sorted by name.
    pub fn help_lines(&self) -> Vec<String> {
        self.commands.iter().map(SlashCommand::help_line).collect()
    }
}

/// Reads one command file; frontmatter is optional.
pub(crate) fn load_command(path: &Path, origin: Origin) -> Result<SlashCommand, CompatError> {
    let text = std::fs::read_to_string(path).map_err(|error| CompatError::io(path, &error))?;
    let parsed = frontmatter::parse_optional(&text, path)?;
    let name = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok(SlashCommand {
        name,
        description: parsed.get_str("description").map(str::to_owned),
        argument_hint: parsed.get_str("argument-hint").map(str::to_owned),
        body: parsed.body,
        path: path.to_path_buf(),
        origin,
    })
}

/// Collects regular files below `dir`, skipping hidden entries.
fn walk(dir: &Path, depth: usize, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        if path.is_dir() {
            if depth + 1 < MAX_DEPTH {
                walk(&path, depth + 1, files);
            }
        } else if path.is_file() {
            files.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        std::fs::write(path, text).expect("write");
    }

    #[test]
    fn discovers_with_forge_precedence_and_reports_non_markdown() {
        let project = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        write(
            &project.path().join(".claude/commands/hello.md"),
            "---\ndescription: Say hello\nargument-hint: \"[name]\"\n---\nHello $ARGUMENTS!\n",
        );
        write(
            &project.path().join(".forge/commands/hello.md"),
            "Forge hello $ARGUMENTS\n",
        );
        write(
            &project.path().join(".claude/commands/frontend/lint.md"),
            "Lint it.\n",
        );
        write(
            &project.path().join(".claude/commands/legacy.toml"),
            "prompt = \"x\"\n",
        );
        write(
            &home.path().join(".claude/commands/review.md"),
            "Review $1 against $2.\n",
        );
        write(
            &home.path().join(".claude/commands/broken.md"),
            "---\n- not: [a map\n---\n",
        );

        let set = CommandSet::discover(project.path(), Some(home.path()));
        let names: Vec<&str> = set.commands().iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["hello", "lint", "review"]);
        let hello = set.get("hello").expect("hello");
        assert_eq!(hello.origin, Origin::ProjectForge);
        assert!(hello.description.is_none());
        assert_eq!(set.skipped().len(), 1);
        assert!(set.skipped()[0].ends_with("legacy.toml"));
        assert_eq!(set.errors().len(), 1);
        assert!(
            set.get("lint")
                .is_some_and(|c| c.origin == Origin::ProjectClaude)
        );
    }

    #[test]
    fn expands_arguments_positionals_and_appends_when_no_placeholder() {
        let command = |body: &str| SlashCommand {
            name: "x".to_owned(),
            description: None,
            argument_hint: None,
            body: body.to_owned(),
            path: PathBuf::from("x.md"),
            origin: Origin::ProjectClaude,
        };
        assert_eq!(
            command("Hello $ARGUMENTS!\n").expand("big world"),
            "Hello big world!\n"
        );
        assert_eq!(
            command("Review $1 against $2.").expand("main dev extra"),
            "Review main against dev."
        );
        assert_eq!(command("Do $1.").expand(""), "Do .");
        assert_eq!(
            command("Fixed prompt.\n").expand("with args"),
            "Fixed prompt.\n\nARGUMENTS: with args\n"
        );
        assert_eq!(command("Fixed prompt.\n").expand("  "), "Fixed prompt.\n");
    }

    #[test]
    fn slash_lines_expand_and_help_lists_descriptions() {
        let project = tempfile::tempdir().expect("tempdir");
        write(
            &project.path().join(".claude/commands/hello.md"),
            "---\ndescription: Say hello\nargument-hint: \"[name]\"\n---\nHello $ARGUMENTS!\n",
        );
        let set = CommandSet::discover(project.path(), None);
        assert_eq!(
            set.expand_slash("/hello  Eli ").as_deref(),
            Some("Hello Eli!\n")
        );
        assert_eq!(set.expand_slash("/hello").as_deref(), Some("Hello !\n"));
        assert!(set.expand_slash("/nope").is_none());
        assert!(set.expand_slash("hello").is_none());
        assert_eq!(set.help_lines(), ["/hello [name]     Say hello"]);
    }
}
