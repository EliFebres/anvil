//! Every program anvil starts begins from a cleared environment refilled
//! from `anvil_types::child_env::vars()`, so a provider key in the user's
//! shell never reaches it. This test fails when non-test code builds a
//! `Command` (under any alias) whose own builder chain, or whose variable,
//! does not call `env_clear()` before the program starts or the builder
//! leaves the scope, and when anything passes the parent's environment back
//! with `.envs(std::env::vars())`. Comments and string literals are blanked
//! first, so neither can hide or fake a call.

use std::path::{Path, PathBuf};

/// Reviewed places that build a command without clearing it, as (file,
/// trimmed line, reason). Empty today; an entry needs a reason a reviewer
/// can check.
const EXCEPTIONS: [(&str, &str, &str); 0] = [];

/// Builder methods that start the program.
const STARTS: [&str; 3] = [".spawn(", ".output(", ".status("];

/// The text before the first inline test module: `#[cfg(test)]` followed
/// by `mod name {`. A `#[cfg(test)] mod name;` declaration near the top of
/// a file does not end the code, so the rest of the file is still checked.
fn non_test_code(text: &str) -> &str {
    const MARK: &str = "#[cfg(test)]";
    let mut from = 0;
    while let Some(found) = text[from..].find(MARK) {
        let at = from + found;
        let after = text[at + MARK.len()..].trim_start();
        let line = after.lines().next().unwrap_or_default();
        if line.starts_with("mod ") && line.contains('{') {
            return &text[..at];
        }
        from = at + MARK.len();
    }
    text
}

fn rust_files(dir: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            if path
                .file_name()
                .is_some_and(|name| name != "target" && name != "tests")
            {
                rust_files(&path, found);
            }
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            found.push(path);
        }
    }
}

/// Whether `c` can be part of an identifier.
fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// `code` with comments, string literals and character literals replaced
/// by spaces, line breaks kept, so offsets and line numbers still match.
fn strip(code: &str) -> String {
    let chars: Vec<char> = code.chars().collect();
    let blank = |c: char| if c == '\n' { '\n' } else { ' ' };
    let mut out = String::with_capacity(code.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        let after_ident = i > 0 && is_ident(chars[i - 1]);
        let end = if c == '/' && next == Some('/') {
            chars[i..]
                .iter()
                .position(|&ch| ch == '\n')
                .map_or(chars.len(), |p| i + p)
        } else if c == '/' && next == Some('*') {
            let mut depth = 0usize;
            let mut k = i;
            while k < chars.len() {
                if chars[k] == '/' && chars.get(k + 1) == Some(&'*') {
                    depth += 1;
                    k += 2;
                } else if chars[k] == '*' && chars.get(k + 1) == Some(&'/') {
                    depth -= 1;
                    k += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    k += 1;
                }
            }
            k.min(chars.len())
        } else if c == 'r'
            && matches!(next, Some('"' | '#'))
            && (!after_ident || (i == 1 || !is_ident(chars[i - 2])) && chars[i - 1] == 'b')
        {
            let hashes = chars[i + 1..].iter().take_while(|&&ch| ch == '#').count();
            if chars.get(i + 1 + hashes) == Some(&'"') {
                let mut k = i + 2 + hashes;
                while k < chars.len()
                    && !(chars[k] == '"' && (0..hashes).all(|h| chars.get(k + 1 + h) == Some(&'#')))
                {
                    k += 1;
                }
                (k + 1 + hashes).min(chars.len())
            } else {
                i + 1
            }
        } else if c == '"' {
            let mut k = i + 1;
            while k < chars.len() && chars[k] != '"' {
                k += if chars[k] == '\\' { 2 } else { 1 };
            }
            (k + 1).min(chars.len())
        } else if c == '\'' && next == Some('\\') {
            chars
                .get(i + 3..)
                .and_then(|rest| rest.iter().position(|&ch| ch == '\''))
                .map_or(i + 1, |p| i + 4 + p)
        } else if c == '\'' && chars.get(i + 2) == Some(&'\'') {
            i + 3
        } else {
            out.push(c);
            i += 1;
            continue;
        };
        out.extend(chars[i..end].iter().map(|&ch| blank(ch)));
        i = end.max(i + 1);
    }
    out
}

/// The names `code` uses for `Command`: `Command` itself and every alias
/// from `use ...::Command as X` or `type X = ...Command;`.
fn command_names(code: &str) -> Vec<String> {
    let mut names = vec!["Command".to_owned()];
    for statement in code.split(';') {
        let words: Vec<&str> = statement.split_whitespace().collect();
        for (at, word) in words.iter().enumerate() {
            let renamed = *word == "as" && at > 0 && words[at - 1].ends_with("Command");
            let typed = *word == "type"
                && words.get(at + 2) == Some(&"=")
                && words.get(at + 3).is_some_and(|ty| ty.ends_with("Command"));
            if (renamed || typed)
                && let Some(alias) = words.get(at + 1)
            {
                names.push(alias.trim_matches(|c: char| !is_ident(c)).to_owned());
            }
        }
    }
    names
}

