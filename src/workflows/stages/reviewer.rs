use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use crate::workflows::stage::{Stage, StageContext};
use crate::workflows::stages::external_agent_loop::{
    claude_adapter, run_external_agent_loop, ExternalAgentRunOptions,
};
use crate::techniques::critique_loop::{
    run_critique_loop, CritiqueLoopOptions, CritiqueLoopResult, CritiqueRound,
};
use crate::utils::prompt_loader::PromptLoader;

/// The token the reviewer must emit on its own line to signal approval.
pub const APPROVAL_TOKEN: &str = "HARNESS_REVIEW_APPROVED";

/// Minimum critique-loop score (0–10) that is treated as an approval.
pub const REVIEW_APPROVAL_SCORE: f64 = 7.0;

/// Default review criteria used when `claude-loop` is not the technique.
const REVIEW_CRITERIA: &[&str] = &[
    "correctness — does the output solve the stated task?",
    "completeness — are all required changes present?",
    "safety — no regressions, no destructive side-effects",
    "code quality — readable, idiomatic, well-structured",
];

/// Runs a single reviewer pass.
///
/// - For `claude-loop` technique: invokes the external `claude` CLI with the
///   `code_review` prompt and looks for [`APPROVAL_TOKEN`] in the output.
/// - For all other techniques: runs a one-round [`CritiqueLoopOptions`] pass
///   using the harness LLM provider and accepts when the score ≥
///   [`REVIEW_APPROVAL_SCORE`].
///
/// On approval `ctx.review_feedback` is set to `None`.
/// On rejection `ctx.review_feedback` contains the critique text.
pub struct ReviewerStage;

#[async_trait]
impl Stage for ReviewerStage {
    fn name(&self) -> &str {
        "reviewer"
    }

    async fn run(&self, ctx: &mut StageContext) -> Result<()> {
        if ctx.is_aborted() || ctx.agent_result.is_none() {
            return Ok(());
        }

        let technique = ctx.config.agent_loop.technique.clone();
        if technique == "claude-loop" {
            run_claude_review(ctx).await
        } else {
            run_llm_judge_review(ctx).await
        }
    }
}

// ---------------------------------------------------------------------------
// Internal: claude-loop reviewer
// ---------------------------------------------------------------------------

async fn run_claude_review(ctx: &mut StageContext) -> Result<()> {
    ctx.logger
        .info("ReviewerStage (claude-loop): running one review pass");

    let loader = PromptLoader::new(&ctx.harness_root);
    let mut vars = HashMap::new();
    vars.insert("task".to_string(), ctx.parsed_task.task.clone());
    vars.insert("round_note".to_string(), String::new());
    let review_prompt = loader.load(&ctx.prompts.code_review, &vars).await?;

    let opts = ExternalAgentRunOptions {
        cwd: Some(ctx.workspace_dir.to_string_lossy().to_string()),
        ..Default::default()
    };
    let adapter = claude_adapter(ExternalAgentRunOptions::default());
    let review_run = run_external_agent_loop(&review_prompt, ctx, &adapter, &opts, ctx.system_prompt.as_deref()).await?;

    let is_approved = review_run.answer.contains(APPROVAL_TOKEN);
    let score = if is_approved { 10.0 } else { 4.0 };

    let critique = CritiqueLoopResult {
        answer: review_run.answer.clone(),
        rounds: vec![CritiqueRound {
            response: ctx
                .agent_result
                .as_ref()
                .map(|r| r.answer.clone())
                .unwrap_or_default(),
            critique: review_run.answer.clone(),
            score,
        }],
    };
    ctx.review_result = Some(critique);

    if is_approved {
        ctx.logger.info("Reviewer approved the changes.");
        ctx.review_feedback = None;
    } else {
        ctx.logger
            .info("Reviewer requested changes — returning critique to executor.");
        let clean = review_run
            .answer
            .replace(APPROVAL_TOKEN, "")
            .trim()
            .to_string();
        ctx.review_feedback = Some(clean);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Internal: LLM-as-judge reviewer (all non-claude-loop techniques)
// ---------------------------------------------------------------------------

async fn run_llm_judge_review(ctx: &mut StageContext) -> Result<()> {
    ctx.logger
        .info("ReviewerStage (llm-judge): running one review pass");

    let agent_answer = ctx
        .agent_result
        .as_ref()
        .map(|r| r.answer.as_str())
        .unwrap_or("");

    let task_for_review = format!(
        "Review the following output produced by an autonomous agent.\n\n\
         Original task:\n{}\n\nAgent output:\n{}",
        ctx.parsed_task.task, agent_answer
    );

    let result = run_critique_loop(CritiqueLoopOptions {
        task: task_for_review,
        provider: ctx.provider.clone(),
        max_rounds: 1,
        criteria: REVIEW_CRITERIA
            .iter()
            .map(|s| s.to_string())
            .collect(),
        satisfaction_threshold: REVIEW_APPROVAL_SCORE,
    })
    .await?;

    let score = result
        .rounds
        .first()
        .map(|r| r.score)
        .unwrap_or(0.0);
    ctx.logger
        .info(&format!("Review score: {:.0} / 10", score));

    if score >= REVIEW_APPROVAL_SCORE {
        ctx.logger.info("Reviewer accepted the output.");
        ctx.review_feedback = None;
    } else {
        ctx.logger
            .info("Reviewer rejected — returning critique to executor.");
        let critique = result
            .rounds
            .first()
            .map(|r| r.critique.clone())
            .unwrap_or_default();
        ctx.review_feedback = Some(critique);
    }
    ctx.review_result = Some(result);
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_approval_token_value() {
        assert_eq!(APPROVAL_TOKEN, "HARNESS_REVIEW_APPROVED");
    }

    #[test]
    fn test_approval_score_value() {
        assert_eq!(REVIEW_APPROVAL_SCORE, 7.0);
    }

    #[test]
    fn test_approval_token_detection() {
        let output = "Looks good.\nHARNESS_REVIEW_APPROVED\nNo issues found.";
        assert!(output.contains(APPROVAL_TOKEN));
    }

    #[test]
    fn test_no_approval_token() {
        let output = "The code has issues. Please fix the null check on line 42.";
        assert!(!output.contains(APPROVAL_TOKEN));
    }

    #[test]
    fn test_clean_approval_token_from_output() {
        let raw = "Some critique.\nHARNESS_REVIEW_APPROVED\nExtra text.";
        let clean = raw.replace(APPROVAL_TOKEN, "").trim().to_string();
        assert!(!clean.contains(APPROVAL_TOKEN));
        assert!(clean.contains("Some critique."));
    }
}
