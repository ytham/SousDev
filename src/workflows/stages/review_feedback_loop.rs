use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use crate::workflows::stage::{Stage, StageContext};
use crate::workflows::stages::agent_loop::AgentLoopStage;
use crate::workflows::stages::reviewer::ReviewerStage;
use crate::utils::prompt_loader::PromptLoader;

/// Orchestrates repeated reviewer → agent-loop feedback rounds.
///
/// Runs up to `max_review_rounds` cycles of:
///   1. Reviewer pass
///   2. If rejected, load the review-feedback prompt, update the task,
///      and re-run the agent loop.
///
/// Stops early if the reviewer approves (`ctx.review_feedback` is `None`).
/// After `max_review_rounds` the last result is kept and the workflow
/// continues.
pub struct ReviewFeedbackLoopStage;

#[async_trait]
impl Stage for ReviewFeedbackLoopStage {
    fn name(&self) -> &str {
        "review-feedback-loop"
    }

    async fn run(&self, ctx: &mut StageContext) -> Result<()> {
        let max_rounds = ctx
            .config
            .agent_loop
            .max_review_rounds
            .unwrap_or(2);

        for round in 0..max_rounds {
            if ctx.is_aborted() {
                break;
            }

            // Run one review pass.
            ReviewerStage.run(ctx).await?;
            ctx.review_rounds = round + 1;

            // No feedback means the reviewer approved — we're done.
            if ctx.review_feedback.is_none() {
                ctx.logger.info(&format!(
                    "Reviewer approved on round {}.",
                    round + 1
                ));
                break;
            }

            if round < max_rounds - 1 {
                ctx.logger.info(&format!(
                    "Review round {} rejected — re-running agent with critique \
                     (round {} / {})",
                    round + 1,
                    round + 2,
                    max_rounds
                ));

                // Build the feedback task from the review-feedback prompt.
                let loader = PromptLoader::new(&ctx.harness_root);
                let mut vars = HashMap::new();
                vars.insert(
                    "original_task".to_string(),
                    ctx.parsed_task.task.clone(),
                );
                vars.insert(
                    "review_comments".to_string(),
                    ctx.review_feedback.clone().unwrap_or_default(),
                );
                // Provide a sensible default test command; operators can
                // override this via the `review_feedback` prompt template.
                vars.insert(
                    "test_command".to_string(),
                    "pnpm test".to_string(),
                );
                let feedback_task =
                    loader.load(&ctx.prompts.review_feedback, &vars).await?;

                // Replace the task with the feedback-enriched version and
                // clear the review_feedback so the next round starts fresh.
                ctx.parsed_task.task = feedback_task;
                ctx.review_feedback = None;

                // Re-run the agent with the enriched task.
                AgentLoopStage.run(ctx).await?;
            } else {
                ctx.logger.info(&format!(
                    "Max review rounds ({}) reached — proceeding with last result.",
                    max_rounds
                ));
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_review_feedback_loop_name() {
        assert_eq!(ReviewFeedbackLoopStage.name(), "review-feedback-loop");
    }
}
