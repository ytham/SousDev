use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A structured description of a single task parsed out of trigger output.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ParsedTask {
    /// The primary task description passed to the agent.
    pub task: String,
    /// Optional additional context appended after the main task.
    pub context: Option<String>,
    /// Arbitrary metadata (e.g. issue number, labels, trigger stdout).
    pub metadata: Option<HashMap<String, serde_json::Value>>,
}

impl ParsedTask {
    /// Create a task with only the required task string.
    pub fn new(task: impl Into<String>) -> Self {
        Self {
            task: task.into(),
            context: None,
            metadata: None,
        }
    }

    /// Return the full text passed to the agent: task + optional context block.
    pub fn full_text(&self) -> String {
        match &self.context {
            Some(ctx) => format!("{}\n\nAdditional context:\n{}", self.task, ctx),
            None => self.task.clone(),
        }
    }
}

// `WorkflowResult` is defined in stores.rs; re-export for downstream convenience.
pub use crate::workflows::stores::WorkflowResult;

/// Build a [`WorkflowResult`] that represents a skipped (no-op) run.
///
/// `success` is `true` when `error` is `None` — a deliberate skip (e.g.
/// parser returned `None`) is not treated as a failure, only an unexpected
/// error during the skip check is.
pub fn make_skipped_result(
    workflow_name: &str,
    run_id: &str,
    started_at: &str,
    error: Option<&str>,
) -> WorkflowResult {
    WorkflowResult {
        workflow_name: workflow_name.to_string(),
        run_id: run_id.to_string(),
        started_at: started_at.to_string(),
        completed_at: chrono::Utc::now().to_rfc3339(),
        success: error.is_none(),
        skipped: true,
        error: error.map(|e| e.to_string()),
        retry_count: 0,
        review_rounds: 0,
        trajectory: vec![],
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parsed_task_new() {
        let t = ParsedTask::new("Fix the bug");
        assert_eq!(t.task, "Fix the bug");
        assert!(t.context.is_none());
        assert!(t.metadata.is_none());
    }

    #[test]
    fn test_full_text_no_context() {
        let t = ParsedTask::new("Fix the bug");
        assert_eq!(t.full_text(), "Fix the bug");
    }

    #[test]
    fn test_full_text_with_context() {
        let mut t = ParsedTask::new("Fix the bug");
        t.context = Some("Run tests with: pnpm test".to_string());
        let full = t.full_text();
        assert!(full.starts_with("Fix the bug"));
        assert!(full.contains("Additional context:"));
        assert!(full.contains("pnpm test"));
    }

    #[test]
    fn test_parsed_task_serde_roundtrip() {
        let mut t = ParsedTask::new("task");
        t.context = Some("ctx".into());
        let mut meta = HashMap::new();
        meta.insert("key".to_string(), serde_json::Value::Bool(true));
        t.metadata = Some(meta);
        let json = serde_json::to_string(&t).unwrap();
        let decoded: ParsedTask = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.task, "task");
        assert_eq!(decoded.context.as_deref(), Some("ctx"));
    }

    #[test]
    fn test_make_skipped_result_no_error() {
        let r = make_skipped_result("my-pipe", "run-1", "2026-01-01T00:00:00Z", None);
        assert!(r.success);
        assert!(r.skipped);
        assert!(r.error.is_none());
        assert_eq!(r.workflow_name, "my-pipe");
        assert_eq!(r.run_id, "run-1");
    }

    #[test]
    fn test_make_skipped_result_with_error() {
        let r = make_skipped_result("my-pipe", "run-1", "2026-01-01T00:00:00Z", Some("oops"));
        assert!(!r.success);
        assert!(r.skipped);
        assert_eq!(r.error.as_deref(), Some("oops"));
    }

    #[test]
    fn test_make_skipped_result_has_completed_at() {
        let r = make_skipped_result("p", "r", "2026-01-01T00:00:00Z", None);
        assert!(!r.completed_at.is_empty());
    }
}
