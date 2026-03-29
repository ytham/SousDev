//! TUI event types emitted by the workflow infrastructure.
//!
//! Events flow from the `CronRunner`, `WorkflowExecutor`, and `WorkflowLog`
//! into the TUI via a `tokio::sync::mpsc` channel.  The TUI reads these
//! events to update its state (sidebar flowcharts, log panes, etc.).

/// An event emitted by the workflow infrastructure for the TUI to consume.
#[derive(Debug, Clone)]
pub enum TuiEvent {
    /// A workflow has been registered with the cron scheduler.
    WorkflowRegistered {
        name: String,
        schedule: String,
        mode: WorkflowMode,
        repo: Option<String>,
    },

    /// A cron tick fired for a workflow.
    TickFired { workflow_name: String },

    /// A cron tick was skipped (overlap guard or provider error).
    TickSkipped {
        workflow_name: String,
        reason: String,
    },

    /// A workflow run has started.
    RunStarted {
        workflow_name: String,
        run_id: String,
        mode: WorkflowMode,
        /// Human-readable label, e.g. "issue #42" or "PR #7".
        item_label: Option<String>,
    },

    /// A stage within a workflow run has started.
    StageStarted {
        workflow_name: String,
        run_id: String,
        stage_name: String,
    },

    /// A stage within a workflow run has completed successfully.
    StageCompleted {
        workflow_name: String,
        run_id: String,
        stage_name: String,
    },

    /// A stage within a workflow run has failed.
    StageFailed {
        workflow_name: String,
        run_id: String,
        stage_name: String,
        error: String,
    },

    /// A log message from a workflow run (mirrors WorkflowLog entries).
    LogMessage {
        workflow_name: String,
        run_id: String,
        level: String,
        stage: String,
        message: String,
    },

    /// A workflow run has completed.
    RunCompleted {
        workflow_name: String,
        run_id: String,
        success: bool,
        skipped: bool,
        error: Option<String>,
        pr_url: Option<String>,
    },

    /// A batch of item summaries for the info panel.
    ///
    /// Emitted by the executor after fetching and classifying issues/PRs.
    /// Replaces the previous item list for this workflow.
    ItemsSummary {
        workflow_name: String,
        items: Vec<ItemSummary>,
    },

    /// The entire cron scheduler is shutting down.
    Shutdown,
}

/// Summary of a single issue or PR for the info panel.
#[derive(Debug, Clone)]
pub struct ItemSummary {
    /// Human-readable identifier (e.g. `"#42"` or `"PR #12050"`).
    pub id: String,
    /// Short title (truncated for display).
    pub title: String,
    /// Full URL (GitHub or Linear).
    pub url: String,
    /// Current processing status.
    pub status: ItemStatus,
}

/// Processing status of an issue or PR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemStatus {
    /// Not yet processed.
    None,
    /// Currently being processed by the agent.
    InProgress,
    /// Completed successfully (PR opened, review posted, comments addressed).
    Success,
    /// Failed (may be in cooldown).
    Error,
    /// In failure cooldown — will retry after cooldown expires.
    Cooldown,
    /// PR review has been posted (pr-reviewer specific).
    Reviewed,
    /// PR has new unaddressed reviewer comments (pr-responder specific).
    NewComments,
}

/// Which workflow mode is being executed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowMode {
    /// Bug autofix from GitHub Issues.
    Issues,
    /// PR review from GitHub PRs.
    PrReview,
    /// PR comment response from GitHub PR comments.
    PrResponse,
    /// Standard shell-trigger or standalone workflow.
    Standard,
}

impl std::fmt::Display for WorkflowMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorkflowMode::Issues => write!(f, "issues"),
            WorkflowMode::PrReview => write!(f, "pr-review"),
            WorkflowMode::PrResponse => write!(f, "pr-response"),
            WorkflowMode::Standard => write!(f, "standard"),
        }
    }
}

/// Sender handle that workflow code uses to emit [`TuiEvent`]s.
///
/// Wraps an `Option<mpsc::UnboundedSender>` so callers can always call
/// `.send()` — when `None` (headless mode), the call is a no-op.
#[derive(Clone)]
pub struct TuiEventSender {
    inner: Option<tokio::sync::mpsc::UnboundedSender<TuiEvent>>,
}

impl TuiEventSender {
    /// Create a sender that delivers events to the TUI.
    pub fn new(tx: tokio::sync::mpsc::UnboundedSender<TuiEvent>) -> Self {
        Self { inner: Some(tx) }
    }

    /// Create a no-op sender (headless mode — all sends are silently dropped).
    pub fn noop() -> Self {
        Self { inner: None }
    }

