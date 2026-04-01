use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::fs;

/// All SousDev state files live under this subdirectory relative to the
/// project root. This keeps them out of the top-level directory and makes
/// gitignoring trivial (`output/`).
const OUTPUT_DIR: &str = "output";

// ---- RunStore ---------------------------------------------------------------

const RUN_STORE_FILE: &str = "runs.json";

/// The result of a single workflow run, persisted by [`RunStore`].
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WorkflowResult {
    #[serde(alias = "pipeline_name")]
    pub workflow_name: String,
    pub run_id: String,
    pub started_at: String,
    pub completed_at: String,
    pub success: bool,
    pub skipped: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_number: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issue_number: Option<u64>,
    pub retry_count: usize,
    pub review_rounds: usize,
    pub trajectory: Vec<crate::types::technique::TrajectoryStep>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_result: Option<crate::types::technique::RunResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_result: Option<crate::techniques::critique_loop::CritiqueLoopResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_review_result: Option<PrReviewResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_response_result: Option<PrResponseResult>,
}

/// Summary of a PR review pass stored inside a [`WorkflowResult`].
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PrReviewResult {
    pub inline_comment_count: usize,
    pub summary_posted: bool,
    pub head_sha: String,
    pub errors: Vec<String>,
}

/// Summary of a PR response pass stored inside a [`WorkflowResult`].
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PrResponseResult {
    pub inline_replies_posted: usize,
    pub summary_posted: bool,
    pub new_head_sha: String,
    pub errors: Vec<String>,
}

/// Append-only store for [`WorkflowResult`] records, written to
/// `.harness-runs.json` in the configured directory.
pub struct RunStore {
    file_path: PathBuf,
}

impl RunStore {
    /// Create a [`RunStore`] that persists in `dir/output/`.
    pub fn new(dir: &Path) -> Self {
        Self {
            file_path: dir.join(OUTPUT_DIR).join(RUN_STORE_FILE),
        }
    }

    /// Append a result to the store.
    pub async fn append(&self, result: &WorkflowResult) -> Result<()> {
        let mut all = self.read_all().await.unwrap_or_default();
        all.push(result.clone());
        if let Some(parent) = self.file_path.parent() {
            fs::create_dir_all(parent).await?;
        }
        fs::write(&self.file_path, serde_json::to_string_pretty(&all)?).await?;
        Ok(())
    }

    /// Return the most recent `limit` results, optionally filtered by
    /// `workflow_name`.
    pub async fn get_history(
        &self,
        workflow_name: Option<&str>,
        limit: usize,
    ) -> Result<Vec<WorkflowResult>> {
        let all = self.read_all().await.unwrap_or_default();
        let filtered: Vec<WorkflowResult> = match workflow_name {
            Some(name) => all.into_iter().filter(|r| r.workflow_name == name).collect(),
            None => all,
        };
        let len = filtered.len();
        Ok(filtered
            .into_iter()
            .skip(len.saturating_sub(limit))
            .collect())
    }

    async fn read_all(&self) -> Result<Vec<WorkflowResult>> {
        if !self.file_path.exists() {
            return Ok(vec![]);
        }
        let raw = fs::read_to_string(&self.file_path).await?;
        if raw.trim().is_empty() {
            return Ok(vec![]);
        }
        Ok(serde_json::from_str(&raw)?)
    }
}

// ---- HandledIssueStore -------------------------------------------------------

const HANDLED_ISSUES_FILE: &str = "handled-issues.json";

/// A record written when an issue has been successfully handled by a workflow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandledIssueRecord {
    pub pr_number: Option<u64>,
    pub issue_url: String,
    pub issue_title: String,
    pub issue_repo: String,
    pub pr_url: Option<String>,
    pub pr_open: bool,
    pub handled_at: String,
    pub updated_at: String,
}

/// Tracks which issues each workflow has already handled, preventing duplicate
/// processing across cron ticks.
///
/// Persisted to `.harness-handled-issues.json`.  The file schema is
/// `{ workflow_name: { issue_number_str: HandledIssueRecord } }`.
pub struct HandledIssueStore {
    file_path: PathBuf,
}

impl HandledIssueStore {
    /// Create a [`HandledIssueStore`] that persists in `dir/output/`.
    pub fn new(dir: &Path) -> Self {
        Self {
            file_path: dir.join(OUTPUT_DIR).join(HANDLED_ISSUES_FILE),
        }
    }

    /// Return `true` when `workflow_name` has already handled `issue_number`.
    pub async fn is_handled(&self, workflow_name: &str, issue_number: u64) -> Result<bool> {
        let data = self.read_all().await.unwrap_or_default();
        Ok(data
            .get(workflow_name)
            .and_then(|m| m.get(&issue_number.to_string()))
            .is_some())
    }

    /// Mark an issue as handled, deriving the issue number from the URL's last
    /// path segment.  Prefer [`mark_handled_with_number`] when the number is
    /// already known.
    pub async fn mark_handled(
        &self,
        workflow_name: &str,
        record: HandledIssueRecord,
    ) -> Result<()> {
        let issue_number = record
            .issue_url
            .split('/')
            .next_back()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        let mut data = self.read_all().await.unwrap_or_default();
        data.entry(workflow_name.to_string())
            .or_default()
            .insert(issue_number.to_string(), record);
        self.write_all(&data).await
    }

    /// Mark an issue as handled using an explicit `number`.
    pub async fn mark_handled_with_number(
        &self,
        workflow_name: &str,
        number: u64,
        record: HandledIssueRecord,
    ) -> Result<()> {
        let mut data = self.read_all().await.unwrap_or_default();
        data.entry(workflow_name.to_string())
            .or_default()
            .insert(number.to_string(), record);
        self.write_all(&data).await
    }

    /// Remove a previously-handled issue (e.g. to allow reprocessing).
    pub async fn unmark_handled(&self, workflow_name: &str, issue_number: u64) -> Result<()> {
        let mut data = self.read_all().await.unwrap_or_default();
        if let Some(m) = data.get_mut(workflow_name) {
            m.remove(&issue_number.to_string());
        }
        self.write_all(&data).await
    }

    /// Return all handled-issue records for a workflow.
    pub async fn get_all_records(
        &self,
        workflow_name: &str,
    ) -> Result<HashMap<String, HandledIssueRecord>> {
        let data = self.read_all().await.unwrap_or_default();
        Ok(data.get(workflow_name).cloned().unwrap_or_default())
    }

