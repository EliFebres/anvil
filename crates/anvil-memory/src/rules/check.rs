//! Runs the rule checks on changed files.
//!
//! A `regex` check fails when the pattern matches any line of a changed
//! file; the failure lists `path:line: text`. A `command` check runs the
//! program with the changed files appended as arguments and fails on a
//! non-zero exit; the failure carries the tail of its output. Each check
//! sees only existing files whose language the rule lists.

use std::path::{Path, PathBuf};
use std::process::Command;

use super::{ANY_LANGUAGE, Check, Rule, RuleSet};

/// Lines of command output kept in a failure.
const OUTPUT_TAIL_LINES: usize = 80;

/// Lines a regex failure lists before cutting.
const MAX_REGEX_HITS: usize = 50;

/// What a failed check reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckFailure {
    /// The rule whose check failed.
    pub rule_id: String,
    /// The matches or the command output.
    pub output: String,
}

/// The language tag of a file, by extension.
///
/// # Arguments
///
/// * `path` - The file.
///
/// # Returns
///
/// A tag such as `python` or `rust`, or `None` for an unknown extension.
pub fn language_of(path: &Path) -> Option<&'static str> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    let language = match extension.as_str() {
        "py" | "pyi" => "python",
        "rs" => "rust",
        "ts" | "tsx" => "typescript",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "go" => "go",
        "java" => "java",
        "rb" => "ruby",
        "sh" | "bash" => "shell",
        "toml" => "toml",
        "md" => "markdown",
        _ => return None,
    };
    Some(language)
}

impl Rule {
    /// Whether the rule's language list covers a file.
    ///
    /// # Arguments
    ///
    /// * `path` - The file.
    ///
    /// # Returns
    ///
    /// `true` for `*`, or when the file's language is listed.
    pub fn applies_to(&self, path: &Path) -> bool {
        let language = language_of(path);
        self.languages.iter().any(|listed| {
            listed == ANY_LANGUAGE
                || language.is_some_and(|found| listed.eq_ignore_ascii_case(found))
        })
    }
}

/// Runs every rule that has a check on the changed files.
///
/// # Arguments
///
/// * `rules` - The rules.
/// * `cwd` - The project root; commands run here and paths are shown relative to it.
/// * `home` - The home directory, for `~` in commands.
/// * `changed` - The files the session changed; missing files are skipped.
///
/// # Returns
///
/// One failure per rule whose check failed, in rule order.
pub fn run_checks(
    rules: &RuleSet,
    cwd: &Path,
    home: Option<&Path>,
    changed: &[PathBuf],
) -> Vec<CheckFailure> {
    let mut failures = Vec::new();
    for rule in &rules.rules {
        let Some(check) = &rule.check else {
            continue;
        };
        let files: Vec<&Path> = changed
            .iter()
            .map(PathBuf::as_path)
            .filter(|path| path.is_file() && rule.applies_to(path))
            .collect();
        if files.is_empty() {
            tracing::debug!(rule = rule.id, "no changed files match; check skipped");
            continue;
        }
        let output = match check {
            Check::Regex(pattern) => run_regex(pattern, cwd, &files),
            Check::Command(command) => run_command(command, cwd, home, &files),
        };
        if let Some(output) = output {
            failures.push(CheckFailure {
                rule_id: rule.id.clone(),
                output,
            });
        }
    }
    failures
}

/// Lines of the files that match the pattern, or `None` when none do.
fn run_regex(pattern: &str, cwd: &Path, files: &[&Path]) -> Option<String> {
    let regex = match regex::Regex::new(pattern) {
        Ok(regex) => regex,
        Err(error) => return Some(format!("invalid regex `{pattern}`: {error}")),
    };
    let mut hits = Vec::new();
    for file in files {
        let Ok(text) = std::fs::read_to_string(file) else {
            tracing::debug!(path = %file.display(), "unreadable; skipped by the regex check");
            continue;
        };
        for (index, line) in text.lines().enumerate() {
            if regex.is_match(line) {
                hits.push(format!(
                    "{}:{}: {}",
                    display(file, cwd),
                    index + 1,
                    line.trim()
                ));
            }
        }
    }
    if hits.is_empty() {
        return None;
    }
    let total = hits.len();
    hits.truncate(MAX_REGEX_HITS);
    if total > MAX_REGEX_HITS {
        hits.push(format!("... and {} more", total - MAX_REGEX_HITS));
    }
    Some(hits.join("\n"))
}

/// Runs the command with the files appended, relative to `cwd` so the
/// output stays short; its output when it fails.
fn run_command(command: &str, cwd: &Path, home: Option<&Path>, files: &[&Path]) -> Option<String> {
    let mut tokens = split_command(command)
        .into_iter()
        .map(|token| expand_home(&token, home))
        .collect::<Vec<_>>();
    if tokens.is_empty() {
        return Some("the command is empty".to_owned());
    }
    let program = tokens.remove(0);
    let output = Command::new(&program)
        .args(&tokens)
        .args(files.iter().map(|file| display(file, cwd)))
        .current_dir(cwd)
        .output();
    match output {
        Ok(output) if output.status.success() => None,
        Ok(output) => {
            let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&output.stderr));
            let tail = tail_lines(&text, OUTPUT_TAIL_LINES);
            Some(if tail.trim().is_empty() {
                format!("`{command}` exited with {}", output.status)
            } else {
                tail
            })
        }
        Err(error) => Some(format!("could not run `{program}`: {error}")),
    }
}

