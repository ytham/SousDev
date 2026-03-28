/// TUI application state and event loop.
///
/// The `App` struct holds all mutable state needed to render the dashboard.
/// The `run_app` function sets up the terminal, spawns the cron scheduler,
/// and drives the ratatui draw/input loop.
use std::collections::HashSet;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::Mutex;

use crate::workflows::cron_runner::CronRunner;
use crate::tui::events::{WorkflowMode, TuiEvent, TuiEventSender};
use crate::tui::session::{self, SessionConfig};
use crate::tui::ui;
use crate::types::config::HarnessConfig;

/// The input mode determines how keystrokes are interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    /// Normal mode: up/down selects workflow, page-up/page-down scrolls logs.
    Normal,
    /// Command mode: activated by Esc, shows the command menu.
    Command,
}

/// A single log line stored per-workflow.
#[derive(Debug, Clone)]
pub struct LogLine {
    pub level: String,
    pub stage: String,
    pub message: String,
}

/// Stage status within a workflow flowchart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageStatus {
    /// Stage has not run yet this tick.
    Pending,
    /// Stage is currently executing.
    Running,
    /// Stage completed successfully.
    Done,
    /// Stage failed.
    Failed,
}

/// Per-workflow state tracked by the TUI.
#[derive(Debug, Clone)]
pub struct WorkflowState {
    /// Workflow name.
    pub name: String,
    /// Cron schedule expression.
    pub schedule: String,
    /// Workflow mode (issues, pr-review, etc.).
    pub mode: WorkflowMode,
    /// Repository identifier (owner/repo).
    pub repo: Option<String>,
    /// Ordered list of stage names for the flowchart.
    pub stages: Vec<String>,
    /// Current status of each stage (parallel to `stages`).
    pub stage_statuses: Vec<StageStatus>,
    /// Overall workflow status label.
    pub status: WorkflowStatus,
    /// Current run ID (if running).
    pub run_id: Option<String>,
    /// Log lines for this workflow.
    pub logs: Vec<LogLine>,
    /// Whether this workflow is enabled (cron ticks are skipped when disabled).
    pub enabled: bool,
}

/// High-level workflow status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowStatus {
    Idle,
    Running,
    Success,
    Failed,
    Skipped,
    Disabled,
}

impl std::fmt::Display for WorkflowStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorkflowStatus::Idle => write!(f, "idle"),
            WorkflowStatus::Running => write!(f, "running"),
            WorkflowStatus::Success => write!(f, "success"),
            WorkflowStatus::Failed => write!(f, "failed"),
            WorkflowStatus::Skipped => write!(f, "skipped"),
            WorkflowStatus::Disabled => write!(f, "disabled"),
        }
    }
}

/// The full TUI application state.
pub struct App {
    /// All registered workflows.
    pub workflows: Vec<WorkflowState>,
    /// Index of the currently selected workflow in the sidebar.
    pub selected: usize,
    /// Current input mode (Normal or Command).
    pub input_mode: InputMode,
    /// Scroll offset for the log pane (0 = auto-tail).
    pub log_scroll: usize,
    /// Whether the app should quit.
    pub should_quit: bool,
    /// Session config (persisted to `.session.toml`).
    pub session: SessionConfig,
    /// Project root directory (for saving `.session.toml`).
    pub project_root: Option<PathBuf>,
    /// Shared set of disabled workflow names, read by the CronRunner.
    pub disabled_workflows: Arc<Mutex<HashSet<String>>>,
    /// Flag set when session needs to be persisted to disk.
    pub session_dirty: bool,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    /// Create a new empty app.
    pub fn new() -> Self {
        Self {
            workflows: Vec::new(),
            selected: 0,
            input_mode: InputMode::Normal,
            log_scroll: 0,
            should_quit: false,
            session: SessionConfig::default(),
            project_root: None,
            disabled_workflows: Arc::new(Mutex::new(HashSet::new())),
            session_dirty: false,
        }
    }

    /// Create an app pre-loaded with session config.
    pub fn with_session(
        session: SessionConfig,
        project_root: PathBuf,
        disabled_workflows: Arc<Mutex<HashSet<String>>>,
    ) -> Self {
        Self {
            session,
            project_root: Some(project_root),
            disabled_workflows,
            ..Self::new()
        }
    }

    /// Return the currently selected workflow, if any.
    pub fn selected_workflow(&self) -> Option<&WorkflowState> {
        self.workflows.get(self.selected)
    }