    async fn read_all(
        &self,
    ) -> Result<HashMap<String, HashMap<String, HandledIssueRecord>>> {
        if !self.file_path.exists() {
            return Ok(HashMap::new());
        }
        let raw = fs::read_to_string(&self.file_path).await?;
        if raw.trim().is_empty() {
            return Ok(HashMap::new());
        }
        Ok(serde_json::from_str(&raw)?)
    }

    async fn write_all(
        &self,
        data: &HashMap<String, HashMap<String, HandledIssueRecord>>,
    ) -> Result<()> {
        if let Some(parent) = self.file_path.parent() {
            fs::create_dir_all(parent).await?;
        }
        fs::write(&self.file_path, serde_json::to_string_pretty(data)?).await?;
        Ok(())
    }
}

// ---- PRReviewStore -----------------------------------------------------------

const PR_REVIEW_FILE: &str = "reviewed-prs.json";

/// A record written when a workflow has reviewed a pull request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrReviewRecord {
    pub pr_number: u64,
    pub pr_url: String,
    pub pr_title: String,
    pub pr_repo: String,
    /// The HEAD commit SHA that was reviewed.
    pub head_sha: String,
    /// Lines added at time of review (for detecting real changes vs rebases).
    #[serde(default)]
    pub additions: u64,
    /// Lines deleted at time of review.
    #[serde(default)]
    pub deletions: u64,
    /// The highest comment ID seen during this review pass (used as a cursor
    /// for the next pass).
    pub last_comment_id: u64,
    pub reviewed_at: String,
}

/// Tracks which PRs each workflow has reviewed and at which HEAD SHA, enabling
/// incremental review on subsequent pushes.
///
/// Persisted to `.harness-reviewed-prs.json`.
pub struct PrReviewStore {
    file_path: PathBuf,
}

impl PrReviewStore {
    /// Create a [`PrReviewStore`] that persists in `dir/output/`.
    pub fn new(dir: &Path) -> Self {
        Self {
            file_path: dir.join(OUTPUT_DIR).join(PR_REVIEW_FILE),
        }
    }

    /// Retrieve the stored review record for `pr_number` under `workflow_name`,
    /// returning `None` if no record exists.
    pub async fn get_record(
        &self,
        workflow_name: &str,
        pr_number: u64,
    ) -> Result<Option<PrReviewRecord>> {
        let data = self.read_all().await.unwrap_or_default();
        Ok(data
            .get(workflow_name)
            .and_then(|m| m.get(&pr_number.to_string()))
            .cloned())
    }

    /// Upsert a review record for `workflow_name`.
    pub async fn mark_reviewed(
        &self,
        workflow_name: &str,
        record: PrReviewRecord,
    ) -> Result<()> {
        let mut data = self.read_all().await.unwrap_or_default();
        data.entry(workflow_name.to_string())
            .or_default()
            .insert(record.pr_number.to_string(), record);
        self.write_all(&data).await
    }

    /// Remove the review record for `pr_number` under `workflow_name`.
    pub async fn remove_record(&self, workflow_name: &str, pr_number: u64) -> Result<()> {
        let mut data = self.read_all().await.unwrap_or_default();
        if let Some(m) = data.get_mut(workflow_name) {
            m.remove(&pr_number.to_string());
        }
        self.write_all(&data).await
    }

    /// Return all PR review records for a workflow.
    pub async fn get_all_records(
        &self,
        workflow_name: &str,
    ) -> Result<HashMap<String, PrReviewRecord>> {
        let data = self.read_all().await.unwrap_or_default();
        Ok(data.get(workflow_name).cloned().unwrap_or_default())
    }

    async fn read_all(&self) -> Result<HashMap<String, HashMap<String, PrReviewRecord>>> {
        if !self.file_path.exists() {
            return Ok(HashMap::new());
        }
        let raw = fs::read_to_string(&self.file_path).await?;
        if raw.trim().is_empty() {
            return Ok(HashMap::new());
        }
        Ok(serde_json::from_str(&raw)?)
    }

    async fn write_all(
        &self,
        data: &HashMap<String, HashMap<String, PrReviewRecord>>,
    ) -> Result<()> {
        if let Some(parent) = self.file_path.parent() {
            fs::create_dir_all(parent).await?;
        }
        fs::write(&self.file_path, serde_json::to_string_pretty(data)?).await?;
        Ok(())
    }
}

// ---- PRResponseStore ---------------------------------------------------------

const PR_RESPONSE_FILE: &str = "pr-responses.json";

/// A record written when a workflow has responded to review comments on a PR.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrResponseRecord {
    pub pr_number: u64,
    pub pr_url: String,
    pub pr_repo: String,
    /// The HEAD commit SHA at the time of the response.
    pub head_sha: String,
    /// Highest inline review comment ID seen (cursor for next pass).
    pub last_inline_comment_id: u64,
    /// Highest timeline comment ID seen (cursor for next pass).
    pub last_timeline_comment_id: u64,
    pub responded_at: String,
}

/// Tracks which PRs each workflow has already responded to, and the cursor
/// positions for detecting new comments on subsequent cron ticks.
///
/// Persisted to `.harness-pr-responses.json`.
pub struct PrResponseStore {
    file_path: PathBuf,
}

impl PrResponseStore {
    /// Create a [`PrResponseStore`] that persists in `dir/output/`.
    pub fn new(dir: &Path) -> Self {
        Self {
            file_path: dir.join(OUTPUT_DIR).join(PR_RESPONSE_FILE),
        }
    }

    /// Retrieve the stored response record for `pr_number` under
    /// `workflow_name`, returning `None` if no record exists.
    pub async fn get_record(
        &self,
        workflow_name: &str,
        pr_number: u64,
    ) -> Result<Option<PrResponseRecord>> {
        let data = self.read_all().await.unwrap_or_default();
        Ok(data
            .get(workflow_name)
            .and_then(|m| m.get(&pr_number.to_string()))
            .cloned())
    }

    /// Upsert a response record for `workflow_name`.
    pub async fn mark_responded(
        &self,
        workflow_name: &str,
        record: PrResponseRecord,
    ) -> Result<()> {
        let mut data = self.read_all().await.unwrap_or_default();
        data.entry(workflow_name.to_string())
            .or_default()
            .insert(record.pr_number.to_string(), record);
        self.write_all(&data).await
    }

    /// Remove the response record for `pr_number` under `workflow_name`.
    pub async fn remove_record(&self, workflow_name: &str, pr_number: u64) -> Result<()> {
        let mut data = self.read_all().await.unwrap_or_default();
        if let Some(m) = data.get_mut(workflow_name) {
            m.remove(&pr_number.to_string());
        }
        self.write_all(&data).await
    }

