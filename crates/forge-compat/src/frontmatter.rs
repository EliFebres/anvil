//! YAML frontmatter at the top of a Markdown file, as Claude Code writes it.
//!
//! A frontmatter block is a `---` line, YAML, and a closing `---` line, all
//! at the start of the file. Byte order marks, Windows line endings and
//! empty blocks stacked above the real one (some published skills have a
//! stray `---`/`---` pair) are tolerated.

use std::path::Path;

use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

use crate::error::CompatError;

/// A parsed file: the frontmatter as a JSON object plus the Markdown body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frontmatter {
    /// The frontmatter keys. Empty when the file has no block.
    pub fields: Map<String, Value>,
    /// Everything after the closing `---`, with a single trailing newline.
    pub body: String,
}

impl Frontmatter {
    /// A required string field.
    ///
    /// # Arguments
    ///
    /// * `field` - The key.
    /// * `path` - The file, for the error.
    ///
    /// # Returns
    ///
    /// The value, trimmed.
    ///
    /// # Errors
    ///
    /// [`CompatError::MissingField`] when the key is absent, not a string or
    /// blank.
    pub fn require_str(&self, field: &'static str, path: &Path) -> Result<&str, CompatError> {
        self.fields
            .get(field)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .ok_or_else(|| CompatError::MissingField {
                path: path.to_path_buf(),
                field,
            })
    }

    /// An optional string field; non-strings count as absent.
    ///
    /// # Arguments
    ///
    /// * `field` - The key.
    ///
    /// # Returns
    ///
    /// The value, trimmed, when present and non-blank.
    pub fn get_str(&self, field: &str) -> Option<&str> {
        self.fields
            .get(field)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
    }

    /// Deserializes the fields into a typed manifest.
    ///
    /// # Arguments
    ///
    /// * `path` - The file, for the error.
    ///
    /// # Returns
    ///
    /// The typed value.
    ///
    /// # Errors
    ///
    /// [`CompatError::Frontmatter`] when the fields do not fit the type.
    pub fn typed<T: DeserializeOwned>(&self, path: &Path) -> Result<T, CompatError> {
        serde_json::from_value(Value::Object(self.fields.clone())).map_err(|error| {
            CompatError::Frontmatter {
                path: path.to_path_buf(),
                detail: error.to_string(),
            }
        })
    }
}

/// Splits a file into its frontmatter YAML and body.
///
/// # Arguments
///
/// * `text` - The whole file.
///
/// # Returns
///
/// The YAML between the delimiters (possibly empty) and the body, or `None`
/// when the file does not start with a `---` line. Line endings in both
/// parts are normalized to `\n`.
pub fn split(text: &str) -> Option<(String, String)> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.lines().peekable();
    while lines.peek().is_some_and(|line| line.trim().is_empty()) {
        lines.next();
    }
    if lines.next().map(str::trim_end) != Some("---") {
        return None;
    }
    let mut yaml: Vec<&str> = Vec::new();
    for line in lines.by_ref() {
        if line.trim_end() == "---" {
            if yaml.iter().all(|line| line.trim().is_empty()) {
                // An empty block above the real one: treat this `---` as the
                // opening of the next block.
                yaml.clear();
                continue;
            }
            let mut body = lines.collect::<Vec<_>>().join("\n");
            body = body.trim_matches('\n').to_owned();
            if !body.is_empty() {
                body.push('\n');
            }
            return Some((yaml.join("\n"), body));
        }
        yaml.push(line);
    }
    None
}

/// Parses a file that must have frontmatter.
///
/// # Arguments
///
/// * `text` - The whole file.
/// * `path` - The file, for errors.
///
/// # Returns
///
/// The fields and body.
///
/// # Errors
///
/// [`CompatError::NoFrontmatter`] when there is no block, or
/// [`CompatError::Frontmatter`] when the YAML is invalid or not a mapping.
pub fn parse(text: &str, path: &Path) -> Result<Frontmatter, CompatError> {
    let Some((yaml, body)) = split(text) else {
        return Err(CompatError::NoFrontmatter {
            path: path.to_path_buf(),
        });
    };
    let fields = parse_yaml_object(&yaml, path)?;
    Ok(Frontmatter { fields, body })
}

/// Parses a file whose frontmatter is optional.
///
/// # Arguments
///
/// * `text` - The whole file.
/// * `path` - The file, for errors.
///
/// # Returns
///
/// The fields (empty when there is no block) and the body (the whole file
/// when there is no block).
///
/// # Errors
///
/// [`CompatError::Frontmatter`] when a block exists but is not valid YAML.
pub fn parse_optional(text: &str, path: &Path) -> Result<Frontmatter, CompatError> {
    match split(text) {
        Some((yaml, body)) => Ok(Frontmatter {
            fields: parse_yaml_object(&yaml, path)?,
            body,
        }),
        None => Ok(Frontmatter {
            fields: Map::new(),
            body: text
                .strip_prefix('\u{feff}')
                .unwrap_or(text)
                .replace("\r\n", "\n"),
        }),
    }
}

