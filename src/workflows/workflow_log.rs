//! WorkflowLog — per-run structured log written to
//! `output/logs/<workflow_name>/<run_id>.json`.
//!
//! Each workflow run gets its own log file containing timestamped entries
//! with a level, stage name, and message.  This enables a TUI to:
//!   - List past runs per workflow (read the directory)
//!   - Stream or page through a run's log (read the JSON array)
//!   - Show status per workflow (read the last entry's level)
//!
//! The log file is flushed (rewritten) after every entry so a crash never
//! loses more than one message.  Files are append-only in spirit — the
//! `entries` vec grows monotonically.

use anyhow::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs;
use tokio::sync::Mutex;

use crate::tui::events::{TuiEvent, TuiEventSender};

/// A single timestamped log entry within a workflow run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    /// ISO-8601 timestamp of when this entry was recorded.
    pub timestamp: String,
    /// Severity: `"info"`, `"warn"`, `"error"`, `"debug"`.
    pub level: String,
    /// Name of the stage that produced this entry (e.g. `"agent-loop"`).
    pub stage: String,
    /// The log message.
    pub message: String,
}

/// Metadata header written at the top of each log file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogFileHeader {
    #[serde(alias = "pipeline_name")]
    pub workflow_name: String,
    pub run_id: String,
    pub started_at: String,
    /// Set when the run completes (success, failure, or skip).
    pub completed_at: Option<String>,
    /// `"running"`, `"success"`, `"failed"`, `"skipped"`.
    pub status: String,
}

/// The full JSON structure written to each log file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogFile {
    pub header: LogFileHeader,
    pub entries: Vec<LogEntry>,
}

/// A handle to a single run's log file.  Cloneable and shareable across
/// stages via `Arc<WorkflowLog>`.
///
/// Thread-safe: entries are appended behind a `Mutex`.
#[derive(Clone)]
pub struct WorkflowLog {
    file_path: PathBuf,
    inner: Arc<Mutex<LogFile>>,
    tui_tx: TuiEventSender,
}

impl WorkflowLog {
    /// Create a new workflow log for a workflow run.
    ///
    /// The log file is written to `output/logs/<workflow_name>/<run_id>.json`
    /// relative to `project_root`.
    pub async fn new(
        project_root: &Path,
        workflow_name: &str,
        run_id: &str,
    ) -> Result<Self> {
        Self::with_tui_sender(project_root, workflow_name, run_id, TuiEventSender::noop()).await
    }

    /// Create a new workflow log with a TUI event sender attached.
    ///
    /// When a sender is attached, every log entry is mirrored as a
    /// [`TuiEvent::LogMessage`] for real-time display.
    pub async fn with_tui_sender(
        project_root: &Path,
        workflow_name: &str,
        run_id: &str,
        tui_tx: TuiEventSender,
    ) -> Result<Self> {
        let dir = project_root
            .join("output")
            .join("logs")
            .join(workflow_name);
        fs::create_dir_all(&dir).await?;

        let file_path = dir.join(format!("{}.json", run_id));

        let log_file = LogFile {
            header: LogFileHeader {
                workflow_name: workflow_name.to_string(),
                run_id: run_id.to_string(),
                started_at: Utc::now().to_rfc3339(),
                completed_at: None,
                status: "running".to_string(),
            },
            entries: Vec::new(),
        };

        let this = Self {
            file_path,
            inner: Arc::new(Mutex::new(log_file)),
            tui_tx,
        };

        this.flush().await?;
        Ok(this)
    }

    /// Append a log entry and flush to disk.
    ///
    /// If a TUI event sender is attached, the entry is also mirrored as a
    /// [`TuiEvent::LogMessage`].
    pub async fn log(
        &self,
        level: &str,
        stage: &str,
        message: &str,
    ) -> Result<()> {
        let (workflow_name, run_id) = {
            let mut inner = self.inner.lock().await;
            inner.entries.push(LogEntry {
                timestamp: Utc::now().to_rfc3339(),
                level: level.to_string(),
                stage: stage.to_string(),
                message: message.to_string(),
            });
            (
                inner.header.workflow_name.clone(),
                inner.header.run_id.clone(),
            )
        };

        self.tui_tx.send(TuiEvent::LogMessage {
            workflow_name,
            run_id,
            level: level.to_string(),
            stage: stage.to_string(),
            message: message.to_string(),
        });

        self.flush().await
    }

    /// Convenience: log at info level.
    pub async fn info(&self, stage: &str, message: &str) -> Result<()> {
        self.log("info", stage, message).await
    }

    /// Convenience: log at error level.
    pub async fn error(&self, stage: &str, message: &str) -> Result<()> {
        self.log("error", stage, message).await
    }

    /// Convenience: log at debug level.
    pub async fn debug(&self, stage: &str, message: &str) -> Result<()> {
        self.log("debug", stage, message).await
    }

    /// Mark the run as completed with a final status.
    pub async fn complete(&self, status: &str) -> Result<()> {
        {
            let mut inner = self.inner.lock().await;
            inner.header.completed_at = Some(Utc::now().to_rfc3339());
            inner.header.status = status.to_string();
        }
        self.flush().await
    }

