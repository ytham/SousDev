use anyhow::Result;
use async_trait::async_trait;
use std::time::Duration;
use tokio::process::Command;
use crate::workflows::stage::{Stage, StageContext};
use crate::workflows::stages::external_agent_loop::{
    claude_adapter, codex_adapter, gemini_adapter, run_external_agent_loop,
    ExternalAgentRunOptions,
};
use crate::types::technique::RunResult;

/// Maximum exponential back-off cap (1 minute).
const MAX_BACKOFF_MS: u64 = 60_000;

/// Routes the agent loop to the correct external CLI or harness-native
/// technique, applying exponential back-off retries on failure.
pub struct AgentLoopStage;

#[async_trait]
impl Stage for AgentLoopStage {
    fn name(&self) -> &str {
        "agent-loop"
    }

    async fn run(&self, ctx: &mut StageContext) -> Result<()> {
        let technique = ctx.config.agent_loop.technique.clone();
        let max_retries = ctx.config.agent_loop.max_retries.unwrap_or(1);
        let base_backoff = ctx.config.agent_loop.backoff_ms.unwrap_or(2_000);
        let ext_cfg = ctx
            .config
            .agent_loop
            .external_agent
            .clone()
            .unwrap_or_default();
        let base_task_text = ctx.parsed_task.full_text();

        ctx.logger.info(&format!(
            "AgentLoopStage: running \"{}\" (max {} retries)",
            technique, max_retries
        ));

        let mut last_result: Option<RunResult> = None;

        for attempt in 0..=max_retries {
            if ctx.is_aborted() {
                return Err(anyhow::anyhow!("AgentLoopStage aborted."));
            }

            // On retries, enrich the task with context from the prior attempt.
            let task_text = if attempt == 0 {
                base_task_text.clone()
            } else {
                build_resume_task_text(
                    &base_task_text,
                    last_result.as_ref(),
                    &ctx.workspace_dir,
                )
                .await
            };

            ctx.logger
                .info(&format!("Attempt {} / {}", attempt + 1, max_retries + 1));

            let opts = ExternalAgentRunOptions {
                cwd: Some(ctx.workspace_dir.to_string_lossy().to_string()),
                timeout_secs: ext_cfg.timeout_secs,
                model: ext_cfg.model.clone(),
                extra_flags: ext_cfg.extra_flags.clone(),
            };

            let sys_prompt = ctx.system_prompt.as_deref();

            let result = match technique.as_str() {
                "claude-loop" => {
                    let adapter = claude_adapter(ExternalAgentRunOptions::default());
                    run_external_agent_loop(&task_text, ctx, &adapter, &opts, sys_prompt).await
                }
                "codex-loop" => {
                    let adapter = codex_adapter(ExternalAgentRunOptions::default());
                    run_external_agent_loop(&task_text, ctx, &adapter, &opts, sys_prompt).await
                }
                "gemini-loop" => {
                    let adapter = gemini_adapter(ExternalAgentRunOptions::default());
                    run_external_agent_loop(&task_text, ctx, &adapter, &opts, sys_prompt).await
                }
                other => Err(anyhow::anyhow!(
                    "Unknown technique: '{}'. Harness-native techniques \
                     (react, reflexion, plan-and-solve) must be invoked \
                     directly, not through AgentLoopStage.",
                    other
                )),
            };

            let result = match result {
                Ok(r) => r,
                Err(e) => {
                    ctx.logger
                        .error(&format!("Attempt {} threw: {}", attempt + 1, e));
                    RunResult::failure("unknown", e.to_string(), vec![], 0)
                }
            };

            if result.success {
                ctx.logger.info(&format!(
                    "Technique \"{}\" completed successfully.",
                    technique
                ));
                ctx.agent_result = Some(result);
                return Ok(());
            }

            last_result = Some(result);

            if attempt < max_retries {
                let delay =
                    (base_backoff * 2u64.pow(attempt as u32)).min(MAX_BACKOFF_MS);
                ctx.logger
                    .info(&format!("Waiting {}ms before next attempt…", delay));
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
        }

        // All attempts exhausted — record the last result and propagate error.
        if let Some(result) = last_result {
            ctx.agent_result = Some(result);
        }
        Err(anyhow::anyhow!(
            "AgentLoopStage: all {} attempts failed.",
            max_retries + 1
        ))
    }
}

// ---------------------------------------------------------------------------
// Resume context builder
// ---------------------------------------------------------------------------

/// Build an enriched task prompt for a retry, appending context gathered from
/// the prior attempt.
async fn build_resume_task_text(
    base_task: &str,
    prior_result: Option<&RunResult>,
    workspace_dir: &std::path::Path,
) -> String {
    let mut sections: Vec<String> = Vec::new();

    if let Some(result) = prior_result {
        if !result.answer.is_empty() {
            let truncated = if result.answer.len() > 1000 {
                format!("{}\n… (truncated)", &result.answer[..1000])
            } else {
                result.answer.clone()
            };
            sections.push(format!("Previous attempt output:\n{}", truncated));
        }
    }

    // Append `git diff HEAD --stat` (best-effort; ignore errors for non-git dirs).
    if let Ok(output) = Command::new("git")
        .arg("diff")
        .arg("HEAD")
        .arg("--stat")
        .current_dir(workspace_dir)
        .output()
        .await
    {
        let stat = String::from_utf8_lossy(&output.stdout);
        let stat = stat.trim();
        if !stat.is_empty() {
            let lines: Vec<&str> = stat.lines().collect();
            let truncated = if lines.len() > 30 {
                format!(
                    "{}\n… ({} more files)",
                    lines[..30].join("\n"),
                    lines.len() - 30
                )
            } else {
                stat.to_string()
            };
            sections.push(format!(
                "Files changed by previous attempt (git diff --stat):\n{}",
                truncated
            ));
        }
    }

    if sections.is_empty() {
        return base_task.to_string();
    }

    format!(
        "{}\n\n---\nContext from previous attempt \
         (do not undo this work unless it is incorrect):\n{}",
        base_task,
        sections.join("\n\n")
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_build_resume_task_no_prior() {
        // No prior result, no git repo at /tmp — should just return base task.
        let text =
            build_resume_task_text("Fix the bug", None, std::path::Path::new("/tmp")).await;
        assert!(text.starts_with("Fix the bug"));
    }

    #[tokio::test]
    async fn test_build_resume_task_with_prior_answer() {
        let prior = RunResult::failure("test", "err msg", vec![], 100);
        let text = build_resume_task_text(
            "Fix the bug",
            Some(&prior),
            std::path::Path::new("/tmp"),
        )
        .await;
        assert!(text.starts_with("Fix the bug"));
    }

    #[tokio::test]
    async fn test_build_resume_includes_prior_answer() {
        let prior = RunResult {
            technique: "test".into(),
            answer: "I tried doing X but it failed.".into(),
            trajectory: vec![],
            llm_calls: 1,
            duration_ms: 500,
            success: false,
            error: Some("exit 1".into()),
        };
        let text = build_resume_task_text(
            "Fix the bug",
            Some(&prior),
            std::path::Path::new("/tmp"),
        )
        .await;
        assert!(text.contains("I tried doing X but it failed."));
    }

    #[test]
    fn test_agent_loop_stage_name() {
        assert_eq!(AgentLoopStage.name(), "agent-loop");
    }

    // ── Additional tests ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_build_resume_task_text_with_prior_answer_truncated() {
        // Prior answer longer than 1000 chars should be truncated.
        let long_answer = "X".repeat(2000);
        let prior = RunResult {
            technique: "test".into(),
            answer: long_answer,
            trajectory: vec![],
            llm_calls: 1,
            duration_ms: 500,
            success: false,
            error: Some("fail".into()),
        };
        let text = build_resume_task_text(
            "Fix the bug",
            Some(&prior),
            std::path::Path::new("/tmp"),
        )
        .await;
        assert!(text.contains("Fix the bug"));
        assert!(text.contains("truncated"));
        // The included prior output should be at most 1000 X's.
        let x_count = text.matches('X').count();
        assert!(x_count <= 1000, "Expected <= 1000 X's, got {}", x_count);
    }

    #[tokio::test]
    async fn test_build_resume_task_text_empty_answer() {
        // A prior result with an empty answer should not include the
        // "Previous attempt output" section.
        let prior = RunResult {
            technique: "test".into(),
            answer: String::new(),
            trajectory: vec![],
            llm_calls: 0,
            duration_ms: 100,
            success: false,
            error: Some("oops".into()),
        };
        let text = build_resume_task_text(
            "Do the thing",
            Some(&prior),
            std::path::Path::new("/tmp"),
        )
        .await;
        assert!(text.starts_with("Do the thing"));
        assert!(!text.contains("Previous attempt output"));
    }
}
