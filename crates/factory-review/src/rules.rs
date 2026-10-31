//! The standing rules the style reviewer cites.
//!
//! `~/.anvil/rules.toml` belongs to anvil-memory, which this crate may not
//! depend on, so only the two fields a reviewer needs are read and every
//! other key is ignored.

use std::path::Path;

use serde::Deserialize;

use crate::error::ReviewError;

/// Where the rules live under the home directory.
const RULES_PATH: &str = ".anvil/rules.toml";

/// One standing rule: an id to cite and the rule text.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Rule {
    /// The id reviewers cite, such as `no-unwrap`.
    pub id: String,
    /// The rule in one or two sentences.
    pub rule: String,
}

/// The file shape; anvil-memory's extra keys are ignored.
#[derive(Debug, Default, Deserialize)]
struct RuleFile {
    #[serde(default)]
    rules: Vec<Rule>,
}

/// Reads `<home>/.anvil/rules.toml`.
///
/// # Arguments
///
/// * `home` - The home directory; `None` or a missing file yields no rules.
///
/// # Returns
///
/// The rules in file order.
///
/// # Errors
///
/// [`ReviewError::RulesIo`] when the file exists but cannot be read, and
/// [`ReviewError::RulesParse`] when it is not TOML of the expected shape.
pub fn load_rules(home: Option<&Path>) -> Result<Vec<Rule>, ReviewError> {
    let Some(home) = home else {
        return Ok(Vec::new());
    };
    let path = home.join(RULES_PATH);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(ReviewError::RulesIo { path, source }),
    };
    let file: RuleFile =
        toml::from_str(&text).map_err(|source| ReviewError::RulesParse { path, source })?;
    Ok(file.rules)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_home_or_file_means_no_rules() {
        assert!(load_rules(None).expect("ok").is_empty());
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(load_rules(Some(dir.path())).expect("ok").is_empty());
    }

    #[test]
    fn reads_id_and_rule_and_ignores_the_rest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(RULES_PATH);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(
            &path,
            "[[rules]]\nid = \"no-todo\"\nlanguages = [\"rust\"]\nrule = \"Leave no TODO.\"\ncheck = { regex = \"TODO\" }\n\n[[rules]]\nid = \"plain\"\nrule = \"Be plain.\"\n",
        )
        .expect("write");
        let rules = load_rules(Some(dir.path())).expect("ok");
        assert_eq!(
            rules,
            vec![
                Rule {
                    id: "no-todo".to_owned(),
                    rule: "Leave no TODO.".to_owned()
                },
                Rule {
                    id: "plain".to_owned(),
                    rule: "Be plain.".to_owned()
                },
            ]
        );
    }

    #[test]
    fn a_broken_file_names_its_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(RULES_PATH);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, "[[rules]]\nid = 1\n").expect("write");
        let error = load_rules(Some(dir.path())).expect_err("rejected");
        assert!(matches!(error, ReviewError::RulesParse { .. }), "{error}");
        assert!(error.to_string().contains("rules.toml"));
    }
}
