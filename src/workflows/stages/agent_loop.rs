use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::types::technique::RunResult;
use crate::utils::prompt_loader::PromptLoader;
use crate::workflows::stage::{Stage, StageContext};
use crate::workflows::stages::external_agent_loop::{
    claude_adapter, codex_adapter, gemini_adapter, run_external_agent_loop,
    ExternalAgentRunOptions,
};

/// Maximum exponential back-off cap (1 minute).
const MAX_BACKOFF_MS: u64 = 60_000;

/// Routes the agent loop to the correct external CLI or harness-native
/// technique, applying exponential back-off retries on failure.
///
/// Between failed attempts, a reflection step analyzes what went wrong
/// and produces targeted guidance for the next attempt (inspired by the
/// Reflexion technique).
pub struct AgentLoopStage;

#[async_trait]
impl Stage for AgentLoopStage {
    fn name(&self) -> &str {
        "agent-loop"
    }

    async fn run(&self, ctx: &mut StageContext) -> Result<()> {
        let technique = ctx.config.agent_loop.technique.clone();
        // Default retries: 0 for PR review (reviews shouldn't retry), 1 for all others.
        let default_retries = if ctx.config.github_prs.is_some() { 0 } else { 1 };
        let max_retries = ctx.config.agent_loop.max_retries.unwrap_or(default_retries);
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
        let mut last_reflection: Option<String> = None;

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
                    last_reflection.as_deref(),
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
                    let err_msg = format!("Attempt {} threw: {}", attempt + 1, e);
                    ctx.logger.error(&err_msg);
                    if let Some(ref log) = ctx.workflow_log {
                        let _ = log.log("error", "agent-loop", &err_msg).await;
                    }
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

            // Log the failure reason to the TUI.
            let error_msg = result
                .error
                .as_deref()
                .unwrap_or("(no error message)");
            let fail_msg = format!(
                "Attempt {}/{} failed: {}",
                attempt + 1,
                max_retries + 1,
                error_msg
            );
            ctx.logger.error(&fail_msg);
            if let Some(ref log) = ctx.workflow_log {
                let _ = log.log("error", "agent-loop", &fail_msg).await;
            }

            last_result = Some(result);

            // Generate a reflection before the next attempt (skip on the last).
            if attempt < max_retries {
                ctx.logger.info("Generating reflection on failed attempt…");
                last_reflection =
                    generate_reflection(ctx, &base_task_text, last_result.as_ref()).await;

                if let Some(ref reflection) = last_reflection {
                    ctx.logger
                        .info(&format!("Reflection: {}", reflection));
                    if let Some(ref log) = ctx.workflow_log {
                        let _ = log.log("info", "reflect", reflection).await;
                    }
                }

                let delay =
                    (base_backoff * 2u64.pow(attempt as u32)).min(MAX_BACKOFF_MS);
                ctx.logger
                    .info(&format!("Waiting {}ms before next attempt…", delay));
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
        }

        // All attempts exhausted — record the last result and propagate error.
        let last_error = last_result
            .as_ref()
            .and_then(|r| r.error.as_deref())
            .unwrap_or("unknown error");
        let final_msg = format!(
            "AgentLoopStage: all {} attempts failed. Last error: {}",
            max_retries + 1,
            last_error
        );
        if let Some(ref log) = ctx.workflow_log {
            let _ = log.log("error", "agent-loop", &final_msg).await;
        }
        if let Some(result) = last_result {
            ctx.agent_result = Some(result);
        }
        Err(anyhow::anyhow!("{}", final_msg))
    }
}

// ---------------------------------------------------------------------------
// Reflection generator
// ---------------------------------------------------------------------------