/// Offsets in `code` where a builder is made: `NAME::new(` for each name,
/// not preceded by an identifier character.
fn sites(code: &str, names: &[String]) -> Vec<usize> {
    let mut found: Vec<usize> = names
        .iter()
        .flat_map(|name| {
            let pattern = format!("{name}::new(");
            code.match_indices(&pattern)
                .map(|(at, _)| at)
                .filter(|&at| !code[..at].chars().next_back().is_some_and(is_ident))
                .collect::<Vec<_>>()
        })
        .collect();
    found.sort_unstable();
    found.dedup();
    found
}

/// Where the expression starting at `from` ends: a `;` or `,` at its own
/// depth, or the bracket that closes the one around it.
fn statement_end(code: &str, from: usize) -> usize {
    let mut depth = 0i32;
    for (offset, c) in code[from..].char_indices() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => {
                depth -= 1;
                if depth < 0 {
                    return from + offset;
                }
            }
            ';' | ',' if depth == 0 => return from + offset,
            _ => {}
        }
    }
    code.len()
}

/// `text` without white space.
fn flat(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Whether a builder chain starts its program.
fn starts(chain: &str) -> bool {
    let chain = flat(chain);
    STARTS.iter().any(|start| chain.contains(start))
}

/// Whether a builder chain clears the environment before it starts the
/// program, if it starts it at all.
fn clears_first(chain: &str) -> bool {
    let chain = flat(chain);
    let Some(clear) = chain.find(".env_clear()") else {
        return false;
    };
    STARTS
        .iter()
        .filter_map(|start| chain.find(start))
        .all(|start| clear < start)
}

/// The variable a builder made at `site` is bound to, for `let [mut] x =
/// Command::new(...)`.
fn bound_name(code: &str, site: usize) -> Option<String> {
    let start = code[..site].rfind([';', '{', '}']).map_or(0, |at| at + 1);
    // A path such as `tokio::process::` may stand before `Command::new(`.
    let head = code[start..site]
        .trim_end_matches(|c: char| is_ident(c) || c == ':')
        .trim();
    let head = head.strip_prefix("let ")?.trim_start();
    let head = head.strip_prefix("mut ").unwrap_or(head);
    let head = head.strip_suffix('=')?;
    let name = head.split(':').next()?.trim();
    (!name.is_empty() && name.chars().all(is_ident)).then(|| name.to_owned())
}

/// Whether `text` names the identifier `name`.
fn mentions(text: &str, name: &str) -> bool {
    text.match_indices(name).any(|(at, _)| {
        !text[..at].chars().next_back().is_some_and(is_ident)
            && !text[at + name.len()..].chars().next().is_some_and(is_ident)
    })
}

/// Whether the builder in variable `name` is cleared, by a statement that
/// starts with `name.` and calls `env_clear()`, before any statement starts
/// it, passes it on or returns it, and before its block ends.
fn cleared_later(code: &str, from: usize, name: &str) -> bool {
    let mut at = from;
    while at < code.len() {
        at += code[at..].len() - code[at..].trim_start().len();
        if at >= code.len() || code[at..].starts_with(['}', ')', ']']) {
            return false;
        }
        let end = statement_end(code, at);
        let statement = &code[at..end];
        if flat(statement).starts_with(&format!("{name}.")) {
            if clears_first(statement) {
                return true;
            }
            if starts(statement) {
                return false;
            }
        } else if mentions(statement, name) {
            return false;
        }
        at = end + 1;
    }
    false
}

/// Offsets of `.envs(...)` calls in `code` whose argument is the parent's
/// own environment (`std::env::vars()`, `env::vars_os()`, `vars()`).
fn parent_environment(code: &str) -> Vec<usize> {
    code.match_indices(".envs(")
        .map(|(at, _)| at)
        .filter(|&at| {
            let open = at + ".envs(".len();
            let argument = flat(&code[open..statement_end(code, open)]);
            argument.starts_with("vars")
                || argument
                    .match_indices("env::vars")
                    .any(|(found, _)| !argument[..found].chars().next_back().is_some_and(is_ident))
        })
        .collect()
}

/// Places in `code` that build a command without clearing its
/// environment first, or that pass the parent's environment to a child,
/// as `file:line: text`.
fn uncleared(relative: &str, code: &str) -> Vec<String> {
    let stripped = strip(code);
    let lines: Vec<&str> = code.lines().collect();
    let line_of = |offset: usize| stripped[..offset].matches('\n').count();
    let mut offsets: Vec<usize> = sites(&stripped, &command_names(&stripped))
        .into_iter()
        .filter(|&site| {
            let end = statement_end(&stripped, site);
            let chain = &stripped[site..end];
            let cleared = clears_first(chain)
                || (!starts(chain)
                    && bound_name(&stripped, site)
                        .is_some_and(|name| cleared_later(&stripped, end + 1, &name)));
            !cleared
        })
        .collect();
    offsets.extend(parent_environment(&stripped));
    offsets.sort_unstable();
    offsets
        .into_iter()
        .filter_map(|offset| {
            let index = line_of(offset);
            let trimmed = lines.get(index).map_or("", |line| line.trim());
            let excepted = EXCEPTIONS
                .iter()
                .any(|(file, text, _)| *file == relative && *text == trimmed);
            (!excepted).then(|| format!("{relative}:{}: {trimmed}", index + 1))
        })
        .collect()
}

#[test]
fn every_non_test_command_starts_from_a_cleared_environment() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = Vec::new();
    rust_files(&root.join("crates"), &mut files);
    assert!(files.len() > 50, "found only {} Rust files", files.len());
    let mut offenders = Vec::new();
    for file in files {
        let relative = file
            .strip_prefix(&root)
            .unwrap_or(&file)
            .to_string_lossy()
            .replace('\\', "/");
        if relative.ends_with("/tests.rs") || relative.ends_with("_tests.rs") {
            continue;
        }
        let text = std::fs::read_to_string(&file).unwrap_or_default();
        // Test modules start programs freely to build fixtures.
        offenders.extend(uncleared(&relative, non_test_code(&text)));
    }
    assert!(
        offenders.is_empty(),
        "start these with .env_clear().envs(anvil_types::child_env::vars()):\n{}",
        offenders.join("\n")
    );
}

