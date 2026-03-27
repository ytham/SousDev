use anyhow::Result;
use async_trait::async_trait;
use crate::workflows::workflow::ParsedTask;
use crate::workflows::stage::{Stage, StageContext};

// ---------------------------------------------------------------------------
// Skip signal
// ---------------------------------------------------------------------------

/// Sentinel error — thrown by a parser to signal "nothing to do this tick".
///
/// The executor treats this as a deliberate skip (marks the run as
/// `skipped = true, success = true`) rather than a failure.
#[derive(Debug, thiserror::Error)]
#[error("SkipWorkflowSignal: parser returned None — nothing to process this tick")]
pub struct SkipWorkflowSignal;

// ---------------------------------------------------------------------------
// ParseStage
// ---------------------------------------------------------------------------

/// Calls a user-supplied parser closure on the trigger stdout and stores the
/// result in `ctx.parsed_task`.
///
/// The parser receives the raw trigger stdout string and may return:
/// - `Some(ParsedTask)` — proceed with this task.
/// - `None` — skip this run (a [`SkipWorkflowSignal`] error is returned).
pub struct ParseStage<F>
where
    F: Fn(&str) -> Option<ParsedTask> + Send + Sync,
{
    /// The parsing function.
    pub parser: F,
}

#[async_trait]
impl<F> Stage for ParseStage<F>
where
    F: Fn(&str) -> Option<ParsedTask> + Send + Sync,
{
    fn name(&self) -> &str {
        "parse"
    }

    async fn run(&self, ctx: &mut StageContext) -> Result<()> {
        let stdout = ctx
            .parsed_task
            .metadata
            .as_ref()
            .and_then(|m| m.get("trigger_stdout"))
            .and_then(|v| v.as_str())
            .unwrap_or("");

        match (self.parser)(stdout) {
            Some(task) => {
                ctx.parsed_task = task;
                Ok(())
            }
            None => Err(SkipWorkflowSignal.into()),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflows::stage::{ResolvedPrompts, StageContext};
    use crate::tools::registry::ToolRegistry;
    use crate::types::config::WorkflowConfig;
    use crate::utils::logger::Logger;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn make_ctx_with_stdout(stdout: &str) -> StageContext {
        let mut meta = HashMap::new();
        meta.insert(
            "trigger_stdout".to_string(),
            serde_json::Value::String(stdout.to_string()),
        );
        StageContext {
            config: Arc::new(WorkflowConfig::default()),
            provider: Arc::new(crate::providers::anthropic::AnthropicProvider::new("test")),
            registry: Arc::new(ToolRegistry::new()),
            workspace_dir: std::path::PathBuf::from("/tmp"),
            branch: String::new(),
            parsed_task: ParsedTask {
                task: String::new(),
                context: None,
                metadata: Some(meta),
            },
            harness_root: std::path::PathBuf::from("/tmp"),
            prompts: ResolvedPrompts::default(),
            target_repo: None,
            logger: Logger::new("test"),
            run_id: "test-run".into(),
            retry_count: 0,
            review_rounds: 0,
            agent_result: None,
            review_result: None,
            review_feedback: None,
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
    async fn test_parse_returns_task() {
        let stage = ParseStage {
            parser: |s: &str| {
                if s.contains("FAIL") {
                    Some(ParsedTask::new("Fix failing tests"))
                } else {
                    None
                }
            },
        };
        let mut ctx = make_ctx_with_stdout("FAIL src/auth.test.ts");
        stage.run(&mut ctx).await.unwrap();
        assert_eq!(ctx.parsed_task.task, "Fix failing tests");
    }

    #[tokio::test]
    async fn test_parse_none_returns_skip_signal() {
        let stage = ParseStage { parser: |_| None };
        let mut ctx = make_ctx_with_stdout("all tests pass");
        let result = stage.run(&mut ctx).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        // Verify it downcasts to SkipWorkflowSignal.
        assert!(
            err.downcast_ref::<SkipWorkflowSignal>().is_some(),
            "expected SkipWorkflowSignal, got: {}",
            err
        );
    }

    #[tokio::test]
    async fn test_parse_no_metadata_treats_as_empty() {
        let stage = ParseStage {
            parser: |s: &str| {
                if s.is_empty() {
                    Some(ParsedTask::new("empty task"))
                } else {
                    None
                }
            },
        };
        // No metadata at all — stdout falls back to "".
        // Parser returns Some("empty task") for empty string → Ok(()).
        let mut ctx = make_ctx_with_stdout("anything");
        ctx.parsed_task.metadata = None;
        let result = stage.run(&mut ctx).await;
        // Empty string → parser returns Some → task is set successfully.
        assert!(result.is_ok());
        assert_eq!(ctx.parsed_task.task, "empty task");
    }

    #[tokio::test]
    async fn test_parse_preserves_task_context() {
        let stage = ParseStage {
            parser: |_| {
                let mut t = ParsedTask::new("task");
                t.context = Some("extra ctx".into());
                Some(t)
            },
        };
        let mut ctx = make_ctx_with_stdout("anything");
        stage.run(&mut ctx).await.unwrap();
        assert_eq!(ctx.parsed_task.context.as_deref(), Some("extra ctx"));
    }
}
