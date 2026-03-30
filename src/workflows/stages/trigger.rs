use anyhow::Result;
use async_trait::async_trait;
use tokio::process::Command;
use crate::workflows::stage::{Stage, StageContext};

/// Runs the configured shell-command trigger and stores its stdout in
/// `ctx.parsed_task.metadata["trigger_stdout"]` for the parse stage to read.
pub struct TriggerStage;

#[async_trait]
impl Stage for TriggerStage {
    fn name(&self) -> &str {
        "trigger"
    }

    async fn run(&self, ctx: &mut StageContext) -> Result<()> {
        let trigger = ctx
            .config
            .trigger
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("TriggerStage: no trigger configured"))?;

        ctx.logger
            .info(&format!("TriggerStage: running: {}", trigger.command));

        let timeout_ms = trigger.timeout_ms.unwrap_or(60_000);
        let cwd = trigger.cwd.clone().unwrap_or_else(|| ".".to_string());

        let output = tokio::time::timeout(
            std::time::Duration::from_millis(timeout_ms),
            Command::new("sh")
                .arg("-c")
                .arg(&trigger.command)
                .current_dir(&cwd)
                .output(),
        )
        .await
        .map_err(|_| {
            anyhow::anyhow!("Trigger command timed out after {}ms", timeout_ms)
        })??;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        ctx.logger
            .debug(&format!("TriggerStage stdout: {} bytes", stdout.len()));

        // Store stdout for the parse stage.
        let meta = ctx
            .parsed_task
            .metadata
            .get_or_insert_with(Default::default);
        meta.insert(
            "trigger_stdout".to_string(),
            serde_json::Value::String(stdout),
        );
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflows::workflow::ParsedTask;
    use crate::workflows::stage::{ResolvedPrompts, StageContext};
    use crate::tools::registry::ToolRegistry;
    use crate::types::config::{WorkflowConfig, TriggerConfig};
    use crate::utils::logger::Logger;
    use std::sync::Arc;

    fn make_ctx(command: &str) -> StageContext {
        let mut config = WorkflowConfig::default();
        config.trigger = Some(TriggerConfig {
            command: command.to_string(),
            cwd: None,
            timeout_ms: None,
        });
        StageContext {
            config: Arc::new(config),
            provider: Arc::new(crate::providers::anthropic::AnthropicProvider::new("test")),
            registry: Arc::new(ToolRegistry::new()),
            workspace_dir: std::path::PathBuf::from("/tmp"),
            branch: String::new(),
            parsed_task: ParsedTask::new(""),
            harness_root: std::path::PathBuf::from("/tmp"),
            prompts: ResolvedPrompts::default(),
            system_prompt: None,
            target_repo: None,
            logger: Logger::new("test"),
            run_id: "test-run".into(),
            retry_count: 0,
            review_rounds: 0,
            agent_result: None,
            review_result: None,
            review_feedback: None,
            issue_url: None,
            issue_display_id: None,
            pr_url: None,
            pr_title: None,
            pr_generated_body: None,
            reviewing_pr: None,
            pr_review_result: None,
            responding_pr: None,
            unaddressed_comments: None,
            pr_response_result: None,
            reviewer_login: None,
            workflow_log: None,
            aborted: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    fn make_ctx_no_trigger() -> StageContext {
        let config = WorkflowConfig::default();
        StageContext {
            config: Arc::new(config),
            provider: Arc::new(crate::providers::anthropic::AnthropicProvider::new("test")),
            registry: Arc::new(ToolRegistry::new()),
            workspace_dir: std::path::PathBuf::from("/tmp"),
            branch: String::new(),
            parsed_task: ParsedTask::new(""),
            harness_root: std::path::PathBuf::from("/tmp"),
            prompts: ResolvedPrompts::default(),
            system_prompt: None,
            target_repo: None,
            logger: Logger::new("test"),
            run_id: "test-run".into(),
            retry_count: 0,
            review_rounds: 0,
            agent_result: None,
            review_result: None,
            review_feedback: None,
            issue_url: None,
            issue_display_id: None,
            pr_url: None,
            pr_title: None,
            pr_generated_body: None,
            reviewing_pr: None,
            pr_review_result: None,
            responding_pr: None,
            unaddressed_comments: None,
            pr_response_result: None,
            reviewer_login: None,
            workflow_log: None,
            aborted: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    #[tokio::test]
    async fn test_trigger_captures_stdout() {
        let mut ctx = make_ctx("echo hello");
        TriggerStage.run(&mut ctx).await.unwrap();
        let stdout = ctx
            .parsed_task
            .metadata
            .as_ref()
            .unwrap()
            .get("trigger_stdout")
            .unwrap()
            .as_str()
            .unwrap();
        assert_eq!(stdout.trim(), "hello");
    }

    #[tokio::test]
    async fn test_trigger_missing_config_errors() {
        let mut ctx = make_ctx_no_trigger();
        let result = TriggerStage.run(&mut ctx).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("no trigger configured"));
    }

    #[tokio::test]
    async fn test_trigger_multi_line_output() {
        let mut ctx = make_ctx("printf 'line1\\nline2\\n'");
        TriggerStage.run(&mut ctx).await.unwrap();
        let stdout = ctx
            .parsed_task
            .metadata
            .as_ref()
            .unwrap()
            .get("trigger_stdout")
            .unwrap()
            .as_str()
            .unwrap();
        assert!(stdout.contains("line1"));
        assert!(stdout.contains("line2"));
    }
}