#[test]
fn a_test_module_declaration_does_not_hide_the_rest_of_the_file() {
    let text = "#[cfg(test)]\nmod helpers;\nfn f() { Command::new(\"x\"); }\n#[cfg(test)]\nmod tests {\n}\n";
    let code = non_test_code(text);
    assert!(code.contains("fn f()"), "{code}");
    assert!(!code.contains("mod tests"), "{code}");
}

#[test]
fn the_scan_catches_a_command_that_keeps_the_environment() {
    let bad = "fn f() {\n    let out = Command::new(\"icacls\").arg(p).output();\n}\n";
    assert_eq!(uncleared("x.rs", bad).len(), 1);
    let good = "fn f() {\n    let out = Command::new(\"icacls\")\n        .env_clear()\n        .output();\n}\n";
    assert!(uncleared("x.rs", good).is_empty());
}

#[test]
fn the_scan_is_not_fooled_by_the_evasions_the_review_found() {
    let evasions = [
        (
            "env_clear then the parent's whole environment",
            "fn f() {\n    Command::new(\"x\").env_clear().envs(std::env::vars()).output();\n}\n",
        ),
        (
            "vars_os through a use",
            "use std::env;\nfn f() {\n    Command::new(\"x\").env_clear().envs(env::vars_os()).spawn();\n}\n",
        ),
        (
            "an aliased Command",
            "use std::process::Command as Proc;\nfn f() {\n    Proc::new(\"x\").arg(\"y\").output();\n}\n",
        ),
        (
            "a type alias",
            "type Proc = tokio::process::Command;\nfn f() {\n    let out = Proc::new(\"x\").output();\n}\n",
        ),
        (
            "a second command borrows the first one's env_clear",
            "fn f() {\n    let a = Command::new(\"a\").output();\n    let b = Command::new(\"b\").env_clear().output();\n}\n",
        ),
        (
            "env_clear in a comment",
            "fn f() {\n    Command::new(\"x\") // .env_clear()\n        .output();\n}\n",
        ),
        (
            "env_clear in a string",
            "fn f() {\n    Command::new(\"x\").arg(\".env_clear()\").output();\n}\n",
        ),
        (
            "env_clear after the program started",
            "fn f() {\n    let child = Command::new(\"x\").spawn();\n    child.env_clear();\n}\n",
        ),
        (
            "a builder that escapes before it is cleared",
            "fn f() {\n    let mut c = Command::new(\"x\");\n    run(c);\n    c.env_clear();\n}\n",
        ),
    ];
    for (what, code) in evasions {
        assert!(
            !uncleared("x.rs", code).is_empty(),
            "{what} passed the scan"
        );
    }
    let accepted = [
        "fn f() {\n    let mut sh = Command::new(&c);\n    sh.args([\"-c\"])\n        .env_clear()\n        .envs(anvil_types::child_env::vars());\n    run(sh);\n}\n",
        "fn f() -> Command {\n    let mut command = Command::new(\"git\");\n    command.args(A).env_clear().envs(child_env::vars());\n    command\n}\n",
        "fn f() {\n    let out = std::process::Command::new(\"x\")\n        .env_clear()\n        .output();\n}\n",
        "fn f() {\n    let c = match x {\n        A => Command::new(\"a\").env_clear().arg(\"'\"),\n        B => Command::new(\"b\").env_clear(),\n    };\n}\n",
    ];
    for code in accepted {
        assert!(uncleared("x.rs", code).is_empty(), "{code}");
    }
}