/// Splits a command line on whitespace, keeping double-quoted spans together.
///
/// # Arguments
///
/// * `text` - The command line.
///
/// # Returns
///
/// The tokens without their quotes.
pub fn split_command(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut pending = false;
    for c in text.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                pending = true;
            }
            c if c.is_whitespace() && !quoted => {
                if pending {
                    tokens.push(std::mem::take(&mut current));
                    pending = false;
                }
            }
            c => {
                current.push(c);
                pending = true;
            }
        }
    }
    if pending {
        tokens.push(current);
    }
    tokens
}

/// Replaces a leading `~` with the home directory.
fn expand_home(token: &str, home: Option<&Path>) -> String {
    let Some(home) = home else {
        return token.to_owned();
    };
    match token.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') || rest.starts_with('\\') => {
            format!("{}{rest}", home.display())
        }
        _ => token.to_owned(),
    }
}

/// The path relative to `cwd` with forward slashes, or as given.
fn display(path: &Path, cwd: &Path) -> String {
    path.strip_prefix(cwd)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// The last `count` lines of `text`.
fn tail_lines(text: &str, count: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(count);
    lines[start..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(id: &str, languages: &[&str], check: Check) -> Rule {
        Rule {
            id: id.to_owned(),
            languages: languages.iter().map(|l| (*l).to_owned()).collect(),
            rule: "x".to_owned(),
            check: Some(check),
        }
    }

    #[test]
    fn languages_and_applicability() {
        assert_eq!(language_of(Path::new("a/b.PY")), Some("python"));
        assert_eq!(language_of(Path::new("lib.rs")), Some("rust"));
        assert_eq!(language_of(Path::new("Makefile")), None);
        let python = rule("p", &["Python"], Check::Regex("x".to_owned()));
        assert!(python.applies_to(Path::new("x.py")));
        assert!(!python.applies_to(Path::new("x.rs")));
        assert!(!python.applies_to(Path::new("Makefile")));
        let any = rule("a", &["*"], Check::Regex("x".to_owned()));
        assert!(any.applies_to(Path::new("Makefile")));
    }

    #[test]
    fn regex_checks_report_path_and_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).expect("mkdir");
        let file = src.join("main.rs");
        std::fs::write(&file, "fn a() {}\n// TODO later\nfn b() {} // TODO\n").expect("write");
        let clean = dir.path().join("clean.rs");
        std::fs::write(&clean, "fn c() {}\n").expect("write");
        let set = RuleSet {
            rules: vec![
                rule("no-todo", &["rust"], Check::Regex("TODO".to_owned())),
                rule("py-only", &["python"], Check::Regex(".".to_owned())),
            ],
        };
        let changed = vec![file.clone(), clean.clone(), dir.path().join("gone.rs")];
        let failures = run_checks(&set, dir.path(), None, &changed);
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert_eq!(failures[0].rule_id, "no-todo");
        assert_eq!(
            failures[0].output,
            "src/main.rs:2: // TODO later\nsrc/main.rs:3: fn b() {} // TODO"
        );
        assert!(run_checks(&set, dir.path(), None, &[clean]).is_empty());
    }

    #[test]
    fn command_checks_fail_on_nonzero_exit_or_missing_program() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("a.md");
        std::fs::write(&file, "x").expect("write");
        let set = RuleSet {
            rules: vec![rule(
                "ghost",
                &["*"],
                Check::Command("anvil-no-such-program-xyz --flag".to_owned()),
            )],
        };
        let failures = run_checks(&set, dir.path(), None, &[file]);
        assert_eq!(failures.len(), 1);
        assert!(
            failures[0]
                .output
                .starts_with("could not run `anvil-no-such-program-xyz`"),
            "{}",
            failures[0].output
        );
    }

    #[test]
    fn command_splitting_and_home_expansion() {
        assert_eq!(
            split_command("python \"C:\\My Tools\\check.py\" --strict  x"),
            vec!["python", "C:\\My Tools\\check.py", "--strict", "x"]
        );
        assert_eq!(split_command("  "), Vec::<String>::new());
        assert_eq!(split_command("a \"\" b"), vec!["a", "", "b"]);
        let home = Path::new("/home/me");
        assert_eq!(
            expand_home("~/.anvil/checks/x.py", Some(home)),
            "/home/me/.anvil/checks/x.py"
        );
        assert_eq!(expand_home("~", Some(home)), "/home/me");
        assert_eq!(expand_home("~user/x", Some(home)), "~user/x");
        assert_eq!(expand_home("~/x", None), "~/x");
        assert_eq!(tail_lines("a\nb\nc", 2), "b\nc");
    }
}