/// Parses YAML into a JSON object; an empty document is an empty object.
fn parse_yaml_object(yaml: &str, path: &Path) -> Result<Map<String, Value>, CompatError> {
    if yaml.trim().is_empty() {
        return Ok(Map::new());
    }
    let value: Value = serde_saphyr::from_str(yaml).map_err(|error| CompatError::Frontmatter {
        path: path.to_path_buf(),
        detail: error.to_string(),
    })?;
    match value {
        Value::Object(fields) => Ok(fields),
        Value::Null => Ok(Map::new()),
        other => Err(CompatError::Frontmatter {
            path: path.to_path_buf(),
            detail: format!("expected a mapping, found {}", kind(&other)),
        }),
    }
}

/// A short word for a JSON value's type.
fn kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "a list",
        Value::Object(_) => "a mapping",
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn path() -> PathBuf {
        PathBuf::from("SKILL.md")
    }

    #[test]
    fn splits_yaml_from_body() {
        let (yaml, body) =
            split("---\nname: x\ndescription: y\n---\n\n# Title\n\nBody.\n").expect("split");
        assert_eq!(yaml, "name: x\ndescription: y");
        assert_eq!(body, "# Title\n\nBody.\n");
    }

    #[test]
    fn handles_windows_line_endings_and_a_bom() {
        let text = "\u{feff}---\r\nname: x\r\ndescription: why\r\n---\r\nBody line 1\r\nline 2\r\n";
        let parsed = parse(text, &path()).expect("parses");
        assert_eq!(parsed.fields["name"], "x");
        assert_eq!(parsed.fields["description"], "why");
        assert_eq!(parsed.body, "Body line 1\nline 2\n");
    }

    #[test]
    fn skips_an_empty_block_stacked_above_the_real_one() {
        let text = "---\n---\n---\nname: caveman\ndescription: >\n  Terse voice.\n  Keeps facts.\n---\n\nRules.\n";
        let parsed = parse(text, &path()).expect("parses");
        assert_eq!(parsed.fields["name"], "caveman");
        assert_eq!(parsed.fields["description"], "Terse voice. Keeps facts.\n");
        assert_eq!(parsed.body, "Rules.\n");

        let two = "---\n---\nname: caveman\ndescription: d\n---\nBody\n";
        let parsed = parse(two, &path()).expect("parses");
        assert_eq!(parsed.fields["name"], "caveman");
        assert_eq!(parsed.body, "Body\n");
    }

    #[test]
    fn unknown_keys_are_kept() {
        let parsed = parse(
            "---\nname: x\ndescription: y\nlicense: MIT\nmetadata:\n  version: 2\n---\n",
            &path(),
        )
        .expect("parses");
        assert_eq!(parsed.fields["license"], "MIT");
        assert_eq!(parsed.fields["metadata"]["version"], 2);
        assert_eq!(parsed.body, "");
    }

    #[test]
    fn missing_description_is_reported_by_name() {
        let parsed = parse("---\nname: x\n---\nBody\n", &path()).expect("parses");
        assert_eq!(
            parsed.require_str("description", &path()),
            Err(CompatError::MissingField {
                path: path(),
                field: "description"
            })
        );
        assert_eq!(parsed.require_str("name", &path()), Ok("x"));
    }

    #[test]
    fn a_file_without_a_block_is_an_error_or_a_plain_body() {
        assert!(matches!(
            parse("# Just markdown\n", &path()),
            Err(CompatError::NoFrontmatter { .. })
        ));
        let plain = parse_optional("# Just markdown\r\n", &path()).expect("parses");
        assert!(plain.fields.is_empty());
        assert_eq!(plain.body, "# Just markdown\n");
    }

    #[test]
    fn invalid_yaml_and_non_mappings_are_errors() {
        assert!(matches!(
            parse("---\nname: [unclosed\n---\n", &path()),
            Err(CompatError::Frontmatter { .. })
        ));
        assert!(matches!(
            parse("---\n- a\n- b\n---\n", &path()),
            Err(CompatError::Frontmatter { detail, .. }) if detail.contains("a list")
        ));
    }

    #[test]
    fn typed_deserializes_into_a_manifest() {
        let parsed = parse(
            "---\nname: x\ndescription: why\nallowed-tools: Read, Grep\nrequires: [shell]\n---\n",
            &path(),
        )
        .expect("parses");
        let manifest: forge_types::SkillManifest = parsed.typed(&path()).expect("typed");
        assert_eq!(manifest.allowed_tools, ["Read", "Grep"]);
        assert_eq!(manifest.requires, ["shell"]);
    }
}