    /// Return the path to the log file.
    pub fn path(&self) -> &Path {
        &self.file_path
    }

    /// Write the current state to disk.
    async fn flush(&self) -> Result<()> {
        let inner = self.inner.lock().await;
        let json = serde_json::to_string_pretty(&*inner)?;
        fs::write(&self.file_path, json).await?;
        Ok(())
    }
}

/// List all run log files for a workflow, sorted by filename (newest last
/// when run IDs are UUIDs or timestamps).
pub async fn list_run_logs(
    project_root: &Path,
    workflow_name: &str,
) -> Result<Vec<PathBuf>> {
    let dir = project_root
        .join("output")
        .join("logs")
        .join(workflow_name);

    if !dir.exists() {
        return Ok(vec![]);
    }

    let mut entries = Vec::new();
    let mut read_dir = fs::read_dir(&dir).await?;
    while let Some(entry) = read_dir.next_entry().await? {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("json") {
            entries.push(path);
        }
    }
    entries.sort();
    Ok(entries)
}

/// Read and parse a single run log file.
pub async fn read_run_log(path: &Path) -> Result<LogFile> {
    let content = fs::read_to_string(path).await?;
    let log: LogFile = serde_json::from_str(&content)?;
    Ok(log)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_workflow_log_creates_file() {
        let dir = TempDir::new().unwrap();
        let log = WorkflowLog::new(dir.path(), "my-pipeline", "run-123")
            .await
            .unwrap();

        assert!(log.path().exists());
        let content = fs::read_to_string(log.path()).await.unwrap();
        let parsed: LogFile = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed.header.workflow_name, "my-pipeline");
        assert_eq!(parsed.header.run_id, "run-123");
        assert_eq!(parsed.header.status, "running");
        assert!(parsed.entries.is_empty());
    }

    #[tokio::test]
    async fn test_workflow_log_appends_entries() {
        let dir = TempDir::new().unwrap();
        let log = WorkflowLog::new(dir.path(), "pipe", "run-1")
            .await
            .unwrap();

        log.info("agent-loop", "Starting agent").await.unwrap();
        log.error("reviewer", "Review failed").await.unwrap();
        log.debug("pull-request", "Pushing branch").await.unwrap();

        let parsed = read_run_log(log.path()).await.unwrap();
        assert_eq!(parsed.entries.len(), 3);
        assert_eq!(parsed.entries[0].level, "info");
        assert_eq!(parsed.entries[0].stage, "agent-loop");
        assert_eq!(parsed.entries[0].message, "Starting agent");
        assert_eq!(parsed.entries[1].level, "error");
        assert_eq!(parsed.entries[2].level, "debug");
    }

    #[tokio::test]
    async fn test_workflow_log_complete_sets_status() {
        let dir = TempDir::new().unwrap();
        let log = WorkflowLog::new(dir.path(), "pipe", "run-1")
            .await
            .unwrap();

        log.complete("success").await.unwrap();

        let parsed = read_run_log(log.path()).await.unwrap();
        assert_eq!(parsed.header.status, "success");
        assert!(parsed.header.completed_at.is_some());
    }

    #[tokio::test]
    async fn test_workflow_log_file_path_structure() {
        let dir = TempDir::new().unwrap();
        let log = WorkflowLog::new(dir.path(), "issue-autofix", "abc-123")
            .await
            .unwrap();

        let expected = dir.path().join("output/logs/issue-autofix/abc-123.json");
        assert_eq!(log.path(), expected);
    }

    #[tokio::test]
    async fn test_list_run_logs_empty() {
        let dir = TempDir::new().unwrap();
        let logs = list_run_logs(dir.path(), "nonexistent").await.unwrap();
        assert!(logs.is_empty());
    }

    #[tokio::test]
    async fn test_list_run_logs_returns_sorted() {
        let dir = TempDir::new().unwrap();
        // Create 3 log files
        WorkflowLog::new(dir.path(), "pipe", "run-c").await.unwrap();
        WorkflowLog::new(dir.path(), "pipe", "run-a").await.unwrap();
        WorkflowLog::new(dir.path(), "pipe", "run-b").await.unwrap();

        let logs = list_run_logs(dir.path(), "pipe").await.unwrap();
        assert_eq!(logs.len(), 3);
        // Should be sorted alphabetically
        let names: Vec<_> = logs.iter().map(|p| p.file_name().unwrap().to_str().unwrap().to_string()).collect();
        assert_eq!(names, vec!["run-a.json", "run-b.json", "run-c.json"]);
    }

    #[tokio::test]
    async fn test_read_run_log_roundtrip() {
        let dir = TempDir::new().unwrap();
        let log = WorkflowLog::new(dir.path(), "test-pipe", "run-42")
            .await
            .unwrap();
        log.info("stage-a", "hello").await.unwrap();
        log.complete("success").await.unwrap();

        let parsed = read_run_log(log.path()).await.unwrap();
        assert_eq!(parsed.header.workflow_name, "test-pipe");
        assert_eq!(parsed.header.run_id, "run-42");
        assert_eq!(parsed.header.status, "success");
        assert_eq!(parsed.entries.len(), 1);
        assert_eq!(parsed.entries[0].message, "hello");
    }
}
