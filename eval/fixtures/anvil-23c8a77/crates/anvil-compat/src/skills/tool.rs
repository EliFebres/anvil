//! The `skill` tool: loads a skill's full instructions into the conversation.

use std::sync::Arc;

use anvil_core::tools::{
    Access, Tool, ToolContext, ToolFuture, ToolOutput, parse_args, schema_for,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use super::SkillSet;

/// Arguments of the `skill` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SkillArgs {
    /// The skill's name, exactly as listed in the system prompt.
    pub name: String,
    /// Free text to pass to the skill, such as what the user typed after `/<name>`.
    #[serde(default)]
    pub args: Option<String>,
}

/// Returns a skill's body and supporting-file list.
pub struct SkillTool {
    /// The loaded skills.
    skills: Arc<SkillSet>,
}

impl SkillTool {
    /// Wraps a skill set.
    ///
    /// # Arguments
    ///
    /// * `skills` - The loaded skills.
    ///
    /// # Returns
    ///
    /// The tool.
    pub fn new(skills: Arc<SkillSet>) -> Self {
        Self { skills }
    }
}

impl Tool for SkillTool {
    fn name(&self) -> &str {
        "skill"
    }

    fn description(&self) -> &str {
        "Loads a skill's full instructions. Call it with the name of a skill from the \
         system prompt's skill list when a task matches its description or the user asks \
         for it by name, then follow the instructions it returns."
    }

    fn schema(&self) -> Value {
        schema_for::<SkillArgs>()
    }

    fn access(&self) -> Access {
        Access::Read
    }

    fn target(&self, args: &Value) -> Option<String> {
        args.get("name").and_then(Value::as_str).map(str::to_owned)
    }

    fn run<'a>(&'a self, args: Value, _ctx: &'a ToolContext) -> ToolFuture<'a> {
        Box::pin(async move {
            let args: SkillArgs = match parse_args(args) {
                Ok(args) => args,
                Err(output) => return output,
            };
            let name = args.name.trim().trim_start_matches('/');
            match self.skills.get(name) {
                Some(skill) => ToolOutput::ok(skill.render(args.args.as_deref())),
                None if self.skills.is_empty() => {
                    ToolOutput::error("no skills are installed".to_owned())
                }
                None => ToolOutput::error(format!(
                    "unknown skill `{name}`; available skills: {}",
                    self.skills.names().join(", ")
                )),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::skills::{SKILL_FILE, SkillRoots};

    fn set_with(name: &str, body: &str) -> Arc<SkillSet> {
        let project = tempfile::tempdir().expect("tempdir");
        let dir = project.path().join(".anvil").join("skills").join(name);
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            dir.join(SKILL_FILE),
            format!("---\nname: {name}\ndescription: d\n---\n{body}\n"),
        )
        .expect("write");
        let set = SkillSet::discover(&SkillRoots {
            project: project.path().to_path_buf(),
            home: None,
            bundled: None,
        });
        // The tempdir may go away now; the body is already in memory.
        Arc::new(set)
    }

    #[test]
    fn schema_snapshot() {
        insta::assert_json_snapshot!(SkillTool::new(Arc::default()).schema());
    }

    #[tokio::test]
    async fn returns_the_body_only_when_invoked() {
        let set = set_with("alpha", "THE-BODY");
        let tool = SkillTool::new(Arc::clone(&set));
        let section = set.prompt_section(&[]).expect("section");
        assert!(!section.text.contains("THE-BODY"));

        let ctx = ToolContext::new(".");
        let output = tool.run(json!({"name": "alpha"}), &ctx).await;
        assert!(!output.is_error);
        assert!(output.content.contains("THE-BODY"));

        let with_slash = tool.run(json!({"name": "/alpha", "args": "x"}), &ctx).await;
        assert!(with_slash.content.ends_with("## Arguments\n\nx\n"));

        let missing = tool.run(json!({"name": "beta"}), &ctx).await;
        assert!(missing.is_error);
        assert!(missing.content.contains("available skills: alpha"));
    }
}
