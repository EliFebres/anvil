//! The `complete_step` tool: the loop's own tool for marking a plan step done.
//!
//! It exists so a tick is one more call beside a step's last edit instead
//! of an `edit_file` on the plan that the model tends to spend a turn on.
//! Only [`AgentLoop`](super::AgentLoop) registers it, because only a loop
//! session has a plan.

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use super::PLAN_FILE;
use super::plan::{Tick, tick};
use crate::tools::{Access, Tool, ToolContext, ToolFuture, ToolOutput, parse_args, schema_for};

/// The tool's name.
pub const COMPLETE_STEP: &str = "complete_step";

/// Arguments of the `complete_step` tool.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct CompleteStepArgs {
    /// The number of the finished step, as written in the plan.
    pub step: u32,
}

/// Ticks one step's checkbox in the session's plan file.
pub struct CompleteStep;

impl Tool for CompleteStep {
    fn name(&self) -> &str {
        COMPLETE_STEP
    }

    fn description(&self) -> &str {
        "Marks one step of the approved plan as done. Call it in the same turn as the step's \
         last edit, as one more call beside that edit, never in a turn of its own. Steps still \
         open when the review approves are marked done automatically."
    }

    fn schema(&self) -> Value {
        schema_for::<CompleteStepArgs>()
    }

    fn access(&self) -> Access {
        Access::Write
    }

    fn run<'a>(&'a self, args: Value, ctx: &'a ToolContext) -> ToolFuture<'a> {
        Box::pin(async move {
            let args: CompleteStepArgs = match parse_args(args) {
                Ok(args) => args,
                Err(output) => return output,
            };
            let Some(session_dir) = &ctx.session_dir else {
                return ToolOutput::error("this session has no plan".to_owned());
            };
            let path = session_dir.join(PLAN_FILE);
            let plan = match std::fs::read_to_string(&path) {
                Ok(plan) => plan,
                Err(error) => return ToolOutput::error(format!("cannot read the plan: {error}")),
            };
            let step = args.step;
            match tick(&plan, step) {
                Tick::Ticked(ticked) => match std::fs::write(&path, ticked) {
                    Ok(()) => ToolOutput::ok(format!("step {step} marked done")),
                    Err(error) => ToolOutput::error(format!("cannot write the plan: {error}")),
                },
                Tick::AlreadyDone => ToolOutput::ok(format!("step {step} was already done")),
                Tick::NoSuchStep => ToolOutput::error(format!("the plan has no step {step}")),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn ctx(dir: &std::path::Path) -> ToolContext {
        let mut ctx = ToolContext::new(dir.to_path_buf());
        ctx.session_dir = Some(dir.to_path_buf());
        ctx
    }

    #[test]
    fn schema_snapshot() {
        insta::assert_json_snapshot!(CompleteStep.schema());
    }

    #[tokio::test]
    async fn writes_the_tick_into_the_plan_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let plan = dir.path().join(PLAN_FILE);
        std::fs::write(&plan, "# Plan\n\n1. [ ] First\n2. [ ] Second\n").expect("plan");
        let ctx = ctx(dir.path());

        let output = CompleteStep.run(json!({"step": 2}), &ctx).await;
        assert!(!output.is_error, "{}", output.content);
        assert_eq!(output.content, "step 2 marked done");
        assert_eq!(
            std::fs::read_to_string(&plan).expect("plan"),
            "# Plan\n\n1. [ ] First\n2. [x] Second\n"
        );

        let again = CompleteStep.run(json!({"step": 2}), &ctx).await;
        assert!(!again.is_error);
        assert_eq!(again.content, "step 2 was already done");
    }

    #[tokio::test]
    async fn a_missing_step_or_plan_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = ctx(dir.path());
        let no_plan = CompleteStep.run(json!({"step": 1}), &ctx).await;
        assert!(no_plan.is_error);
        assert!(
            no_plan.content.starts_with("cannot read the plan"),
            "{}",
            no_plan.content
        );

        std::fs::write(dir.path().join(PLAN_FILE), "# Plan\n\n1. [ ] First\n").expect("plan");
        let no_step = CompleteStep.run(json!({"step": 7}), &ctx).await;
        assert!(no_step.is_error);
        assert_eq!(no_step.content, "the plan has no step 7");

        let no_session = CompleteStep
            .run(
                json!({"step": 1}),
                &ToolContext::new(dir.path().to_path_buf()),
            )
            .await;
        assert_eq!(no_session.content, "this session has no plan");

        let bad_args = CompleteStep.run(json!({"step": "one"}), &ctx).await;
        assert!(bad_args.is_error);
    }
}