    /// Process a TUI event and update state accordingly.
    pub fn handle_tui_event(&mut self, event: TuiEvent) {
        match event {
            TuiEvent::WorkflowRegistered {
                name,
                schedule,
                mode,
                repo,
            } => {
                let enabled = self.session.is_enabled(&name);
                let stages = stages_for_mode(mode);
                let stage_statuses = vec![StageStatus::Pending; stages.len()];
                let status = if enabled {
                    WorkflowStatus::Idle
                } else {
                    WorkflowStatus::Disabled
                };
                self.workflows.push(WorkflowState {
                    name,
                    schedule,
                    mode,
                    repo,
                    stages,
                    stage_statuses,
                    status,
                    run_id: None,
                    logs: Vec::new(),
                    enabled,
                });
            }

            TuiEvent::TickFired { workflow_name } => {
                if let Some(wf) = self.find_workflow_mut(&workflow_name) {
                    wf.logs.push(LogLine {
                        level: "info".into(),
                        stage: "cron".into(),
                        message: "Tick fired".into(),
                    });
                }
            }

            TuiEvent::TickSkipped {
                workflow_name,
                reason,
            } => {
                if let Some(wf) = self.find_workflow_mut(&workflow_name) {
                    wf.logs.push(LogLine {
                        level: "warn".into(),
                        stage: "cron".into(),
                        message: format!("Tick skipped: {}", reason),
                    });
                }
            }

            TuiEvent::RunStarted {
                workflow_name,
                run_id,
                item_label,
                ..
            } => {
                if let Some(wf) = self.find_workflow_mut(&workflow_name) {
                    wf.status = WorkflowStatus::Running;
                    wf.run_id = Some(run_id);
                    // Reset all stage statuses to pending.
                    for s in &mut wf.stage_statuses {
                        *s = StageStatus::Pending;
                    }
                    let label = item_label
                        .map(|l| format!("Run started: {}", l))
                        .unwrap_or_else(|| "Run started".to_string());
                    wf.logs.push(LogLine {
                        level: "info".into(),
                        stage: "executor".into(),
                        message: label,
                    });
                }
                // Auto-scroll to bottom when a run starts for the selected workflow.
                self.log_scroll = 0;
            }

            TuiEvent::StageStarted {
                workflow_name,
                stage_name,
                ..
            } => {
                if let Some(wf) = self.find_workflow_mut(&workflow_name) {
                    if let Some(idx) = wf.stages.iter().position(|s| *s == stage_name) {
                        wf.stage_statuses[idx] = StageStatus::Running;
                    }
                    wf.logs.push(LogLine {
                        level: "info".into(),
                        stage: stage_name.clone(),
                        message: format!("Stage started: {}", stage_name),
                    });
                }
            }

            TuiEvent::StageCompleted {
                workflow_name,
                stage_name,
                ..
            } => {
                if let Some(wf) = self.find_workflow_mut(&workflow_name) {
                    if let Some(idx) = wf.stages.iter().position(|s| *s == stage_name) {
                        wf.stage_statuses[idx] = StageStatus::Done;
                    }
                }
            }

            TuiEvent::StageFailed {
                workflow_name,
                stage_name,
                error,
                ..
            } => {
                if let Some(wf) = self.find_workflow_mut(&workflow_name) {
                    if let Some(idx) = wf.stages.iter().position(|s| *s == stage_name) {
                        wf.stage_statuses[idx] = StageStatus::Failed;
                    }
                    wf.logs.push(LogLine {
                        level: "error".into(),
                        stage: stage_name,
                        message: error,
                    });
                }
            }

            TuiEvent::LogMessage {
                workflow_name,
                level,
                stage,
                message,
                ..
            } => {
                if let Some(wf) = self.find_workflow_mut(&workflow_name) {
                    wf.logs.push(LogLine {
                        level,
                        stage,
                        message,
                    });
                }
            }

            TuiEvent::RunCompleted {
                workflow_name,
                success,
                skipped,
                error,
                pr_url,
                ..
            } => {
                if let Some(wf) = self.find_workflow_mut(&workflow_name) {
                    wf.status = if skipped {
                        WorkflowStatus::Skipped
                    } else if success {
                        WorkflowStatus::Success
                    } else {
                        WorkflowStatus::Failed
                    };
                    wf.run_id = None;

                    let mut msg = format!("Run completed: {}", wf.status);
                    if let Some(url) = pr_url {
                        msg.push_str(&format!(" | PR: {}", url));
                    }
                    if let Some(err) = error {
                        msg.push_str(&format!(" | Error: {}", err));
                    }
                    wf.logs.push(LogLine {
                        level: if success { "info" } else { "error" }.into(),
                        stage: "executor".into(),
                        message: msg,
                    });
                }
            }

            TuiEvent::Shutdown => {
                self.should_quit = true;
            }
        }
    }

    /// Handle a keyboard event.
    pub fn handle_key(&mut self, key: KeyCode, modifiers: KeyModifiers) {
        match self.input_mode {
            InputMode::Normal => match key {
                KeyCode::Up => {
                    if self.selected > 0 {
                        self.selected -= 1;
                        self.log_scroll = 0;
                    }
                }
                KeyCode::Down => {
                    if self.selected + 1 < self.workflows.len() {
                        self.selected += 1;
                        self.log_scroll = 0;
                    }
                }
                KeyCode::PageUp => {
                    self.log_scroll = self.log_scroll.saturating_add(10);
                }
                KeyCode::PageDown => {
                    self.log_scroll = self.log_scroll.saturating_sub(10);
                }
                KeyCode::Home => {
                    // Scroll to top of logs.
                    if let Some(wf) = self.selected_workflow() {
                        self.log_scroll = wf.logs.len().saturating_sub(1);
                    }
                }
                KeyCode::End => {
                    // Scroll to bottom (auto-tail).
                    self.log_scroll = 0;
                }
                KeyCode::Char(':') => {
                    self.input_mode = InputMode::Command;
                }
                KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                    self.should_quit = true;
                }
                _ => {}
            },
            InputMode::Command => match key {
                KeyCode::Char('q') => {
                    self.should_quit = true;
                }
                KeyCode::Char('p') => {
                    // Pause/resume: abort the currently running workflow.
                    // (Future: wire into the abort flag on StageContext.)
                    self.input_mode = InputMode::Normal;
                }
                KeyCode::Char('e') => {
                    self.toggle_selected_workflow();
                    self.input_mode = InputMode::Normal;
                }
                KeyCode::Esc => {
                    self.input_mode = InputMode::Normal;
                }
                _ => {
                    self.input_mode = InputMode::Normal;
                }
            },
        }
    }

    /// Toggle the enabled state of the currently selected workflow.
    ///
    /// Updates the in-memory session, the workflow state, and the shared
    /// disabled-workflows set.  Marks the session dirty for async persistence.
    pub fn toggle_selected_workflow(&mut self) {
        if let Some(wf) = self.workflows.get_mut(self.selected) {
            let new_enabled = self.session.toggle_enabled(&wf.name);
            wf.enabled = new_enabled;
            if new_enabled {
                // Restore to Idle if was Disabled
                if wf.status == WorkflowStatus::Disabled {
                    wf.status = WorkflowStatus::Idle;
                }
                wf.logs.push(LogLine {
                    level: "info".into(),
                    stage: "session".into(),
                    message: "Workflow enabled".into(),
                });
            } else {
                wf.status = WorkflowStatus::Disabled;
                wf.logs.push(LogLine {
                    level: "warn".into(),
                    stage: "session".into(),
                    message: "Workflow disabled".into(),
                });
            }
            self.session_dirty = true;
        }
    }

    fn find_workflow_mut(&mut self, name: &str) -> Option<&mut WorkflowState> {
        self.workflows.iter_mut().find(|w| w.name == name)
    }
}