/// Generate a structured reflection on a failed attempt using the Claude CLI.
///
/// Loads `prompts/reflect.md`, substitutes variables from the failed result,
/// and calls the Claude CLI for a concise 3-5 sentence reflection.
/// Returns `None` if the reflection fails (best-effort — never blocks the retry).
async fn generate_reflection(
    ctx: &StageContext,
    base_task: &str,
    prior_result: Option<&RunResult>,
) -> Option<String> {
    let loader = PromptLoader::new(&ctx.harness_root);

    // Gather template variables.
    let mut vars = HashMap::new();
    vars.insert("task".to_string(), base_task.to_string());

    if let Some(result) = prior_result {
        let answer = if result.answer.len() > 1000 {
            format!("{}… (truncated)", &result.answer[..1000])
        } else {
            result.answer.clone()
        };
        vars.insert("agent_output".to_string(), answer);
        vars.insert(
            "error".to_string(),
            result.error.clone().unwrap_or_else(|| "(no error message)".into()),
        );
    } else {
        vars.insert("agent_output".to_string(), "(no output)".into());
        vars.insert("error".to_string(), "(unknown)".into());
    }

    // Get git diff --stat.
    let diff_stat = get_diff_stat(&ctx.workspace_dir).await;
    vars.insert("diff_stat".to_string(), diff_stat);

    // Load the reflection prompt template.
    let prompt = match loader.load("prompts/reflect.md", &vars).await {
        Ok(p) => p,
        Err(_) => {
            // Fallback inline template if the file is missing.
            format!(
                "Task: {}\n\nPrevious attempt failed with: {}\n\n\
                 Write a 3-5 sentence reflection: what went wrong and \
                 what should the next attempt do differently.",
                base_task,
                vars.get("error").map(|s| s.as_str()).unwrap_or("unknown")
            )
        }
    };

    // Call Claude CLI for the reflection (lightweight — text output, short).
    match call_claude_for_reflection(&prompt, ctx).await {
        Ok(text) => {
            let trimmed = text.trim().to_string();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            }
        }
        Err(e) => {
            ctx.logger
                .info(&format!("Reflection generation failed (continuing): {}", e));
            None
        }
    }
}

/// Call the Claude CLI with a short prompt and return the text response.
async fn call_claude_for_reflection(prompt: &str, ctx: &StageContext) -> Result<String> {
    let mut args = vec![
        "--print".to_string(),
        "--dangerously-skip-permissions".to_string(),
        "--output-format".to_string(),
        "text".to_string(),
    ];

    if let Some(ref sp) = ctx.system_prompt {
        args.push("--system-prompt".to_string());
        args.push(sp.clone());
    }

    args.push("-".to_string());

    let mut child = Command::new("claude")
        .args(&args)
        .current_dir(&ctx.workspace_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(prompt.as_bytes()).await?;
    }

    let output = tokio::time::timeout(Duration::from_secs(60), child.wait_with_output())
        .await
        .map_err(|_| anyhow::anyhow!("Reflection Claude CLI timed out"))??;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow::anyhow!("Claude CLI failed: {}", &stderr[..stderr.len().min(200)]));
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Run `git diff HEAD --stat` and return the output (best-effort).
async fn get_diff_stat(workspace_dir: &std::path::Path) -> String {
    match Command::new("git")
        .args(["diff", "HEAD", "--stat"])
        .current_dir(workspace_dir)
        .output()
        .await
    {
        Ok(output) => {
            let stat = String::from_utf8_lossy(&output.stdout);
            let stat = stat.trim();
            if stat.is_empty() {
                "(no changes)".into()
            } else {
                let lines: Vec<&str> = stat.lines().collect();
                if lines.len() > 30 {
                    format!("{}\n… ({} more files)", lines[..30].join("\n"), lines.len() - 30)
                } else {
                    stat.to_string()
                }
            }
        }
        Err(_) => "(git diff unavailable)".into(),
    }
}

// ---------------------------------------------------------------------------
// Resume context builder
// ---------------------------------------------------------------------------

