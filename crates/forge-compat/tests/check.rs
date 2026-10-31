//! `forge compat check` against the fixtures: a sanitized `~/.claude` and a
//! trimmed copy of the Caveman plugin repository.

use std::path::{Path, PathBuf};

use forge_compat::check::{Status, check};
use forge_types::ForgeConfig;

/// Tool names a plain forge session has.
const TOOLS: &[&str] = &[
    "read_file",
    "write_file",
    "edit_file",
    "glob",
    "grep",
    "shell",
    "list_dir",
    "skill",
    "delegate",
];

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

/// Renders with a stable root so the snapshot does not depend on the checkout path.
fn render(name: &str, config: Option<&ForgeConfig>) -> String {
    let mut report = check(&fixture(name), config, TOOLS);
    report.root = PathBuf::from(format!("<fixtures>/{name}"));
    report.render()
}

#[test]
fn a_claude_home_folder_loads_its_synced_skills_and_reports_the_rest() {
    let report = check(&fixture("claude-home"), None, TOOLS);
    assert!(!report.has_errors(), "{}", report.render());
    let skills: Vec<&str> = report
        .items
        .iter()
        .filter(|item| item.kind == "skill" && item.status == Status::Loads)
        .map(|item| item.path.as_str())
        .collect();
    assert_eq!(
        skills,
        [
            "skills/synced/dfcf6f6d_20689276/docs/SKILL.md",
            "skills/synced/dfcf6f6d_20689276/pdf/SKILL.md",
        ]
    );
    insta::assert_snapshot!(render("claude-home", None));
}

#[test]
fn the_caveman_repository_loads_without_errors() {
    let report = check(&fixture("caveman"), None, TOOLS);
    assert!(!report.has_errors(), "{}", report.render());
    let (loads, skipped, errors) = report.counts();
    assert!(loads >= 6, "{loads} loads");
    assert!(skipped >= 6, "{skipped} skipped");
    assert_eq!(errors, 0);
    insta::assert_snapshot!(render("caveman", None));
}

#[test]
fn an_unknown_agent_model_is_explained_against_the_config() {
    let config: ForgeConfig =
        toml::from_str(forge_types::config::CONFIG_TEMPLATE).expect("template parses");
    let report = check(&fixture("caveman"), Some(&config), TOOLS);
    let reviewer = report
        .items
        .iter()
        .find(|item| item.path == "agents/cavecrew-reviewer.md")
        .expect("reviewer row");
    assert_eq!(reviewer.status, Status::Loads);
    assert!(
        reviewer
            .detail
            .contains("model `haiku` is not a role or provider"),
        "{}",
        reviewer.detail
    );
}

#[test]
fn an_empty_folder_reports_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let report = check(dir.path(), None, TOOLS);
    assert!(report.items.is_empty());
    assert!(
        report
            .render()
            .contains("nothing forge reads was found here")
    );
}
