//! Session configuration persisted across TUI launches.
//!
//! The session file (`.session.toml`) stores per-workflow state like
//! enabled/disabled that should survive restarts but is not part of the
//! main `config.toml`.  The file is gitignored.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Per-workflow session state.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WorkflowSession {
    /// Whether this workflow is enabled.  Defaults to `true` when absent.
    pub enabled: Option<bool>,
}

/// Root session config deserialized from `.session.toml`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SessionConfig {
    /// Per-workflow overrides keyed by workflow name.
    /// Also accepts `pipelines` for backward compatibility.
    #[serde(default, alias = "pipelines")]
    pub workflows: HashMap<String, WorkflowSession>,
}

impl SessionConfig {
    /// Return whether a workflow is enabled.
    ///
    /// Defaults to `true` if the workflow is not in the session file.
    pub fn is_enabled(&self, workflow_name: &str) -> bool {
        self.workflows
            .get(workflow_name)
            .and_then(|p| p.enabled)
            .unwrap_or(true)
    }

    /// Set the enabled state for a workflow.
    pub fn set_enabled(&mut self, workflow_name: &str, enabled: bool) {
        let entry = self
            .workflows
            .entry(workflow_name.to_string())
            .or_default();
        entry.enabled = Some(enabled);
    }

    /// Toggle the enabled state for a workflow.  Returns the new state.
    pub fn toggle_enabled(&mut self, workflow_name: &str) -> bool {
        let was_enabled = self.is_enabled(workflow_name);
        self.set_enabled(workflow_name, !was_enabled);
        !was_enabled
    }
}

/// The filename used for session state.
const SESSION_FILENAME: &str = ".session.toml";

/// Resolve the path to the session file given a project root.
pub fn session_path(project_root: &Path) -> PathBuf {
    project_root.join(SESSION_FILENAME)
}

/// Load the session config from disk.
///
/// Returns `SessionConfig::default()` if the file does not exist or is
/// unparseable (we never fail on session load — it's best-effort).
pub async fn load_session(project_root: &Path) -> SessionConfig {
    let path = session_path(project_root);
    match tokio::fs::read_to_string(&path).await {
        Ok(content) => toml::from_str(&content).unwrap_or_default(),
        Err(_) => SessionConfig::default(),
    }
}

/// Write the session config to disk.
pub async fn save_session(project_root: &Path, config: &SessionConfig) -> Result<()> {
    let path = session_path(project_root);
    let content = toml::to_string_pretty(config)?;
    tokio::fs::write(&path, content).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_default_session_all_enabled() {
        let s = SessionConfig::default();
        assert!(s.is_enabled("anything"));
        assert!(s.is_enabled("issue-autofix"));
    }

    #[test]
    fn test_set_enabled_false() {
        let mut s = SessionConfig::default();
        s.set_enabled("issue-autofix", false);
        assert!(!s.is_enabled("issue-autofix"));
        assert!(s.is_enabled("other-pipeline"));
    }

    #[test]
    fn test_set_enabled_true_explicitly() {
        let mut s = SessionConfig::default();
        s.set_enabled("p", true);
        assert!(s.is_enabled("p"));
    }

    #[test]
    fn test_toggle_enabled() {
        let mut s = SessionConfig::default();
        // Default is enabled → toggle to disabled
        let new_state = s.toggle_enabled("p");
        assert!(!new_state);
        assert!(!s.is_enabled("p"));

        // Toggle back to enabled
        let new_state = s.toggle_enabled("p");
        assert!(new_state);
        assert!(s.is_enabled("p"));
    }

    #[test]
    fn test_toml_roundtrip() {
        let mut s = SessionConfig::default();
        s.set_enabled("issue-autofix", false);
        s.set_enabled("pr-reviewer", true);

        let toml_str = toml::to_string_pretty(&s).unwrap();
        let parsed: SessionConfig = toml::from_str(&toml_str).unwrap();

        assert!(!parsed.is_enabled("issue-autofix"));
        assert!(parsed.is_enabled("pr-reviewer"));
        assert!(parsed.is_enabled("unknown"));
    }

    #[test]
    fn test_toml_deserialize_missing_enabled() {
        let toml_str = r#"
[workflows.my-pipeline]
"#;
        let s: SessionConfig = toml::from_str(toml_str).unwrap();
        // enabled is None → defaults to true
        assert!(s.is_enabled("my-pipeline"));
    }

    #[test]
    fn test_toml_deserialize_empty() {
        let s: SessionConfig = toml::from_str("").unwrap();
        assert!(s.is_enabled("anything"));
    }

    #[tokio::test]
    async fn test_load_session_missing_file() {
        let dir = TempDir::new().unwrap();
        let s = load_session(dir.path()).await;
        assert!(s.is_enabled("any"));
    }

    #[tokio::test]
    async fn test_save_and_load_session() {
        let dir = TempDir::new().unwrap();
        let mut s = SessionConfig::default();
        s.set_enabled("issue-autofix", false);
        s.set_enabled("pr-reviewer", true);

        save_session(dir.path(), &s).await.unwrap();

        let loaded = load_session(dir.path()).await;
        assert!(!loaded.is_enabled("issue-autofix"));
        assert!(loaded.is_enabled("pr-reviewer"));
        assert!(loaded.is_enabled("unknown"));
    }

    #[tokio::test]
    async fn test_load_session_corrupt_file() {
        let dir = TempDir::new().unwrap();
        tokio::fs::write(dir.path().join(".session.toml"), "{{invalid toml!!")
            .await
            .unwrap();
        // Should not error — returns default
        let s = load_session(dir.path()).await;
        assert!(s.is_enabled("any"));
    }

    #[test]
    fn test_session_path() {
        let p = session_path(Path::new("/foo/bar"));
        assert_eq!(p, PathBuf::from("/foo/bar/.session.toml"));
    }
}