/// Return the ordered stage names for a given workflow mode.
fn stages_for_mode(mode: WorkflowMode) -> Vec<String> {
    match mode {
        WorkflowMode::Issues => vec![
            "agent-loop".into(),
            "review-feedback-loop".into(),
            "pr-description".into(),
            "pull-request".into(),
        ],
        WorkflowMode::PrReview => vec![
            "pr-checkout".into(),
            "agent-loop".into(),
            "pr-review-poster".into(),
        ],
        WorkflowMode::PrResponse => vec![
            "agent-loop".into(),
            "review-feedback-loop".into(),
            "pull-request".into(),
            "pr-comment-responder".into(),
        ],
        WorkflowMode::Standard => vec![
            "agent-loop".into(),
            "review-feedback-loop".into(),
            "pr-description".into(),
            "pull-request".into(),
        ],
    }
}

/// Set up the terminal, run the event loop, and restore on exit.
pub async fn run_app(config: HarnessConfig, no_workspace: bool) -> Result<()> {
    // Determine project root (where config.toml lives).
    let project_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    // Load session config (.session.toml).
    let sess = session::load_session(&project_root).await;

    // Build the shared disabled-workflows set from session.
    let disabled: HashSet<String> = config
        .workflows
        .iter()
        .filter(|p| !sess.is_enabled(&p.name))
        .map(|p| p.name.clone())
        .collect();
    let disabled_workflows = Arc::new(Mutex::new(disabled));

    // Set up the TUI event channel.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<TuiEvent>();
    let tui_tx = TuiEventSender::new(tx);

    // Spawn the cron runner in the background.
    let runner = CronRunner::new(config, no_workspace)
        .with_tui_sender(tui_tx.clone())
        .with_disabled_workflows(disabled_workflows.clone());
    let cron_handle = tokio::spawn(async move {
        if let Err(e) = runner.start().await {
            tracing::error!("Cron runner error: {}", e);
        }
    });

    // Suppress tracing output to stdout — the TUI gets log data via the
    // event channel; any tracing writes to stdout would corrupt the screen.
    tracing::subscriber::set_global_default(tracing_subscriber::registry())
        .ok();

    // Set up the terminal.
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::with_session(sess, project_root, disabled_workflows);

    // Main event loop.
    loop {
        // Draw the UI.
        terminal.draw(|f| ui::draw(f, &app))?;

        // Process all pending TUI events (non-blocking drain).
        while let Ok(tui_event) = rx.try_recv() {
            app.handle_tui_event(tui_event);
        }

        // Persist session to disk if it changed.
        if app.session_dirty {
            app.session_dirty = false;
            // Update the shared disabled set for the CronRunner.
            {
                let mut set = app.disabled_workflows.lock().await;
                set.clear();
                for wf in &app.workflows {
                    if !wf.enabled {
                        set.insert(wf.name.clone());
                    }
                }
            }
            if let Some(ref root) = app.project_root {
                let _ = session::save_session(root, &app.session).await;
            }
        }

        // Poll for terminal input events with a short timeout so we keep
        // re-rendering even when no keys are pressed (to show new log lines).
        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key_event) = event::read()? {
                app.handle_key(key_event.code, key_event.modifiers);
            }
        }

        if app.should_quit {
            break;
        }
    }

    // Restore terminal.
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    // Cancel the cron runner.
    cron_handle.abort();

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::events::{WorkflowMode, TuiEvent};

    /// Helper: create an app with one registered workflow.
    fn app_with_workflow(name: &str, mode: WorkflowMode) -> App {
        let mut app = App::new();
        app.handle_tui_event(TuiEvent::WorkflowRegistered {
            name: name.into(),
            schedule: "* * * * *".into(),
            mode,
            repo: Some("owner/repo".into()),
        });
        app
    }

    /// Helper: register a workflow and start a run on it.
    fn app_with_running_workflow(name: &str, mode: WorkflowMode) -> App {
        let mut app = app_with_workflow(name, mode);
        app.handle_tui_event(TuiEvent::RunStarted {
            workflow_name: name.into(),
            run_id: "run-1".into(),
            mode,
            item_label: Some("issue #1".into()),
        });
        app
    }

    // ── Construction ──────────────────────────────────────────────────────

    #[test]
    fn test_app_new_is_empty() {
        let app = App::new();
        assert!(app.workflows.is_empty());
        assert_eq!(app.selected, 0);
        assert_eq!(app.input_mode, InputMode::Normal);
        assert!(!app.should_quit);
    }

    #[test]
    fn test_app_default_matches_new() {
        let a = App::new();
        let b = App::default();
        assert_eq!(a.workflows.len(), b.workflows.len());
        assert_eq!(a.selected, b.selected);
        assert_eq!(a.input_mode, b.input_mode);
        assert_eq!(a.should_quit, b.should_quit);
    }

    #[test]
    fn test_selected_workflow_none_when_empty() {
        let app = App::new();
        assert!(app.selected_workflow().is_none());
    }

    // ── WorkflowRegistered ────────────────────────────────────────────────

    #[test]
    fn test_workflow_registered_adds_workflow() {
        let mut app = App::new();
        app.handle_tui_event(TuiEvent::WorkflowRegistered {
            name: "test".into(),
            schedule: "*/5 * * * *".into(),
            mode: WorkflowMode::Issues,
            repo: Some("owner/repo".into()),
        });
        assert_eq!(app.workflows.len(), 1);
        assert_eq!(app.workflows[0].name, "test");
        assert_eq!(app.workflows[0].stages.len(), 4);
        assert_eq!(app.workflows[0].repo, Some("owner/repo".into()));
        assert_eq!(app.workflows[0].status, WorkflowStatus::Idle);
    }

    #[test]
    fn test_multiple_workflows_registered() {
        let mut app = App::new();
        for (name, mode) in &[
            ("issues", WorkflowMode::Issues),
            ("reviews", WorkflowMode::PrReview),
            ("responses", WorkflowMode::PrResponse),
        ] {
            app.handle_tui_event(TuiEvent::WorkflowRegistered {
                name: (*name).into(),
                schedule: "* * * * *".into(),
                mode: *mode,
                repo: None,
            });
        }
        assert_eq!(app.workflows.len(), 3);
        assert_eq!(app.workflows[1].name, "reviews");
        // PR review has 3 stages
        assert_eq!(app.workflows[1].stages.len(), 3);
    }

    #[test]
    fn test_registered_workflow_no_repo() {
        let _app = app_with_workflow("p", WorkflowMode::Standard);
        // Override helper — check with None repo
        let mut app2 = App::new();
        app2.handle_tui_event(TuiEvent::WorkflowRegistered {
            name: "p".into(),
            schedule: "* * * * *".into(),
            mode: WorkflowMode::Standard,
            repo: None,
        });
        assert!(app2.workflows[0].repo.is_none());
    }

    // ── TickFired / TickSkipped ───────────────────────────────────────────

    #[test]
    fn test_tick_fired_adds_log() {
        let mut app = app_with_workflow("p", WorkflowMode::Standard);
        app.handle_tui_event(TuiEvent::TickFired {
            workflow_name: "p".into(),
        });
        assert_eq!(app.workflows[0].logs.len(), 1);
        assert_eq!(app.workflows[0].logs[0].stage, "cron");
        assert_eq!(app.workflows[0].logs[0].message, "Tick fired");
    }

    #[test]
    fn test_tick_skipped_adds_log_with_reason() {
        let mut app = app_with_workflow("p", WorkflowMode::Standard);
        app.handle_tui_event(TuiEvent::TickSkipped {
            workflow_name: "p".into(),
            reason: "previous run still active".into(),
        });
        assert_eq!(app.workflows[0].logs.len(), 1);
        assert_eq!(app.workflows[0].logs[0].level, "warn");
        assert!(app.workflows[0].logs[0]
            .message
            .contains("previous run still active"));
    }

    #[test]
    fn test_tick_for_unknown_workflow_is_ignored() {
        let mut app = app_with_workflow("p", WorkflowMode::Standard);
        app.handle_tui_event(TuiEvent::TickFired {
            workflow_name: "nonexistent".into(),
        });
        // Should not crash, and "p" should have no logs
        assert!(app.workflows[0].logs.is_empty());
    }

    // ── RunStarted ────────────────────────────────────────────────────────

    #[test]
    fn test_run_started_sets_running_status() {
        let app = app_with_running_workflow("p", WorkflowMode::Issues);
        assert_eq!(app.workflows[0].status, WorkflowStatus::Running);
        assert_eq!(app.workflows[0].run_id, Some("run-1".into()));
    }

    #[test]
    fn test_run_started_resets_stage_statuses() {
        let mut app = app_with_workflow("p", WorkflowMode::Issues);
        // Simulate a prior run that completed some stages
        app.workflows[0].stage_statuses[0] = StageStatus::Done;
        app.workflows[0].stage_statuses[1] = StageStatus::Failed;

        app.handle_tui_event(TuiEvent::RunStarted {
            workflow_name: "p".into(),
            run_id: "run-2".into(),
            mode: WorkflowMode::Issues,
            item_label: None,
        });

        // All statuses should be reset to Pending
        for status in &app.workflows[0].stage_statuses {
            assert_eq!(*status, StageStatus::Pending);
        }
    }

    #[test]
    fn test_run_started_adds_log_with_item_label() {
        let app = app_with_running_workflow("p", WorkflowMode::Issues);
        assert_eq!(app.workflows[0].logs.len(), 1);
        assert!(app.workflows[0].logs[0].message.contains("issue #1"));
    }

    #[test]
    fn test_run_started_without_item_label() {
        let mut app = app_with_workflow("p", WorkflowMode::Standard);
        app.handle_tui_event(TuiEvent::RunStarted {
            workflow_name: "p".into(),
            run_id: "r".into(),
            mode: WorkflowMode::Standard,
            item_label: None,
        });
        assert_eq!(app.workflows[0].logs[0].message, "Run started");
    }

    #[test]
    fn test_run_started_resets_scroll_to_auto_tail() {
        let mut app = app_with_workflow("p", WorkflowMode::Standard);
        app.log_scroll = 50;
        app.handle_tui_event(TuiEvent::RunStarted {
            workflow_name: "p".into(),
            run_id: "r".into(),
            mode: WorkflowMode::Standard,
            item_label: None,
        });
        assert_eq!(app.log_scroll, 0);
    }

    // ── StageStarted / StageCompleted / StageFailed ──────────────────────

    #[test]
    fn test_stage_started_sets_running() {
        let mut app = app_with_running_workflow("p", WorkflowMode::Issues);
        app.handle_tui_event(TuiEvent::StageStarted {
            workflow_name: "p".into(),
            run_id: "run-1".into(),
            stage_name: "agent-loop".into(),
        });
        assert_eq!(app.workflows[0].stage_statuses[0], StageStatus::Running);
        // Other stages remain Pending
        assert_eq!(app.workflows[0].stage_statuses[1], StageStatus::Pending);
    }

    #[test]
    fn test_stage_started_adds_log() {
        let mut app = app_with_running_workflow("p", WorkflowMode::Issues);
        let initial_logs = app.workflows[0].logs.len();
        app.handle_tui_event(TuiEvent::StageStarted {
            workflow_name: "p".into(),
            run_id: "run-1".into(),
            stage_name: "agent-loop".into(),
        });
        assert_eq!(app.workflows[0].logs.len(), initial_logs + 1);
        assert!(app.workflows[0].logs.last().unwrap().message.contains("agent-loop"));
    }

    #[test]
    fn test_stage_completed_sets_done() {
        let mut app = app_with_running_workflow("p", WorkflowMode::Issues);
        app.handle_tui_event(TuiEvent::StageStarted {
            workflow_name: "p".into(),
            run_id: "run-1".into(),
            stage_name: "agent-loop".into(),
        });
        app.handle_tui_event(TuiEvent::StageCompleted {
            workflow_name: "p".into(),
            run_id: "run-1".into(),
            stage_name: "agent-loop".into(),
        });
        assert_eq!(app.workflows[0].stage_statuses[0], StageStatus::Done);
    }

    #[test]
    fn test_stage_failed_sets_failed_and_logs_error() {
        let mut app = app_with_running_workflow("p", WorkflowMode::Issues);
        app.handle_tui_event(TuiEvent::StageFailed {
            workflow_name: "p".into(),
            run_id: "run-1".into(),
            stage_name: "agent-loop".into(),
            error: "timeout after 600s".into(),
        });
        assert_eq!(app.workflows[0].stage_statuses[0], StageStatus::Failed);
        let last_log = app.workflows[0].logs.last().unwrap();
        assert_eq!(last_log.level, "error");
        assert!(last_log.message.contains("timeout after 600s"));
    }

    #[test]
    fn test_stage_event_for_unknown_stage_name_does_not_crash() {
        let mut app = app_with_running_workflow("p", WorkflowMode::Issues);
        // "nonexistent" is not in the stage list — should not panic
        app.handle_tui_event(TuiEvent::StageStarted {
            workflow_name: "p".into(),
            run_id: "run-1".into(),
            stage_name: "nonexistent".into(),
        });
        // No status changed to Running
        for status in &app.workflows[0].stage_statuses {
            assert_eq!(*status, StageStatus::Pending);
        }
    }

    // ── LogMessage ────────────────────────────────────────────────────────

    #[test]
    fn test_log_message_appends_to_workflow() {
        let mut app = app_with_workflow("p", WorkflowMode::Standard);
        app.handle_tui_event(TuiEvent::LogMessage {
            workflow_name: "p".into(),
            run_id: "r".into(),
            level: "info".into(),
            stage: "agent-loop".into(),
            message: "Starting agent".into(),
        });
        assert_eq!(app.workflows[0].logs.len(), 1);
        assert_eq!(app.workflows[0].logs[0].level, "info");
        assert_eq!(app.workflows[0].logs[0].stage, "agent-loop");
        assert_eq!(app.workflows[0].logs[0].message, "Starting agent");
    }

    #[test]
    fn test_log_messages_accumulate() {
        let mut app = app_with_workflow("p", WorkflowMode::Standard);
        for i in 0..100 {
            app.handle_tui_event(TuiEvent::LogMessage {
                workflow_name: "p".into(),
                run_id: "r".into(),
                level: "info".into(),
                stage: "agent".into(),
                message: format!("line {}", i),
            });
        }
        assert_eq!(app.workflows[0].logs.len(), 100);
        assert_eq!(app.workflows[0].logs[99].message, "line 99");
    }

    #[test]
    fn test_log_for_unknown_workflow_is_ignored() {
        let mut app = app_with_workflow("p", WorkflowMode::Standard);
        app.handle_tui_event(TuiEvent::LogMessage {
            workflow_name: "other".into(),
            run_id: "r".into(),
            level: "info".into(),
            stage: "x".into(),
            message: "ignored".into(),
        });
        assert!(app.workflows[0].logs.is_empty());
    }

    // ── RunCompleted ──────────────────────────────────────────────────────

    #[test]
    fn test_run_completed_success() {
        let mut app = app_with_running_workflow("p", WorkflowMode::Standard);
        app.handle_tui_event(TuiEvent::RunCompleted {
            workflow_name: "p".into(),
            run_id: "run-1".into(),
            success: true,
            skipped: false,
            error: None,
            pr_url: Some("https://github.com/foo/bar/pull/1".into()),
        });
        assert_eq!(app.workflows[0].status, WorkflowStatus::Success);
        assert!(app.workflows[0].run_id.is_none()); // cleared after completion
        let last_log = app.workflows[0].logs.last().unwrap();
        assert!(last_log.message.contains("success"));
        assert!(last_log.message.contains("github.com/foo/bar/pull/1"));
    }

    #[test]
    fn test_run_completed_failed() {
        let mut app = app_with_running_workflow("p", WorkflowMode::Standard);
        app.handle_tui_event(TuiEvent::RunCompleted {
            workflow_name: "p".into(),
            run_id: "run-1".into(),
            success: false,
            skipped: false,
            error: Some("provider timeout".into()),
            pr_url: None,
        });
        assert_eq!(app.workflows[0].status, WorkflowStatus::Failed);
        let last_log = app.workflows[0].logs.last().unwrap();
        assert_eq!(last_log.level, "error");
        assert!(last_log.message.contains("provider timeout"));
    }

    #[test]
    fn test_run_completed_skipped() {
        let mut app = app_with_running_workflow("p", WorkflowMode::Standard);
        app.handle_tui_event(TuiEvent::RunCompleted {
            workflow_name: "p".into(),
            run_id: "run-1".into(),
            success: true,
            skipped: true,
            error: None,
            pr_url: None,
        });
        assert_eq!(app.workflows[0].status, WorkflowStatus::Skipped);
    }

    // ── Full lifecycle ────────────────────────────────────────────────────

    #[test]
    fn test_full_run_lifecycle() {
        let mut app = app_with_workflow("bug-fixer", WorkflowMode::Issues);
        let stages = app.workflows[0].stages.clone();

        // Tick fires
        app.handle_tui_event(TuiEvent::TickFired {
            workflow_name: "bug-fixer".into(),
        });

        // Run starts
        app.handle_tui_event(TuiEvent::RunStarted {
            workflow_name: "bug-fixer".into(),
            run_id: "abc-123".into(),
            mode: WorkflowMode::Issues,
            item_label: Some("issue #42".into()),
        });
        assert_eq!(app.workflows[0].status, WorkflowStatus::Running);

        // Walk through all stages
        for stage in &stages {
            app.handle_tui_event(TuiEvent::StageStarted {
                workflow_name: "bug-fixer".into(),
                run_id: "abc-123".into(),
                stage_name: stage.clone(),
            });
            // Simulate a log message from the stage
            app.handle_tui_event(TuiEvent::LogMessage {
                workflow_name: "bug-fixer".into(),
                run_id: "abc-123".into(),
                level: "info".into(),
                stage: stage.clone(),
                message: format!("{} working...", stage),
            });
            app.handle_tui_event(TuiEvent::StageCompleted {
                workflow_name: "bug-fixer".into(),
                run_id: "abc-123".into(),
                stage_name: stage.clone(),
            });
        }

        // All stages should be Done
        for status in &app.workflows[0].stage_statuses {
            assert_eq!(*status, StageStatus::Done);
        }

        // Run completes
        app.handle_tui_event(TuiEvent::RunCompleted {
            workflow_name: "bug-fixer".into(),
            run_id: "abc-123".into(),
            success: true,
            skipped: false,
            error: None,
            pr_url: Some("https://github.com/o/r/pull/99".into()),
        });
        assert_eq!(app.workflows[0].status, WorkflowStatus::Success);
        // tick + run_started + (stage_started + log + stage_completed doesn't log
        //   on complete) * 4 stages + run_completed = a bunch of logs
        assert!(app.workflows[0].logs.len() >= 6);
    }

    // ── Keyboard input ────────────────────────────────────────────────────

    #[test]
    fn test_key_navigation() {
        let mut app = App::new();
        for name in &["a", "b"] {
            app.handle_tui_event(TuiEvent::WorkflowRegistered {
                name: (*name).into(),
                schedule: "* * * * *".into(),
                mode: WorkflowMode::Standard,
                repo: None,
            });
        }
        assert_eq!(app.selected, 0);
        app.handle_key(KeyCode::Down, KeyModifiers::empty());
        assert_eq!(app.selected, 1);
        app.handle_key(KeyCode::Down, KeyModifiers::empty());
        assert_eq!(app.selected, 1); // can't go past end
        app.handle_key(KeyCode::Up, KeyModifiers::empty());
        assert_eq!(app.selected, 0);
        app.handle_key(KeyCode::Up, KeyModifiers::empty());
        assert_eq!(app.selected, 0); // can't go before start
    }

    #[test]
    fn test_navigation_resets_scroll() {
        let mut app = App::new();
        for name in &["a", "b"] {
            app.handle_tui_event(TuiEvent::WorkflowRegistered {
                name: (*name).into(),
                schedule: "* * * * *".into(),
                mode: WorkflowMode::Standard,
                repo: None,
            });
        }
        app.log_scroll = 20;
        app.handle_key(KeyCode::Down, KeyModifiers::empty());
        assert_eq!(app.log_scroll, 0);
    }

    #[test]
    fn test_page_up_down_scrolls() {
        let mut app = App::new();
        assert_eq!(app.log_scroll, 0);
        app.handle_key(KeyCode::PageUp, KeyModifiers::empty());
        assert_eq!(app.log_scroll, 10);
        app.handle_key(KeyCode::PageUp, KeyModifiers::empty());
        assert_eq!(app.log_scroll, 20);
        app.handle_key(KeyCode::PageDown, KeyModifiers::empty());
        assert_eq!(app.log_scroll, 10);
        app.handle_key(KeyCode::PageDown, KeyModifiers::empty());
        assert_eq!(app.log_scroll, 0);
        // PageDown at 0 stays at 0
        app.handle_key(KeyCode::PageDown, KeyModifiers::empty());
        assert_eq!(app.log_scroll, 0);
    }

    #[test]
    fn test_home_scrolls_to_top() {
        let mut app = app_with_workflow("p", WorkflowMode::Standard);
        // Add some log lines
        for i in 0..50 {
            app.workflows[0].logs.push(LogLine {
                level: "info".into(),
                stage: "x".into(),
                message: format!("line {}", i),
            });
        }
        app.handle_key(KeyCode::Home, KeyModifiers::empty());
        assert_eq!(app.log_scroll, 49); // logs.len() - 1
    }

    #[test]
    fn test_end_scrolls_to_bottom() {
        let mut app = App::new();
        app.log_scroll = 999;
        app.handle_key(KeyCode::End, KeyModifiers::empty());
        assert_eq!(app.log_scroll, 0);
    }

    // ── Command mode ──────────────────────────────────────────────────────

    #[test]
    fn test_colon_enters_command_mode() {
        let mut app = App::new();
        assert_eq!(app.input_mode, InputMode::Normal);
        app.handle_key(KeyCode::Char(':'), KeyModifiers::empty());
        assert_eq!(app.input_mode, InputMode::Command);
        // Esc dismisses back to normal
        app.handle_key(KeyCode::Esc, KeyModifiers::empty());
        assert_eq!(app.input_mode, InputMode::Normal);
    }

    #[test]
    fn test_quit_from_command_mode() {
        let mut app = App::new();
        app.handle_key(KeyCode::Char(':'), KeyModifiers::empty());
        app.handle_key(KeyCode::Char('q'), KeyModifiers::empty());
        assert!(app.should_quit);
    }

    #[test]
    fn test_pause_returns_to_normal() {
        let mut app = App::new();
        app.handle_key(KeyCode::Char(':'), KeyModifiers::empty());
        assert_eq!(app.input_mode, InputMode::Command);
        app.handle_key(KeyCode::Char('p'), KeyModifiers::empty());
        assert_eq!(app.input_mode, InputMode::Normal);
        assert!(!app.should_quit);
    }

    #[test]
    fn test_unknown_command_returns_to_normal() {
        let mut app = App::new();
        app.handle_key(KeyCode::Char(':'), KeyModifiers::empty());
        assert_eq!(app.input_mode, InputMode::Command);
        app.handle_key(KeyCode::Char('z'), KeyModifiers::empty());
        assert_eq!(app.input_mode, InputMode::Normal);
        assert!(!app.should_quit);
    }

    #[test]
    fn test_ctrl_c_quits_from_normal() {
        let mut app = App::new();
        app.handle_key(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(app.should_quit);
    }

    #[test]
    fn test_q_in_normal_mode_does_not_quit() {
        let mut app = App::new();
        app.handle_key(KeyCode::Char('q'), KeyModifiers::empty());
        assert!(!app.should_quit);
    }

    // ── Shutdown ──────────────────────────────────────────────────────────

    #[test]
    fn test_shutdown_event_quits() {
        let mut app = App::new();
        app.handle_tui_event(TuiEvent::Shutdown);
        assert!(app.should_quit);
    }

    // ── stages_for_mode ───────────────────────────────────────────────────

    #[test]
    fn test_stages_for_mode() {
        assert_eq!(stages_for_mode(WorkflowMode::Issues).len(), 4);
        assert_eq!(stages_for_mode(WorkflowMode::PrReview).len(), 3);
        assert_eq!(stages_for_mode(WorkflowMode::PrResponse).len(), 4);
        assert_eq!(stages_for_mode(WorkflowMode::Standard).len(), 4);
    }

    #[test]
    fn test_stages_for_issues_are_correct() {
        let stages = stages_for_mode(WorkflowMode::Issues);
        assert_eq!(stages[0], "agent-loop");
        assert_eq!(stages[1], "review-feedback-loop");
        assert_eq!(stages[2], "pr-description");
        assert_eq!(stages[3], "pull-request");
    }

    #[test]
    fn test_stages_for_pr_review_are_correct() {
        let stages = stages_for_mode(WorkflowMode::PrReview);
        assert_eq!(stages[0], "pr-checkout");
        assert_eq!(stages[1], "agent-loop");
        assert_eq!(stages[2], "pr-review-poster");
    }

    // ── WorkflowStatus display ────────────────────────────────────────────

    #[test]
    fn test_workflow_status_display() {
        assert_eq!(WorkflowStatus::Idle.to_string(), "idle");
        assert_eq!(WorkflowStatus::Running.to_string(), "running");
        assert_eq!(WorkflowStatus::Success.to_string(), "success");
        assert_eq!(WorkflowStatus::Failed.to_string(), "failed");
        assert_eq!(WorkflowStatus::Skipped.to_string(), "skipped");
        assert_eq!(WorkflowStatus::Disabled.to_string(), "disabled");
    }

    // ── Enable/disable ───────────────────────────────────────────────────

    #[test]
    fn test_workflows_enabled_by_default() {
        let app = app_with_workflow("p", WorkflowMode::Standard);
        assert!(app.workflows[0].enabled);
        assert_eq!(app.workflows[0].status, WorkflowStatus::Idle);
    }

    #[test]
    fn test_session_disabled_workflow_starts_disabled() {
        let mut app = App::new();
        app.session.set_enabled("p", false);
        app.handle_tui_event(TuiEvent::WorkflowRegistered {
            name: "p".into(),
            schedule: "* * * * *".into(),
            mode: WorkflowMode::Standard,
            repo: None,
        });
        assert!(!app.workflows[0].enabled);
        assert_eq!(app.workflows[0].status, WorkflowStatus::Disabled);
    }

    #[test]
    fn test_toggle_selected_workflow() {
        let mut app = app_with_workflow("p", WorkflowMode::Standard);
        assert!(app.workflows[0].enabled);

        // Toggle to disabled
        app.toggle_selected_workflow();
        assert!(!app.workflows[0].enabled);
        assert_eq!(app.workflows[0].status, WorkflowStatus::Disabled);
        assert!(app.session_dirty);
        assert!(!app.session.is_enabled("p"));

        // Toggle back to enabled
        app.session_dirty = false;
        app.toggle_selected_workflow();
        assert!(app.workflows[0].enabled);
        assert_eq!(app.workflows[0].status, WorkflowStatus::Idle);
        assert!(app.session_dirty);
        assert!(app.session.is_enabled("p"));
    }

    #[test]
    fn test_toggle_adds_log_entries() {
        let mut app = app_with_workflow("p", WorkflowMode::Standard);
        let initial_logs = app.workflows[0].logs.len();

        app.toggle_selected_workflow();
        assert_eq!(app.workflows[0].logs.len(), initial_logs + 1);
        assert!(app.workflows[0].logs.last().unwrap().message.contains("disabled"));

        app.toggle_selected_workflow();
        assert_eq!(app.workflows[0].logs.len(), initial_logs + 2);
        assert!(app.workflows[0].logs.last().unwrap().message.contains("enabled"));
    }

    #[test]
    fn test_e_key_toggles_in_command_mode() {
        let mut app = app_with_workflow("p", WorkflowMode::Standard);
        assert!(app.workflows[0].enabled);

        // Enter command mode, press 'e'
        app.handle_key(KeyCode::Char(':'), KeyModifiers::empty());
        app.handle_key(KeyCode::Char('e'), KeyModifiers::empty());

        // Should have toggled and returned to normal mode
        assert!(!app.workflows[0].enabled);
        assert_eq!(app.input_mode, InputMode::Normal);
    }

    #[test]
    fn test_toggle_empty_app_does_not_crash() {
        let mut app = App::new();
        app.toggle_selected_workflow(); // no workflows — should not panic
        assert!(!app.session_dirty);
    }

    #[test]
    fn test_toggle_preserves_success_status_when_re_enabled() {
        let mut app = app_with_workflow("p", WorkflowMode::Standard);
        app.workflows[0].status = WorkflowStatus::Success;

        // Disable
        app.toggle_selected_workflow();
        assert_eq!(app.workflows[0].status, WorkflowStatus::Disabled);

        // Re-enable — status goes to Idle (not back to Success, since it was overwritten)
        app.toggle_selected_workflow();
        assert_eq!(app.workflows[0].status, WorkflowStatus::Idle);
    }

    // ── Multi-workflow scenarios ──────────────────────────────────────────

    #[test]
    fn test_events_target_correct_workflow() {
        let mut app = App::new();
        app.handle_tui_event(TuiEvent::WorkflowRegistered {
            name: "a".into(),
            schedule: "* * * * *".into(),
            mode: WorkflowMode::Issues,
            repo: None,
        });
        app.handle_tui_event(TuiEvent::WorkflowRegistered {
            name: "b".into(),
            schedule: "* * * * *".into(),
            mode: WorkflowMode::PrReview,
            repo: None,
        });

        // Log to "b" only
        app.handle_tui_event(TuiEvent::LogMessage {
            workflow_name: "b".into(),
            run_id: "r".into(),
            level: "info".into(),
            stage: "test".into(),
            message: "hello".into(),
        });

        assert!(app.workflows[0].logs.is_empty());
        assert_eq!(app.workflows[1].logs.len(), 1);
    }

    #[test]
    fn test_selected_workflow_changes_with_navigation() {
        let mut app = App::new();
        for name in &["x", "y", "z"] {
            app.handle_tui_event(TuiEvent::WorkflowRegistered {
                name: (*name).into(),
                schedule: "* * * * *".into(),
                mode: WorkflowMode::Standard,
                repo: None,
            });
        }
        assert_eq!(app.selected_workflow().unwrap().name, "x");
        app.handle_key(KeyCode::Down, KeyModifiers::empty());
        assert_eq!(app.selected_workflow().unwrap().name, "y");
        app.handle_key(KeyCode::Down, KeyModifiers::empty());
        assert_eq!(app.selected_workflow().unwrap().name, "z");
    }
}
