//! `anvil compat check`: what a `.claude` folder (or a plugin repository)
//! would load in anvil, what it would not, and why.
//!
//! Every loader in this crate is run against the folder in reporting mode:
//! nothing is installed or registered, and nothing connects to a server.

use std::fmt;
use std::path::{Path, PathBuf};

use anvil_types::AnvilConfig;
use serde_json::Value;

use crate::agents::load_agent;
use crate::commands::load_command;
use crate::hooks::{HookConfig, HookEvent};
use crate::instructions::find_imports;
use crate::mcp::{self, Transport};
use crate::names::to_anvil_name;
use crate::paths::Origin;
use crate::skills::{find_skill_files, load_skill};

/// Instruction files checked at the root.
const INSTRUCTION_FILES: &[&str] = &["CLAUDE.md", "CLAUDE.local.md", "ANVIL.md"];

/// Settings files checked at the root.
const SETTINGS_FILES: &[&str] = &["settings.json", "settings.local.json"];

/// Top-level settings keys anvil reads.
const USED_SETTINGS_KEYS: &[&str] = &["hooks", "mcpServers"];

/// Whether an item would load.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Status {
    /// anvil loads it as is.
    Loads,
    /// anvil ignores it; the detail says why and what to do instead.
    Skipped,
    /// anvil would try to load it and fail.
    Error,
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(match self {
            Self::Loads => "LOADS",
            Self::Skipped => "SKIP",
            Self::Error => "ERROR",
        })
    }
}

/// One finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// Whether it would load.
    pub status: Status,
    /// What kind of thing it is: `skill`, `command`, `agent`, `hook`, ...
    pub kind: &'static str,
    /// The path relative to the checked folder, with forward slashes.
    pub path: String,
    /// What was found or why it was skipped.
    pub detail: String,
}

/// The whole report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// The folder that was checked.
    pub root: PathBuf,
    /// Findings in discovery order.
    pub items: Vec<Item>,
}

impl Report {
    /// Whether anything would fail to load.
    ///
    /// # Returns
    ///
    /// `true` when at least one item is an error.
    pub fn has_errors(&self) -> bool {
        self.items.iter().any(|item| item.status == Status::Error)
    }

    /// How many items have each status.
    ///
    /// # Returns
    ///
    /// Counts of loads, skipped and errors.
    pub fn counts(&self) -> (usize, usize, usize) {
        let count = |status| {
            self.items
                .iter()
                .filter(|item| item.status == status)
                .count()
        };
        (
            count(Status::Loads),
            count(Status::Skipped),
            count(Status::Error),
        )
    }

    /// Renders the report as aligned text lines.
    ///
    /// # Returns
    ///
    /// The text, ending with a summary line.
    pub fn render(&self) -> String {
        let mut out = format!("compat check: {}\n", self.root.display());
        if self.items.is_empty() {
            out.push_str("nothing anvil reads was found here\n");
        }
        let kind_width = self
            .items
            .iter()
            .map(|item| item.kind.len())
            .max()
            .unwrap_or(0);
        let path_width = self
            .items
            .iter()
            .map(|item| item.path.chars().count())
            .max()
            .unwrap_or(0)
            .min(60);
        for item in &self.items {
            out.push_str(&format!(
                "{:<5}  {:<kind_width$}  {:<path_width$}  {}\n",
                item.status, item.kind, item.path, item.detail
            ));
        }
        let (loads, skipped, errors) = self.counts();
        out.push_str(&format!(
            "{loads} would load, {skipped} skipped, {errors} error(s)\n"
        ));
        out
    }
}

/// Checks a folder.
///
/// # Arguments
///
/// * `root` - A `.claude` folder, a project root or a plugin repository.
/// * `config` - The anvil config, when available, to resolve agent models.
/// * `available_tools` - Registered tool names, to check skill `requires`.
///
/// # Returns
///
/// The report.
pub fn check(root: &Path, config: Option<&AnvilConfig>, available_tools: &[&str]) -> Report {
    let mut report = Report {
        root: root.to_path_buf(),
        items: Vec::new(),
    };
    let mut ctx = Checker {
        root,
        config,
        available_tools,
        items: &mut report.items,
    };
    ctx.instruction_files();
    ctx.skills();
    ctx.commands();
    ctx.agents();
    ctx.settings();
    ctx.mcp_json();
    ctx.plugin_manifest();
    ctx.plugins_dir();
    report
}

/// The state shared by the individual checks.
struct Checker<'a> {
    /// The folder.
    root: &'a Path,
    /// The anvil config, if any.
    config: Option<&'a AnvilConfig>,
    /// Registered tool names.
    available_tools: &'a [&'a str],
    /// Where findings go.
    items: &'a mut Vec<Item>,
}