    /// Emit a TUI event.  No-op if headless.
    pub fn send(&self, event: TuiEvent) {
        if let Some(ref tx) = self.inner {
            let _ = tx.send(event);
        }
    }

    /// Return `true` if this sender is wired to a real channel.
    pub fn is_active(&self) -> bool {
        self.inner.is_some()
    }
}

impl std::fmt::Debug for TuiEventSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TuiEventSender")
            .field("active", &self.is_active())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_noop_sender_does_not_panic() {
        let sender = TuiEventSender::noop();
        sender.send(TuiEvent::Shutdown);
        assert!(!sender.is_active());
    }

    #[test]
    fn test_noop_sender_many_events() {
        let sender = TuiEventSender::noop();
        for _ in 0..100 {
            sender.send(TuiEvent::TickFired {
                workflow_name: "p".into(),
            });
        }
        // Should not panic or leak.
        assert!(!sender.is_active());
    }

    #[test]
    fn test_active_sender_delivers() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let sender = TuiEventSender::new(tx);
        assert!(sender.is_active());
        sender.send(TuiEvent::Shutdown);
        let event = rx.try_recv().unwrap();
        assert!(matches!(event, TuiEvent::Shutdown));
    }

    #[test]
    fn test_sender_delivers_multiple_events_in_order() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let sender = TuiEventSender::new(tx);
        sender.send(TuiEvent::TickFired {
            workflow_name: "a".into(),
        });
        sender.send(TuiEvent::TickFired {
            workflow_name: "b".into(),
        });
        sender.send(TuiEvent::Shutdown);

        let e1 = rx.try_recv().unwrap();
        let e2 = rx.try_recv().unwrap();
        let e3 = rx.try_recv().unwrap();

        assert!(matches!(e1, TuiEvent::TickFired { ref workflow_name } if workflow_name == "a"));
        assert!(matches!(e2, TuiEvent::TickFired { ref workflow_name } if workflow_name == "b"));
        assert!(matches!(e3, TuiEvent::Shutdown));
    }

    #[test]
    fn test_sender_clone_shares_channel() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let sender1 = TuiEventSender::new(tx);
        let sender2 = sender1.clone();

        sender1.send(TuiEvent::TickFired {
            workflow_name: "from-1".into(),
        });
        sender2.send(TuiEvent::TickFired {
            workflow_name: "from-2".into(),
        });

        let e1 = rx.try_recv().unwrap();
        let e2 = rx.try_recv().unwrap();
        assert!(
            matches!(e1, TuiEvent::TickFired { ref workflow_name } if workflow_name == "from-1")
        );
        assert!(
            matches!(e2, TuiEvent::TickFired { ref workflow_name } if workflow_name == "from-2")
        );
    }

    #[test]
    fn test_sender_survives_dropped_receiver() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let sender = TuiEventSender::new(tx);
        drop(rx);
        // Should not panic — the send just silently fails.
        sender.send(TuiEvent::Shutdown);
        assert!(sender.is_active());
    }

    #[test]
    fn test_sender_debug_format() {
        let active = TuiEventSender::new(tokio::sync::mpsc::unbounded_channel().0);
        let debug = format!("{:?}", active);
        assert!(debug.contains("active: true"));

        let noop = TuiEventSender::noop();
        let debug = format!("{:?}", noop);
        assert!(debug.contains("active: false"));
    }

    #[test]
    fn test_workflow_mode_display() {
        assert_eq!(WorkflowMode::Issues.to_string(), "issues");
        assert_eq!(WorkflowMode::PrReview.to_string(), "pr-review");
        assert_eq!(WorkflowMode::PrResponse.to_string(), "pr-response");
        assert_eq!(WorkflowMode::Standard.to_string(), "standard");
    }

    #[test]
    fn test_workflow_mode_equality() {
        assert_eq!(WorkflowMode::Issues, WorkflowMode::Issues);
        assert_ne!(WorkflowMode::Issues, WorkflowMode::PrReview);
    }

    #[test]
    fn test_tui_event_is_clone() {
        let event = TuiEvent::LogMessage {
            workflow_name: "p".into(),
            run_id: "r".into(),
            level: "info".into(),
            stage: "s".into(),
            message: "hello".into(),
        };
        let cloned = event.clone();
        assert!(matches!(
            cloned,
            TuiEvent::LogMessage { ref workflow_name, .. } if workflow_name == "p"
        ));
    }

    #[test]
    fn test_tui_event_debug() {
        let event = TuiEvent::Shutdown;
        let debug = format!("{:?}", event);
        assert!(debug.contains("Shutdown"));
    }
}