/// Build an enriched task prompt for a retry.
///
/// When a `reflection` is available, it is used as the primary context signal
/// (replacing the raw prior answer dump).  When no reflection is available
/// (fallback), the prior answer and git diff stat are appended directly.
async fn build_resume_task_text(
    base_task: &str,
    prior_result: Option<&RunResult>,
    reflection: Option<&str>,
    workspace_dir: &std::path::Path,
) -> String {
    let mut sections: Vec<String> = Vec::new();

    // If we have a reflection, use it as the primary signal.
    if let Some(reflection) = reflection {
        sections.push(format!(
            "Reflection from previous failed attempt:\n{}",
            reflection
        ));
    } else if let Some(result) = prior_result {
        // Fallback: raw prior answer (same as the old behavior).
        if !result.answer.is_empty() {
            let truncated = if result.answer.len() > 1000 {
                format!("{}\n… (truncated)", &result.answer[..1000])
            } else {
                result.answer.clone()
            };
            sections.push(format!("Previous attempt output:\n{}", truncated));
        }
    }

    // Always append git diff stat so the agent knows what files were touched.
    let diff_stat = get_diff_stat(workspace_dir).await;
    if diff_stat != "(no changes)" && diff_stat != "(git diff unavailable)" {
        sections.push(format!(
            "Files changed by previous attempt (git diff --stat):\n{}",
            diff_stat
        ));
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
        let text =
            build_resume_task_text("Fix the bug", None, None, std::path::Path::new("/tmp")).await;
        assert!(text.starts_with("Fix the bug"));
    }

    #[tokio::test]
    async fn test_build_resume_task_with_prior_answer_no_reflection() {
        let prior = RunResult::failure("test", "err msg", vec![], 100);
        let text = build_resume_task_text(
            "Fix the bug",
            Some(&prior),
            None,
            std::path::Path::new("/tmp"),
        )
        .await;
        assert!(text.starts_with("Fix the bug"));
    }

    #[tokio::test]
    async fn test_build_resume_includes_prior_answer_when_no_reflection() {
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
            None,
            std::path::Path::new("/tmp"),
        )
        .await;
        assert!(text.contains("I tried doing X but it failed."));
        assert!(text.contains("Previous attempt output"));
    }

    #[tokio::test]
    async fn test_build_resume_with_reflection_replaces_raw_answer() {
        let prior = RunResult {
            technique: "test".into(),
            answer: "Raw output that should not appear.".into(),
            trajectory: vec![],
            llm_calls: 1,
            duration_ms: 500,
            success: false,
            error: Some("exit 1".into()),
        };
        let reflection = "The test failed because the import was wrong. \
                          Next attempt should check the module path first.";
        let text = build_resume_task_text(
            "Fix the bug",
            Some(&prior),
            Some(reflection),
            std::path::Path::new("/tmp"),
        )
        .await;

        // Reflection should be included.
        assert!(text.contains("Reflection from previous failed attempt"));
        assert!(text.contains("import was wrong"));
        // Raw answer should NOT be included when reflection is present.
        assert!(!text.contains("Raw output that should not appear"));
    }

    #[tokio::test]
    async fn test_build_resume_task_text_with_prior_answer_truncated() {
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
            None,
            std::path::Path::new("/tmp"),
        )
        .await;
        assert!(text.contains("Fix the bug"));
        assert!(text.contains("truncated"));
        let x_count = text.matches('X').count();
        assert!(x_count <= 1000, "Expected <= 1000 X's, got {}", x_count);
    }

    #[tokio::test]
    async fn test_build_resume_task_text_empty_answer() {
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
            None,
            std::path::Path::new("/tmp"),
        )
        .await;
        assert!(text.starts_with("Do the thing"));
        assert!(!text.contains("Previous attempt output"));
    }

    #[tokio::test]
    async fn test_build_resume_reflection_only_no_prior_result() {
        let text = build_resume_task_text(
            "Fix the bug",
            None,
            Some("Try a different approach."),
            std::path::Path::new("/tmp"),
        )
        .await;
        assert!(text.contains("Reflection from previous failed attempt"));
        assert!(text.contains("Try a different approach."));
    }

    #[test]
    fn test_agent_loop_stage_name() {
        assert_eq!(AgentLoopStage.name(), "agent-loop");
    }
}