impl Checker<'_> {
    /// Records one finding.
    fn push(&mut self, status: Status, kind: &'static str, path: &Path, detail: impl Into<String>) {
        self.items.push(Item {
            status,
            kind,
            path: self.relative(path),
            detail: detail.into(),
        });
    }

    /// A path relative to the root with forward slashes.
    fn relative(&self, path: &Path) -> String {
        path.strip_prefix(self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }

    /// `CLAUDE.md` and friends, with their imports.
    fn instruction_files(&mut self) {
        for name in INSTRUCTION_FILES {
            let path = self.root.join(name);
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            if text.trim().is_empty() {
                self.push(Status::Skipped, "instructions", &path, "empty file");
                continue;
            }
            let imports = find_imports(&text);
            let detail = match imports.len() {
                0 => format!("{} lines", text.lines().count()),
                n => format!("{} lines, {n} @import(s)", text.lines().count()),
            };
            self.push(Status::Loads, "instructions", &path, detail);
            for import in imports {
                let resolved = if let Some(rest) = import.strip_prefix("~/") {
                    std::env::home_dir().map(|home| home.join(rest))
                } else {
                    Some(self.root.join(&import))
                };
                match resolved {
                    Some(target) if target.is_file() => {
                        self.push(Status::Loads, "import", &path, format!("@{import}"));
                    }
                    _ => self.push(
                        Status::Skipped,
                        "import",
                        &path,
                        format!("@{import} does not exist; the import is ignored"),
                    ),
                }
            }
        }
    }

    /// `skills/**/SKILL.md`.
    fn skills(&mut self) {
        let dir = self.root.join("skills");
        if !dir.is_dir() {
            return;
        }
        let files = find_skill_files(&dir);
        if files.is_empty() {
            self.push(
                Status::Skipped,
                "skill",
                &dir,
                "no SKILL.md found up to three levels down",
            );
        }
        for file in files {
            match load_skill(&file, Origin::ProjectClaude) {
                Ok(skill) => {
                    let mut notes = vec![format!(
                        "`{}`, {} supporting file(s)",
                        skill.name(),
                        skill.files.len()
                    )];
                    if !skill.manifest.allowed_tools.is_empty() {
                        let mapped: Vec<String> = skill
                            .manifest
                            .allowed_tools
                            .iter()
                            .map(|tool| to_anvil_name(tool))
                            .collect();
                        notes.push(format!("allowed-tools -> {}", mapped.join(", ")));
                    }
                    let missing = skill.missing_requires(self.available_tools);
                    if !missing.is_empty() {
                        notes.push(format!(
                            "requires {} (not available here; the skill still loads)",
                            missing.join(", ")
                        ));
                    }
                    if !skill.manifest.extra.is_empty() {
                        let keys: Vec<&str> =
                            skill.manifest.extra.keys().map(String::as_str).collect();
                        notes.push(format!("extra keys kept: {}", keys.join(", ")));
                    }
                    self.push(Status::Loads, "skill", &file, notes.join("; "));
                }
                Err(error) => self.push(
                    Status::Error,
                    "skill",
                    &file,
                    strip_path(&error.to_string()),
                ),
            }
        }
    }

    /// `commands/**`.
    fn commands(&mut self) {
        let dir = self.root.join("commands");
        if !dir.is_dir() {
            return;
        }
        let mut files = Vec::new();
        collect_files(&dir, &mut files);
        files.sort();
        if files.is_empty() {
            self.push(Status::Skipped, "command", &dir, "empty directory");
        }
        for file in files {
            if file.extension().is_none_or(|ext| ext != "md") {
                let detail = if file.extension().is_some_and(|ext| ext == "toml") {
                    "TOML commands are not supported; convert to Markdown with a `description` frontmatter"
                } else {
                    "only Markdown commands are read"
                };
                self.push(Status::Skipped, "command", &file, detail);
                continue;
            }
            match load_command(&file, Origin::ProjectClaude) {
                Ok(command) => {
                    let description = command
                        .description
                        .clone()
                        .unwrap_or_else(|| "(no description; /help shows the path)".to_owned());
                    let placeholder = if command.body.contains("$ARGUMENTS") {
                        ", uses $ARGUMENTS"
                    } else {
                        ""
                    };
                    self.push(
                        Status::Loads,
                        "command",
                        &file,
                        format!("/{}: {}{placeholder}", command.name, one_line(&description)),
                    );
                }
                Err(error) => {
                    self.push(
                        Status::Error,
                        "command",
                        &file,
                        strip_path(&error.to_string()),
                    );
                }
            }
        }
    }

    /// `agents/*.md`; other files are summarized.
    fn agents(&mut self) {
        let dir = self.root.join("agents");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return;
        };
        let mut markdown = Vec::new();
        let mut other = 0usize;
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_file() && path.extension().is_some_and(|ext| ext == "md") {
                markdown.push(path);
            } else {
                other += 1;
            }
        }
        markdown.sort();
        for file in markdown {
            match load_agent(&file, Origin::ProjectClaude) {
                Ok(agent) => {
                    let mut notes = vec![format!("`{}`", agent.name)];
                    match &agent.tools {
                        Some(tools) => notes.push(format!("tools: {}", tools.join(", "))),
                        None => notes.push("all tools".to_owned()),
                    }
                    if let Some(model) = &agent.model {
                        notes.push(self.describe_model(model));
                    }
                    self.push(Status::Loads, "agent", &file, notes.join("; "));
                }
                Err(error) => self.push(
                    Status::Error,
                    "agent",
                    &file,
                    strip_path(&error.to_string()),
                ),
            }
        }
        match other {
            0 => {}
            1 => self.push(
                Status::Skipped,
                "agent",
                &dir,
                "1 entry that is not a Markdown agent file is ignored",
            ),
            n => self.push(
                Status::Skipped,
                "agent",
                &dir,
                format!("{n} entries that are not Markdown agent files are ignored"),
            ),
        }
    }

    /// How anvil would resolve an agent's `model`.
    fn describe_model(&self, model: &str) -> String {
        let Some(config) = self.config else {
            return format!("model `{model}` (no anvil config here to resolve it against)");
        };
        let is_role = matches!(model, "coder" | "reviewer" | "summarizer");
        if is_role || config.providers.contains_key(model) {
            format!("model `{model}`")
        } else {
            format!(
                "model `{model}` is not a role or provider in the anvil config; the coder `{}` would be used",
                config.roles.coder
            )
        }
    }

    /// `settings.json` and `settings.local.json`.
    fn settings(&mut self) {
        for name in SETTINGS_FILES {
            let path = self.root.join(name);
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let settings: Value = match serde_json::from_str(&text) {
                Ok(settings) => settings,
                Err(error) => {
                    self.push(
                        Status::Error,
                        "settings",
                        &path,
                        format!("not valid JSON: {error}"),
                    );
                    continue;
                }
            };
            if let Some(hooks) = settings.get("hooks") {
                self.hooks(hooks, &path, true);
            }
            if let Some(servers) = settings.get("mcpServers") {
                self.mcp_servers(&text, &path, servers);
            }
            let unused: Vec<&str> = settings
                .as_object()
                .map(|object| {
                    object
                        .keys()
                        .map(String::as_str)
                        .filter(|key| !USED_SETTINGS_KEYS.contains(key))
                        .collect()
                })
                .unwrap_or_default();
            if !unused.is_empty() {
                self.push(
                    Status::Skipped,
                    "settings",
                    &path,
                    format!("keys anvil does not read: {}", unused.join(", ")),
                );
            }
        }
    }

    /// One `hooks` object; `runs` says whether anvil reads hooks from here.
    fn hooks(&mut self, hooks: &Value, path: &Path, runs: bool) {
        let mut config = HookConfig::default();
        config.add(hooks, path);
        for event in config.events() {
            let commands: usize = config.groups(event).iter().map(|g| g.commands.len()).sum();
            let matchers: Vec<String> = config
                .groups(event)
                .iter()
                .filter_map(|group| group.matcher.clone())
                .collect();
            let mut detail = format!("{event}: {commands} command(s)");
            if !matchers.is_empty() {
                detail.push_str(&format!(" (matchers: {})", matchers.join(", ")));
            }
            if runs {
                self.push(Status::Loads, "hook", path, detail);
            } else {
                detail.push_str(
                    "; hooks in a plugin manifest are not run, copy them to .claude/settings.json",
                );
                self.push(Status::Skipped, "hook", path, detail);
            }
        }
        for (name, _) in config.unsupported() {
            self.push(
                Status::Skipped,
                "hook",
                path,
                format!(
                    "{name}: not supported (anvil runs {} command hooks)",
                    [
                        HookEvent::PreToolUse,
                        HookEvent::PostToolUse,
                        HookEvent::Stop,
                        HookEvent::SessionStart,
                    ]
                    .map(|event| event.as_str())
                    .join(", ")
                ),
            );
        }
    }

    /// `mcpServers` in a settings file or `.mcp.json`.
    fn mcp_servers(&mut self, text: &str, path: &Path, raw: &Value) {
        match mcp::parse_definitions(text, path) {
            Ok(servers) => {
                let defined: Vec<&str> = raw
                    .as_object()
                    .map(|object| object.keys().map(String::as_str).collect())
                    .unwrap_or_default();
                for name in defined {
                    match servers.iter().find(|server| server.name == name) {
                        Some(server) => {
                            let transport = match &server.transport {
                                Transport::Stdio { command, args, .. } => {
                                    format!("stdio: {command} {}", args.join(" "))
                                }
                                Transport::Http { url, .. } => format!("http: {url}"),
                            };
                            self.push(
                                Status::Loads,
                                "mcp",
                                path,
                                format!("`{name}` ({}); tools register as mcp__{name}__*", transport.trim_end()),
                            );
                        }
                        None => self.push(
                            Status::Error,
                            "mcp",
                            path,
                            format!("`{name}`: needs a `command` or an http `url`; sse-only servers are not supported"),
                        ),
                    }
                }
            }
            Err(error) => self.push(Status::Error, "mcp", path, strip_path(&error.to_string())),
        }
    }

    /// `.mcp.json` at the root.
    fn mcp_json(&mut self) {
        let path = self.root.join(".mcp.json");
        let Ok(text) = std::fs::read_to_string(&path) else {
            return;
        };
        match serde_json::from_str::<Value>(&text) {
            Ok(value) => {
                let servers = value.get("mcpServers").cloned().unwrap_or(Value::Null);
                self.mcp_servers(&text, &path, &servers);
            }
            Err(error) => self.push(
                Status::Error,
                "mcp",
                &path,
                format!("not valid JSON: {error}"),
            ),
        }
    }

    /// `.claude-plugin/plugin.json`: reported, never loaded.
    fn plugin_manifest(&mut self) {
        let path = self.root.join(".claude-plugin").join("plugin.json");
        let Ok(text) = std::fs::read_to_string(&path) else {
            return;
        };
        let manifest: Value = match serde_json::from_str(&text) {
            Ok(manifest) => manifest,
            Err(error) => {
                self.push(
                    Status::Error,
                    "plugin",
                    &path,
                    format!("not valid JSON: {error}"),
                );
                return;
            }
        };
        let name = manifest
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("unnamed");
        self.push(
            Status::Skipped,
            "plugin",
            &path,
            format!(
                "plugin `{name}`: anvil does not install plugins; its skills/, commands/ and agents/ above are checked as plain folders"
            ),
        );
        match manifest.get("hooks") {
            Some(Value::Object(_)) => {
                let hooks = manifest.get("hooks").cloned().unwrap_or(Value::Null);
                let hooks = hooks.get("hooks").cloned().unwrap_or(hooks);
                self.hooks(&hooks, &path, false);
            }
            Some(Value::String(file)) => self.push(
                Status::Skipped,
                "hook",
                &path,
                format!("hooks file `{file}` belongs to the plugin and is not run"),
            ),
            _ => {}
        }
        if let Some(servers) = manifest.get("mcpServers") {
            self.push(
                Status::Skipped,
                "mcp",
                &path,
                format!(
                    "{} MCP server(s) in the plugin manifest are not read; copy them to .mcp.json",
                    servers.as_object().map_or(0, serde_json::Map::len)
                ),
            );
        }
    }

    /// `plugins/`: Claude Code's installed plugins.
    fn plugins_dir(&mut self) {
        let dir = self.root.join("plugins");
        if !dir.is_dir() {
            return;
        }
        let installed = std::fs::read_to_string(dir.join("installed_plugins.json"))
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|value| {
                value
                    .get("plugins")
                    .and_then(Value::as_object)
                    .map(|plugins| plugins.keys().cloned().collect::<Vec<_>>())
            })
            .unwrap_or_default();
        let detail = if installed.is_empty() {
            "plugins are not supported; install a plugin's skills with `anvil skill add owner/repo/skills/<name>`".to_owned()
        } else {
            format!(
                "plugins are not supported ({} installed: {}); install their skills with `anvil skill add`",
                installed.len(),
                installed.join(", ")
            )
        };
        self.push(Status::Skipped, "plugin", &dir, detail);
    }
}

/// Regular files below `dir`, recursively, skipping hidden entries.
fn collect_files(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        if path.is_dir() {
            collect_files(&path, files);
        } else if path.is_file() {
            files.push(path);
        }
    }
}

/// Drops the `<path>: ` prefix our errors carry, since the row shows the path.
fn strip_path(message: &str) -> String {
    match message.split_once(": ") {
        Some((prefix, rest)) if prefix.contains(['/', '\\']) || prefix.ends_with(".md") => {
            rest.to_owned()
        }
        _ => message.to_owned(),
    }
}

/// Collapses whitespace and caps the length.
fn one_line(text: &str) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if joined.chars().count() <= 90 {
        joined
    } else {
        format!("{}...", joined.chars().take(87).collect::<String>())
    }
}