    /// Return all PR response records for a workflow.
    pub async fn get_all_records(
        &self,
        workflow_name: &str,
    ) -> Result<HashMap<String, PrResponseRecord>> {
        let data = self.read_all().await.unwrap_or_default();
        Ok(data.get(workflow_name).cloned().unwrap_or_default())
    }

    async fn read_all(
        &self,
    ) -> Result<HashMap<String, HashMap<String, PrResponseRecord>>> {
        if !self.file_path.exists() {
            return Ok(HashMap::new());
        }
        let raw = fs::read_to_string(&self.file_path).await?;
        if raw.trim().is_empty() {
            return Ok(HashMap::new());
        }
        Ok(serde_json::from_str(&raw)?)
    }

    async fn write_all(
        &self,
        data: &HashMap<String, HashMap<String, PrResponseRecord>>,
    ) -> Result<()> {
        if let Some(parent) = self.file_path.parent() {
            fs::create_dir_all(parent).await?;
        }
        fs::write(&self.file_path, serde_json::to_string_pretty(data)?).await?;
        Ok(())
    }
}

// ---- Tests ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn now() -> String {
        chrono::Utc::now().to_rfc3339()
    }

    // RunStore tests

    #[tokio::test]
    async fn test_run_store_append_and_history() {
        let dir = TempDir::new().unwrap();
        let store = RunStore::new(dir.path());
        let result = WorkflowResult {
            workflow_name: "test".into(),
            run_id: "run-1".into(),
            started_at: now(),
            completed_at: now(),
            success: true,
            skipped: false,
            ..Default::default()
        };
        store.append(&result).await.unwrap();
        let history = store.get_history(Some("test"), 10).await.unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].run_id, "run-1");
    }

    #[tokio::test]
    async fn test_run_store_empty() {
        let dir = TempDir::new().unwrap();
        let store = RunStore::new(dir.path());
        let history = store.get_history(None, 10).await.unwrap();
        assert!(history.is_empty());
    }

    #[tokio::test]
    async fn test_run_store_filters_by_workflow() {
        let dir = TempDir::new().unwrap();
        let store = RunStore::new(dir.path());
        for name in &["pipe-a", "pipe-b", "pipe-a"] {
            store
                .append(&WorkflowResult {
                    workflow_name: name.to_string(),
                    run_id: uuid::Uuid::new_v4().to_string(),
                    started_at: now(),
                    completed_at: now(),
                    success: true,
                    skipped: false,
                    ..Default::default()
                })
                .await
                .unwrap();
        }
        let history = store.get_history(Some("pipe-a"), 10).await.unwrap();
        assert_eq!(history.len(), 2);
    }

    // HandledIssueStore tests

    #[tokio::test]
    async fn test_handled_issue_store_mark_and_check() {
        let dir = TempDir::new().unwrap();
        let store = HandledIssueStore::new(dir.path());
        assert!(!store.is_handled("my-pipeline", 42).await.unwrap());
        store
            .mark_handled_with_number(
                "my-pipeline",
                42,
                HandledIssueRecord {
                    pr_number: Some(1),
                    issue_url: "https://github.com/owner/repo/issues/42".into(),
                    issue_title: "Bug".into(),
                    issue_repo: "owner/repo".into(),
                    pr_url: Some("https://github.com/owner/repo/pull/1".into()),
                    pr_open: true,
                    handled_at: now(),
                    updated_at: now(),
                },
            )
            .await
            .unwrap();
        assert!(store.is_handled("my-pipeline", 42).await.unwrap());
    }

    #[tokio::test]
    async fn test_handled_issue_store_unmark() {
        let dir = TempDir::new().unwrap();
        let store = HandledIssueStore::new(dir.path());
        store
            .mark_handled_with_number(
                "my-pipeline",
                42,
                HandledIssueRecord {
                    pr_number: None,
                    issue_url: "u".into(),
                    issue_title: "t".into(),
                    issue_repo: "r".into(),
            pr_url: None,
                    pr_open: false,
                    handled_at: now(),
                    updated_at: now(),
                },
            )
            .await
            .unwrap();
        store.unmark_handled("my-pipeline", 42).await.unwrap();
        assert!(!store.is_handled("my-pipeline", 42).await.unwrap());
    }

    #[tokio::test]
    async fn test_handled_issue_store_cross_workflow_isolation() {
        let dir = TempDir::new().unwrap();
        let store = HandledIssueStore::new(dir.path());
        store
            .mark_handled_with_number(
                "pipe-a",
                1,
                HandledIssueRecord {
                    pr_number: None,
                    issue_url: "u".into(),
                    issue_title: "t".into(),
                    issue_repo: "r".into(),
            pr_url: None,
                    pr_open: false,
                    handled_at: now(),
                    updated_at: now(),
                },
            )
            .await
            .unwrap();
        assert!(store.is_handled("pipe-a", 1).await.unwrap());
        assert!(!store.is_handled("pipe-b", 1).await.unwrap());
    }

    // PRReviewStore tests

    #[tokio::test]
    async fn test_pr_review_store_get_none() {
        let dir = TempDir::new().unwrap();
        let store = PrReviewStore::new(dir.path());
        assert!(store.get_record("p", 42).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_pr_review_store_roundtrip() {
        let dir = TempDir::new().unwrap();
        let store = PrReviewStore::new(dir.path());
        let record = PrReviewRecord {
            pr_number: 42,
            pr_url: "u".into(),
            pr_title: "t".into(),
            pr_repo: "owner/repo".into(),
            head_sha: "abc1234".into(),
                            additions: 0,
                            deletions: 0,
            last_comment_id: 100,
            reviewed_at: now(),
        };
        store.mark_reviewed("my-pipeline", record.clone()).await.unwrap();
        let retrieved = store.get_record("my-pipeline", 42).await.unwrap().unwrap();
        assert_eq!(retrieved.head_sha, "abc1234");
        assert_eq!(retrieved.last_comment_id, 100);
    }

    #[tokio::test]
    async fn test_pr_review_store_overwrite() {
        let dir = TempDir::new().unwrap();
        let store = PrReviewStore::new(dir.path());
        store
            .mark_reviewed(
                "p",
                PrReviewRecord {
                    pr_number: 42,
                    pr_url: "u".into(),
                    pr_title: "t".into(),
                    pr_repo: "r".into(),
                    head_sha: "old".into(),
                            additions: 0,
                            deletions: 0,
                    last_comment_id: 10,
                    reviewed_at: now(),
                },
            )
            .await
            .unwrap();
        store
            .mark_reviewed(
                "p",
                PrReviewRecord {
                    pr_number: 42,
                    pr_url: "u".into(),
                    pr_title: "t".into(),
                    pr_repo: "r".into(),
                    head_sha: "new".into(),
                            additions: 0,
                            deletions: 0,
                    last_comment_id: 20,
                    reviewed_at: now(),
                },
            )
            .await
            .unwrap();
        let rec = store.get_record("p", 42).await.unwrap().unwrap();
        assert_eq!(rec.head_sha, "new");
        assert_eq!(rec.last_comment_id, 20);
    }

    #[tokio::test]
    async fn test_pr_review_store_remove() {
        let dir = TempDir::new().unwrap();
        let store = PrReviewStore::new(dir.path());
        store
            .mark_reviewed(
                "p",
                PrReviewRecord {
                    pr_number: 42,
                    pr_url: "u".into(),
                    pr_title: "t".into(),
                    pr_repo: "r".into(),
                    head_sha: "s".into(),
                            additions: 0,
                            deletions: 0,
                    last_comment_id: 0,
                    reviewed_at: now(),
                },
            )
            .await
            .unwrap();
        store.remove_record("p", 42).await.unwrap();
        assert!(store.get_record("p", 42).await.unwrap().is_none());
    }

    // PRResponseStore tests

    #[tokio::test]
    async fn test_pr_response_store_roundtrip() {
        let dir = TempDir::new().unwrap();
        let store = PrResponseStore::new(dir.path());
        assert!(store.get_record("p", 42).await.unwrap().is_none());
        store
            .mark_responded(
                "p",
                PrResponseRecord {
                    pr_number: 42,
                    pr_url: "u".into(),
                    pr_repo: "r".into(),
                    head_sha: "sha".into(),
                    last_inline_comment_id: 500,
                    last_timeline_comment_id: 20,
                    responded_at: now(),
                },
            )
            .await
            .unwrap();
        let rec = store.get_record("p", 42).await.unwrap().unwrap();
        assert_eq!(rec.last_inline_comment_id, 500);
        assert_eq!(rec.last_timeline_comment_id, 20);
    }

    #[tokio::test]
    async fn test_pr_response_store_overwrite() {
        let dir = TempDir::new().unwrap();
        let store = PrResponseStore::new(dir.path());
        store
            .mark_responded(
                "p",
                PrResponseRecord {
                    pr_number: 42,
                    pr_url: "u".into(),
                    pr_repo: "r".into(),
                    head_sha: "old".into(),
                    last_inline_comment_id: 10,
                    last_timeline_comment_id: 5,
                    responded_at: now(),
                },
            )
            .await
            .unwrap();
        store
            .mark_responded(
                "p",
                PrResponseRecord {
                    pr_number: 42,
                    pr_url: "u".into(),
                    pr_repo: "r".into(),
                    head_sha: "new".into(),
                    last_inline_comment_id: 99,
                    last_timeline_comment_id: 50,
                    responded_at: now(),
                },
            )
            .await
            .unwrap();
        let rec = store.get_record("p", 42).await.unwrap().unwrap();
        assert_eq!(rec.head_sha, "new");
        assert_eq!(rec.last_inline_comment_id, 99);
    }

    #[tokio::test]
    async fn test_pr_response_store_cross_workflow_isolation() {
        let dir = TempDir::new().unwrap();
        let store = PrResponseStore::new(dir.path());
        store
            .mark_responded(
                "p-a",
                PrResponseRecord {
                    pr_number: 1,
                    pr_url: "u".into(),
                    pr_repo: "r".into(),
                    head_sha: "s".into(),
                    last_inline_comment_id: 0,
                    last_timeline_comment_id: 0,
                    responded_at: now(),
                },
            )
            .await
            .unwrap();
        assert!(store.get_record("p-a", 1).await.unwrap().is_some());
        assert!(store.get_record("p-b", 1).await.unwrap().is_none());
    }

    // ── RunStore additional tests ────────────────────────────────────────────

    #[tokio::test]
    async fn test_run_store_limit_respects_count() {
        let dir = TempDir::new().unwrap();
        let store = RunStore::new(dir.path());
        for i in 0..5 {
            store
                .append(&WorkflowResult {
                    workflow_name: "pipe".into(),
                    run_id: format!("run-{}", i),
                    started_at: now(),
                    completed_at: now(),
                    success: true,
                    skipped: false,
                    ..Default::default()
                })
                .await
                .unwrap();
        }
        let history = store.get_history(None, 2).await.unwrap();
        assert_eq!(history.len(), 2);
        // The last 2 entries should be run-3 and run-4.
        assert_eq!(history[0].run_id, "run-3");
        assert_eq!(history[1].run_id, "run-4");
    }

    #[tokio::test]
    async fn test_run_store_persists_across_instances() {
        let dir = TempDir::new().unwrap();
        {
            let store = RunStore::new(dir.path());
            store
                .append(&WorkflowResult {
                    workflow_name: "persist".into(),
                    run_id: "r1".into(),
                    started_at: now(),
                    completed_at: now(),
                    success: true,
                    skipped: false,
                    ..Default::default()
                })
                .await
                .unwrap();
        }
        // New instance from the same directory should see the data.
        let store2 = RunStore::new(dir.path());
        let history = store2.get_history(Some("persist"), 10).await.unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].run_id, "r1");
    }

    #[tokio::test]
    async fn test_run_store_get_last_run() {
        let dir = TempDir::new().unwrap();
        let store = RunStore::new(dir.path());
        // Empty store → no last run.
        let history = store.get_history(None, 1).await.unwrap();
        assert!(history.is_empty());
        // Append two runs, get_history(limit=1) returns the most recent.
        store
            .append(&WorkflowResult {
                workflow_name: "p".into(),
                run_id: "old".into(),
                started_at: now(),
                completed_at: now(),
                success: true,
                skipped: false,
                ..Default::default()
            })
            .await
            .unwrap();
        store
            .append(&WorkflowResult {
                workflow_name: "p".into(),
                run_id: "new".into(),
                started_at: now(),
                completed_at: now(),
                success: false,
                skipped: false,
                ..Default::default()
            })
            .await
            .unwrap();
        let last = store.get_history(None, 1).await.unwrap();
        assert_eq!(last.len(), 1);
        assert_eq!(last[0].run_id, "new");
    }

    #[tokio::test]
    async fn test_run_store_get_workflow_names() {
        let dir = TempDir::new().unwrap();
        let store = RunStore::new(dir.path());
        for name in &["alpha", "beta", "alpha", "gamma"] {
            store
                .append(&WorkflowResult {
                    workflow_name: name.to_string(),
                    run_id: uuid::Uuid::new_v4().to_string(),
                    started_at: now(),
                    completed_at: now(),
                    success: true,
                    skipped: false,
                    ..Default::default()
                })
                .await
                .unwrap();
        }
        let all = store.get_history(None, 100).await.unwrap();
        let names: std::collections::HashSet<&str> =
            all.iter().map(|r| r.workflow_name.as_str()).collect();
        assert_eq!(names.len(), 3);
        assert!(names.contains("alpha"));
        assert!(names.contains("beta"));
        assert!(names.contains("gamma"));
    }

    #[tokio::test]
    async fn test_run_store_append_preserves_all_fields() {
        let dir = TempDir::new().unwrap();
        let store = RunStore::new(dir.path());
        let result = WorkflowResult {
            workflow_name: "full-pipeline".into(),
            run_id: "run-999".into(),
            started_at: "2025-01-01T00:00:00Z".into(),
            completed_at: "2025-01-01T01:00:00Z".into(),
            success: true,
            skipped: false,
            pr_url: Some("https://github.com/o/r/pull/42".into()),
            pr_title: Some("Fix the bug".into()),
            pr_number: Some(42),
            error: None,
            issue_number: Some(99),
            retry_count: 2,
            review_rounds: 1,
            trajectory: vec![],
            agent_result: None,
            review_result: None,
            pr_review_result: Some(PrReviewResult {
                inline_comment_count: 3,
                summary_posted: true,
                head_sha: "abc".into(),
                errors: vec![],
            }),
            pr_response_result: None,
        };
        store.append(&result).await.unwrap();
        let history = store.get_history(None, 10).await.unwrap();
        assert_eq!(history.len(), 1);
        let r = &history[0];
        assert_eq!(r.workflow_name, "full-pipeline");
        assert_eq!(r.run_id, "run-999");
        assert_eq!(r.started_at, "2025-01-01T00:00:00Z");
        assert_eq!(r.completed_at, "2025-01-01T01:00:00Z");
        assert!(r.success);
        assert!(!r.skipped);
        assert_eq!(r.pr_url.as_deref(), Some("https://github.com/o/r/pull/42"));
        assert_eq!(r.pr_title.as_deref(), Some("Fix the bug"));
        assert_eq!(r.pr_number, Some(42));
        assert!(r.error.is_none());
        assert_eq!(r.issue_number, Some(99));
        assert_eq!(r.retry_count, 2);
        assert_eq!(r.review_rounds, 1);
        let prr = r.pr_review_result.as_ref().unwrap();
        assert_eq!(prr.inline_comment_count, 3);
        assert!(prr.summary_posted);
    }

    // ── HandledIssueStore additional tests ───────────────────────────────────

    #[tokio::test]
    async fn test_handled_issue_store_persists_across_instances() {
        let dir = TempDir::new().unwrap();
        {
            let store = HandledIssueStore::new(dir.path());
            store
                .mark_handled_with_number(
                    "pipe",
                    7,
                    HandledIssueRecord {
                        pr_number: Some(10),
                        issue_url: "u".into(),
                        issue_title: "t".into(),
                        issue_repo: "r".into(),
                        pr_url: Some("pr".into()),
                        pr_open: true,
                        handled_at: now(),
                        updated_at: now(),
                    },
                )
                .await
                .unwrap();
        }
        let store2 = HandledIssueStore::new(dir.path());
        assert!(store2.is_handled("pipe", 7).await.unwrap());
    }

    #[tokio::test]
    async fn test_handled_issue_store_mark_with_pr_url() {
        let dir = TempDir::new().unwrap();
        let store = HandledIssueStore::new(dir.path());
        store
            .mark_handled_with_number(
                "pipe",
                1,
                HandledIssueRecord {
                    pr_number: Some(5),
                    issue_url: "https://github.com/o/r/issues/1".into(),
                    issue_title: "Bug".into(),
                    issue_repo: "o/r".into(),
                    pr_url: Some("https://github.com/o/r/pull/5".into()),
                    pr_open: true,
                    handled_at: now(),
                    updated_at: now(),
                },
            )
            .await
            .unwrap();
        assert!(store.is_handled("pipe", 1).await.unwrap());
    }

    #[tokio::test]
    async fn test_handled_issue_store_mark_without_pr_url() {
        let dir = TempDir::new().unwrap();
        let store = HandledIssueStore::new(dir.path());
        store
            .mark_handled_with_number(
                "pipe",
                2,
                HandledIssueRecord {
                    pr_number: None,
                    issue_url: "url".into(),
                    issue_title: "t".into(),
                    issue_repo: "r".into(),
            pr_url: None,
                    pr_open: false,
                    handled_at: now(),
                    updated_at: now(),
                },
            )
            .await
            .unwrap();
        assert!(store.is_handled("pipe", 2).await.unwrap());
    }

    #[tokio::test]
    async fn test_handled_issue_store_handles_missing_file() {
        let dir = TempDir::new().unwrap();
        let store = HandledIssueStore::new(dir.path());
        // No file at all — should return false without error.
        assert!(!store.is_handled("anything", 999).await.unwrap());
    }

    #[tokio::test]
    async fn test_handled_issue_store_multiple_issues_same_workflow() {
        let dir = TempDir::new().unwrap();
        let store = HandledIssueStore::new(dir.path());
        for n in 1..=3 {
            store
                .mark_handled_with_number(
                    "same-pipe",
                    n,
                    HandledIssueRecord {
                        pr_number: None,
                        issue_url: format!("u/{}", n),
                        issue_title: format!("issue {}", n),
                        issue_repo: "r".into(),
                        pr_url: None,
                        pr_open: false,
                        handled_at: now(),
                        updated_at: now(),
                    },
                )
                .await
                .unwrap();
        }
        assert!(store.is_handled("same-pipe", 1).await.unwrap());
        assert!(store.is_handled("same-pipe", 2).await.unwrap());
        assert!(store.is_handled("same-pipe", 3).await.unwrap());
        assert!(!store.is_handled("same-pipe", 4).await.unwrap());
    }

    #[tokio::test]
    async fn test_handled_issue_store_overwrite_updates_record() {
        let dir = TempDir::new().unwrap();
        let store = HandledIssueStore::new(dir.path());
        store
            .mark_handled_with_number(
                "pipe",
                1,
                HandledIssueRecord {
                    pr_number: None,
                    issue_url: "old-url".into(),
                    issue_title: "old".into(),
                    issue_repo: "r".into(),
            pr_url: None,
                    pr_open: false,
                    handled_at: now(),
                    updated_at: now(),
                },
            )
            .await
            .unwrap();
        store
            .mark_handled_with_number(
                "pipe",
                1,
                HandledIssueRecord {
                    pr_number: Some(10),
                    issue_url: "new-url".into(),
                    issue_title: "new".into(),
                    issue_repo: "r".into(),
                    pr_url: Some("pr-url".into()),
                    pr_open: true,
                    handled_at: now(),
                    updated_at: now(),
                },
            )
            .await
            .unwrap();
        // Should still be handled (overwrite, not duplicate).
        assert!(store.is_handled("pipe", 1).await.unwrap());
    }

    #[tokio::test]
    async fn test_handled_issue_store_unmark_nonexistent_does_not_error() {
        let dir = TempDir::new().unwrap();
        let store = HandledIssueStore::new(dir.path());
        // Unmarking something that was never marked should succeed.
        store.unmark_handled("pipe", 999).await.unwrap();
    }

    #[tokio::test]
    async fn test_handled_issue_store_empty_file() {
        let dir = TempDir::new().unwrap();
        // Write an empty string to the file path.
        tokio::fs::write(dir.path().join(HANDLED_ISSUES_FILE), "")
            .await
            .unwrap();
        let store = HandledIssueStore::new(dir.path());
        assert!(!store.is_handled("pipe", 1).await.unwrap());
    }

    // ── PrReviewStore additional tests ───────────────────────────────────────

    #[tokio::test]
    async fn test_pr_review_store_persists_across_instances() {
        let dir = TempDir::new().unwrap();
        {
            let store = PrReviewStore::new(dir.path());
            store
                .mark_reviewed(
                    "pipe",
                    PrReviewRecord {
                        pr_number: 10,
                        pr_url: "u".into(),
                        pr_title: "t".into(),
                        pr_repo: "r".into(),
                        head_sha: "sha123".into(),
                            additions: 0,
                            deletions: 0,
                        last_comment_id: 50,
                        reviewed_at: now(),
                    },
                )
                .await
                .unwrap();
        }
        let store2 = PrReviewStore::new(dir.path());
        let rec = store2.get_record("pipe", 10).await.unwrap().unwrap();
        assert_eq!(rec.head_sha, "sha123");
    }

    #[tokio::test]
    async fn test_pr_review_store_cross_workflow_isolation() {
        let dir = TempDir::new().unwrap();
        let store = PrReviewStore::new(dir.path());
        store
            .mark_reviewed(
                "pipe-a",
                PrReviewRecord {
                    pr_number: 5,
                    pr_url: "u".into(),
                    pr_title: "t".into(),
                    pr_repo: "r".into(),
                    head_sha: "s".into(),
                            additions: 0,
                            deletions: 0,
                    last_comment_id: 0,
                    reviewed_at: now(),
                },
            )
            .await
            .unwrap();
        assert!(store.get_record("pipe-a", 5).await.unwrap().is_some());
        assert!(store.get_record("pipe-b", 5).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_pr_review_store_remove_nonexistent_ok() {
        let dir = TempDir::new().unwrap();
        let store = PrReviewStore::new(dir.path());
        // Removing a record that doesn't exist should succeed silently.
        store.remove_record("pipe", 999).await.unwrap();
    }

    #[tokio::test]
    async fn test_pr_review_store_handles_missing_file() {
        let dir = TempDir::new().unwrap();
        let store = PrReviewStore::new(dir.path());
        // No file on disk — get_record should return None, not error.
        assert!(store.get_record("pipe", 1).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_pr_review_store_empty_file_returns_none() {
        let dir = TempDir::new().unwrap();
        tokio::fs::write(dir.path().join(PR_REVIEW_FILE), "")
            .await
            .unwrap();
        let store = PrReviewStore::new(dir.path());
        assert!(store.get_record("pipe", 1).await.unwrap().is_none());
    }

    // ── PrResponseStore additional tests ─────────────────────────────────────

    #[tokio::test]
    async fn test_pr_response_store_persists_across_instances() {
        let dir = TempDir::new().unwrap();
        {
            let store = PrResponseStore::new(dir.path());
            store
                .mark_responded(
                    "pipe",
                    PrResponseRecord {
                        pr_number: 7,
                        pr_url: "u".into(),
                        pr_repo: "r".into(),
                        head_sha: "sha456".into(),
                        last_inline_comment_id: 100,
                        last_timeline_comment_id: 200,
                        responded_at: now(),
                    },
                )
                .await
                .unwrap();
        }
        let store2 = PrResponseStore::new(dir.path());
        let rec = store2.get_record("pipe", 7).await.unwrap().unwrap();
        assert_eq!(rec.head_sha, "sha456");
        assert_eq!(rec.last_inline_comment_id, 100);
    }

    #[tokio::test]
    async fn test_pr_response_store_remove_record() {
        let dir = TempDir::new().unwrap();
        let store = PrResponseStore::new(dir.path());
        store
            .mark_responded(
                "p",
                PrResponseRecord {
                    pr_number: 3,
                    pr_url: "u".into(),
                    pr_repo: "r".into(),
                    head_sha: "s".into(),
                    last_inline_comment_id: 0,
                    last_timeline_comment_id: 0,
                    responded_at: now(),
                },
            )
            .await
            .unwrap();
        store.remove_record("p", 3).await.unwrap();
        assert!(store.get_record("p", 3).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_pr_response_store_remove_nonexistent_ok() {
        let dir = TempDir::new().unwrap();
        let store = PrResponseStore::new(dir.path());
        // Removing a record that doesn't exist should succeed silently.
        store.remove_record("pipe", 999).await.unwrap();
    }

    #[tokio::test]
    async fn test_pr_response_store_handles_missing_file() {
        let dir = TempDir::new().unwrap();
        let store = PrResponseStore::new(dir.path());
        assert!(store.get_record("pipe", 1).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_pr_response_store_empty_file_returns_none() {
        let dir = TempDir::new().unwrap();
        tokio::fs::write(dir.path().join(PR_RESPONSE_FILE), "")
            .await
            .unwrap();
        let store = PrResponseStore::new(dir.path());
        assert!(store.get_record("pipe", 1).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_pr_response_store_tracks_both_cursors_independently() {
        let dir = TempDir::new().unwrap();
        let store = PrResponseStore::new(dir.path());
        store
            .mark_responded(
                "p",
                PrResponseRecord {
                    pr_number: 1,
                    pr_url: "u".into(),
                    pr_repo: "r".into(),
                    head_sha: "s1".into(),
                    last_inline_comment_id: 10,
                    last_timeline_comment_id: 5,
                    responded_at: now(),
                },
            )
            .await
            .unwrap();
        // Update only the inline cursor.
        store
            .mark_responded(
                "p",
                PrResponseRecord {
                    pr_number: 1,
                    pr_url: "u".into(),
                    pr_repo: "r".into(),
                    head_sha: "s2".into(),
                    last_inline_comment_id: 50,
                    last_timeline_comment_id: 5,
                    responded_at: now(),
                },
            )
            .await
            .unwrap();
        let rec = store.get_record("p", 1).await.unwrap().unwrap();
        assert_eq!(rec.last_inline_comment_id, 50);
        assert_eq!(rec.last_timeline_comment_id, 5);
        assert_eq!(rec.head_sha, "s2");
    }
}

// ---- FailureCooldownStore ---------------------------------------------------

const FAILURE_COOLDOWN_FILE: &str = "failure-cooldowns.json";

/// Default cooldown after a failure: 60 minutes.
const DEFAULT_COOLDOWN_MINUTES: i64 = 60;

/// A record of a failed attempt with a timestamp.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailureRecord {
    /// When the failure occurred (ISO 8601).
    pub failed_at: String,
    /// Number of consecutive failures.
    pub failure_count: u64,
}

/// Tracks recent failures per workflow + item to prevent infinite retry loops.
///
/// When an issue or PR fails, it is recorded with a timestamp.  Subsequent
/// ticks skip the item until the cooldown period has elapsed.  The cooldown
/// doubles with each consecutive failure (exponential backoff capped at 24h).
pub struct FailureCooldownStore {
    file_path: PathBuf,
}

impl FailureCooldownStore {
    /// Create a new store.
    pub fn new(project_root: &Path) -> Self {
        Self {
            file_path: project_root.join(OUTPUT_DIR).join(FAILURE_COOLDOWN_FILE),
        }
    }

    /// Check if an item is currently in cooldown (should be skipped).
    pub async fn is_in_cooldown(
        &self,
        workflow_name: &str,
        item_id: &str,
    ) -> Result<bool> {
        let data = self.read_all().await.unwrap_or_default();
        let key = format!("{}:{}", workflow_name, item_id);
        if let Some(record) = data.get(&key) {
            if let Ok(failed_at) = chrono::DateTime::parse_from_rfc3339(&record.failed_at) {
                let cooldown_minutes = cooldown_minutes(record.failure_count);
                let cooldown = chrono::Duration::minutes(cooldown_minutes);
                let now = chrono::Utc::now();
                return Ok(now < failed_at + cooldown);
            }
        }
        Ok(false)
    }

    /// Record a failure for an item.
    pub async fn record_failure(
        &self,
        workflow_name: &str,
        item_id: &str,
    ) -> Result<()> {
        let mut data = self.read_all().await.unwrap_or_default();
        let key = format!("{}:{}", workflow_name, item_id);
        let failure_count = data
            .get(&key)
            .map(|r| r.failure_count + 1)
            .unwrap_or(1);
        data.insert(
            key,
            FailureRecord {
                failed_at: chrono::Utc::now().to_rfc3339(),
                failure_count,
            },
        );
        self.write_all(&data).await
    }

    /// Return all failure records for a given workflow.
    ///
    /// Returns `(item_key, FailureRecord)` pairs for items whose key starts
    /// with `workflow_name:`.
    pub async fn get_failures_for_workflow(
        &self,
        workflow_name: &str,
    ) -> Result<Vec<(String, FailureRecord)>> {
        let data = self.read_all().await.unwrap_or_default();
        let prefix = format!("{}:", workflow_name);
        let results: Vec<(String, FailureRecord)> = data
            .into_iter()
            .filter(|(key, _)| key.starts_with(&prefix))
            .map(|(key, rec)| {
                let item_key = key.strip_prefix(&prefix).unwrap_or(&key).to_string();
                (item_key, rec)
            })
            .collect();
        Ok(results)
    }

    /// Clear the failure record for an item (e.g. on success).
    pub async fn clear_failure(
        &self,
        workflow_name: &str,
        item_id: &str,
    ) -> Result<()> {
        let mut data = self.read_all().await.unwrap_or_default();
        let key = format!("{}:{}", workflow_name, item_id);
        data.remove(&key);
        self.write_all(&data).await
    }

    async fn read_all(&self) -> Result<HashMap<String, FailureRecord>> {
        if !self.file_path.exists() {
            return Ok(HashMap::new());
        }
        let content = fs::read_to_string(&self.file_path).await?;
        if content.trim().is_empty() {
            return Ok(HashMap::new());
        }
        let data: HashMap<String, FailureRecord> = serde_json::from_str(&content)?;
        Ok(data)
    }

    async fn write_all(&self, data: &HashMap<String, FailureRecord>) -> Result<()> {
        if let Some(parent) = self.file_path.parent() {
            fs::create_dir_all(parent).await?;
        }
        let content = serde_json::to_string_pretty(data)?;
        fs::write(&self.file_path, content).await?;
        Ok(())
    }
}

/// Exponential backoff for cooldown: 60min, 120min, 240min, ... capped at 24h.
fn cooldown_minutes(failure_count: u64) -> i64 {
    let base = DEFAULT_COOLDOWN_MINUTES;
    let multiplier = 2i64.saturating_pow(failure_count.saturating_sub(1) as u32);
    (base * multiplier).min(24 * 60)
}

#[cfg(test)]
mod failure_cooldown_tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_cooldown_minutes_backoff() {
        assert_eq!(cooldown_minutes(1), 60);
        assert_eq!(cooldown_minutes(2), 120);
        assert_eq!(cooldown_minutes(3), 240);
        assert_eq!(cooldown_minutes(4), 480);
        assert_eq!(cooldown_minutes(10), 24 * 60); // capped at 24h
    }

    #[tokio::test]
    async fn test_no_failure_not_in_cooldown() {
        let dir = TempDir::new().unwrap();
        let store = FailureCooldownStore::new(dir.path());
        assert!(!store.is_in_cooldown("wf", "42").await.unwrap());
    }

    #[tokio::test]
    async fn test_record_failure_enters_cooldown() {
        let dir = TempDir::new().unwrap();
        let store = FailureCooldownStore::new(dir.path());
        store.record_failure("wf", "42").await.unwrap();
        assert!(store.is_in_cooldown("wf", "42").await.unwrap());
    }

    #[tokio::test]
    async fn test_clear_failure_exits_cooldown() {
        let dir = TempDir::new().unwrap();
        let store = FailureCooldownStore::new(dir.path());
        store.record_failure("wf", "42").await.unwrap();
        assert!(store.is_in_cooldown("wf", "42").await.unwrap());
        store.clear_failure("wf", "42").await.unwrap();
        assert!(!store.is_in_cooldown("wf", "42").await.unwrap());
    }

    #[tokio::test]
    async fn test_different_workflows_independent() {
        let dir = TempDir::new().unwrap();
        let store = FailureCooldownStore::new(dir.path());
        store.record_failure("wf1", "42").await.unwrap();
        assert!(store.is_in_cooldown("wf1", "42").await.unwrap());
        assert!(!store.is_in_cooldown("wf2", "42").await.unwrap());
    }

    #[tokio::test]
    async fn test_consecutive_failures_increase_count() {
        let dir = TempDir::new().unwrap();
        let store = FailureCooldownStore::new(dir.path());
        store.record_failure("wf", "1").await.unwrap();
        store.record_failure("wf", "1").await.unwrap();
        store.record_failure("wf", "1").await.unwrap();
        let data = store.read_all().await.unwrap();
        assert_eq!(data.get("wf:1").unwrap().failure_count, 3);
    }

    #[tokio::test]
    async fn test_get_failures_for_workflow_returns_matching() {
        let dir = TempDir::new().unwrap();
        let store = FailureCooldownStore::new(dir.path());
        store.record_failure("wf", "42").await.unwrap();
        store.record_failure("wf", "43").await.unwrap();
        store.record_failure("other", "99").await.unwrap();
        let failures = store.get_failures_for_workflow("wf").await.unwrap();
        assert_eq!(failures.len(), 2);
        let keys: Vec<String> = failures.iter().map(|(k, _)| k.clone()).collect();
        assert!(keys.contains(&"42".to_string()));
        assert!(keys.contains(&"43".to_string()));
    }

    #[tokio::test]
    async fn test_get_failures_for_workflow_empty() {
        let dir = TempDir::new().unwrap();
        let store = FailureCooldownStore::new(dir.path());
        let failures = store.get_failures_for_workflow("wf").await.unwrap();
        assert!(failures.is_empty());
    }

    #[tokio::test]
    async fn test_get_failures_for_workflow_excludes_other() {
        let dir = TempDir::new().unwrap();
        let store = FailureCooldownStore::new(dir.path());
        store.record_failure("wf1", "42").await.unwrap();
        store.record_failure("wf2", "99").await.unwrap();
        let failures = store.get_failures_for_workflow("wf1").await.unwrap();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].0, "42");
    }

    // ── Missing / empty / corrupt file resilience ─────────────────────────

    #[tokio::test]
    async fn test_run_store_missing_file() {
        let dir = TempDir::new().unwrap();
        let store = RunStore::new(dir.path());
        let runs = store.get_history(Some("any"), 10).await.unwrap();
        assert!(runs.is_empty());
    }

    #[tokio::test]
    async fn test_handled_issue_store_missing_file() {
        let dir = TempDir::new().unwrap();
        let store = HandledIssueStore::new(dir.path());
        assert!(!store.is_handled("wf", 42).await.unwrap());
        let all = store.get_all_records("wf").await.unwrap();
        assert!(all.is_empty());
    }

    #[tokio::test]
    async fn test_pr_review_store_missing_file() {
        let dir = TempDir::new().unwrap();
        let store = PrReviewStore::new(dir.path());
        assert!(store.get_record("wf", 42).await.unwrap().is_none());
        let all = store.get_all_records("wf").await.unwrap();
        assert!(all.is_empty());
    }

    #[tokio::test]
    async fn test_pr_response_store_missing_file() {
        let dir = TempDir::new().unwrap();
        let store = PrResponseStore::new(dir.path());
        assert!(store.get_record("wf", 42).await.unwrap().is_none());
        let all = store.get_all_records("wf").await.unwrap();
        assert!(all.is_empty());
    }

    #[tokio::test]
    async fn test_failure_cooldown_store_missing_file() {
        let dir = TempDir::new().unwrap();
        let store = FailureCooldownStore::new(dir.path());
        assert!(!store.is_in_cooldown("wf", "42").await.unwrap());
        let failures = store.get_failures_for_workflow("wf").await.unwrap();
        assert!(failures.is_empty());
    }

    #[tokio::test]
    async fn test_failure_cooldown_store_empty_file() {
        let dir = TempDir::new().unwrap();
        let store = FailureCooldownStore::new(dir.path());
        // Create the output dir and write an empty file.
        let output_dir = dir.path().join("output");
        std::fs::create_dir_all(&output_dir).unwrap();
        std::fs::write(output_dir.join("failure-cooldowns.json"), "").unwrap();
        // Should not crash.
        assert!(!store.is_in_cooldown("wf", "42").await.unwrap());
    }

    #[tokio::test]
    async fn test_run_store_empty_file() {
        let dir = TempDir::new().unwrap();
        let store = RunStore::new(dir.path());
        let output_dir = dir.path().join("output");
        std::fs::create_dir_all(&output_dir).unwrap();
        std::fs::write(output_dir.join("runs.json"), "").unwrap();
        let runs = store.get_history(Some("any"), 10).await.unwrap();
        assert!(runs.is_empty());
    }

    #[tokio::test]
    async fn test_handled_issue_store_empty_file() {
        let dir = TempDir::new().unwrap();
        let store = HandledIssueStore::new(dir.path());
        let output_dir = dir.path().join("output");
        std::fs::create_dir_all(&output_dir).unwrap();
        std::fs::write(output_dir.join("handled-issues.json"), "").unwrap();
        assert!(!store.is_handled("wf", 42).await.unwrap());
    }

    #[tokio::test]
    async fn test_stores_recreate_after_deletion() {
        let dir = TempDir::new().unwrap();
        let store = FailureCooldownStore::new(dir.path());
        // Write something.
        store.record_failure("wf", "42").await.unwrap();
        assert!(store.is_in_cooldown("wf", "42").await.unwrap());
        // Delete the output directory entirely.
        let output_dir = dir.path().join("output");
        std::fs::remove_dir_all(&output_dir).unwrap();
        // Should not crash — returns empty.
        assert!(!store.is_in_cooldown("wf", "42").await.unwrap());
        // Writing should recreate the directory.
        store.record_failure("wf", "99").await.unwrap();
        assert!(store.is_in_cooldown("wf", "99").await.unwrap());
    }
}
