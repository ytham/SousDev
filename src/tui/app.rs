/// TUI application state and event loop.
///
/// The `App` struct holds all mutable state needed to render the dashboard.
/// The `run_app` function sets up the terminal, spawns the cron scheduler,
/// and drives the ratatui draw/input loop.
use std::collections::{HashMap, HashSet};
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::Mutex;

use crate::workflows::cron_runner::CronRunner;
use crate::tui::events::{ItemStatus, ItemSummary, WorkflowMode, TuiEvent, TuiEventSender};
use crate::tui::session::{self, SessionConfig};
use crate::tui::ui;
use crate::types::config::HarnessConfig;

/// The input mode determines how keystrokes are interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    /// Normal mode: up/down selects workflow, page-up/page-down scrolls logs.
    Normal,
    /// Command mode: activated by `:`, shows the command menu.
    Command,
    /// Cron edit mode: text input for updating the selected workflow's schedule.
    CronEdit,
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
    /// The item label of the currently running item (for info panel updates).
    pub current_item_label: Option<String>,
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

/// Which panel a mouse event landed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    Sidebar,
    Logs,
    InfoBar,
    InfoPanel,
    None,
}

/// Active text selection state for the log pane.
#[derive(Debug, Clone, Default)]
pub struct TextSelection {
    /// Whether a drag is in progress.
    pub active: bool,
    /// Panel where the drag started.
    pub panel: Option<Panel>,
    /// Anchor row (terminal y) where the drag started.
    pub start_row: u16,
    /// Anchor column (terminal x) where the drag started.
    pub start_col: u16,
    /// Current row (terminal y) of the drag.
    pub end_row: u16,
    /// Current column (terminal x) of the drag.
    pub end_col: u16,
}

/// Computed panel rectangles for hit-testing mouse events.
#[derive(Debug, Clone, Default)]
pub struct PanelLayout {
    pub sidebar: ratatui::layout::Rect,
    pub logs: ratatui::layout::Rect,
    pub info_bar: ratatui::layout::Rect,
    pub info_panel: ratatui::layout::Rect,
}

impl PanelLayout {
    /// Determine which panel a terminal coordinate falls in.
    pub fn hit_test(&self, col: u16, row: u16) -> Panel {
        // Info panel takes priority (it floats over logs).
        if self.info_panel.width > 0 && contains(&self.info_panel, col, row) {
            Panel::InfoPanel
        } else if contains(&self.logs, col, row) {
            Panel::Logs
        } else if contains(&self.sidebar, col, row) {
            Panel::Sidebar
        } else if contains(&self.info_bar, col, row) {
            Panel::InfoBar
        } else {
            Panel::None
        }
    }
}

/// Check if a point is inside a Rect.
fn contains(r: &ratatui::layout::Rect, col: u16, row: u16) -> bool {
    col >= r.x && col < r.x + r.width && row >= r.y && row < r.y + r.height
}

/// A temporary toast notification shown in the TUI.
#[derive(Debug, Clone)]
pub struct Toast {
    /// Message to display.
    pub message: String,
    /// When the toast expires and should stop rendering.
    pub expires_at: std::time::Instant,
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
    /// Computed panel layout (updated each frame by `ui::draw`).
    pub layout: PanelLayout,
    /// Current text selection state.
    pub selection: TextSelection,
    /// Active toast notification (auto-expires).
    pub toast: Option<Toast>,
    /// Text buffer for the cron edit input.
    pub cron_input: String,
    /// Whether the info panel is open.
    pub info_panel_open: bool,
    /// Index of the highlighted item in the info panel.
    pub info_panel_selected: usize,
    /// Per-workflow item summaries for the info panel.
    pub workflow_items: HashMap<String, Vec<ItemSummary>>,
    /// Pending failure cooldown clears (workflow_name, item_key).
    pub clear_requests: Vec<(String, String)>,
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
            layout: PanelLayout::default(),
            selection: TextSelection::default(),
            toast: None,
            cron_input: String::new(),
            info_panel_open: false,
            info_panel_selected: 0,
            workflow_items: HashMap::new(),
            clear_requests: Vec::new(),
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
                    current_item_label: None,
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
                // Update info panel: mark the matching item as InProgress.
                if let Some(ref label) = item_label {
                    self.update_item_status(&workflow_name, label, ItemStatus::InProgress);
                }

                if let Some(wf) = self.find_workflow_mut(&workflow_name) {
                    wf.status = WorkflowStatus::Running;
                    wf.run_id = Some(run_id);
                    wf.current_item_label = item_label.clone();
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
                let item_label = self
                    .workflows
                    .iter()
                    .find(|w| w.name == workflow_name)
                    .and_then(|w| w.current_item_label.clone());

                if let Some(wf) = self.find_workflow_mut(&workflow_name) {
                    wf.status = if skipped {
                        WorkflowStatus::Skipped
                    } else if success {
                        WorkflowStatus::Success
                    } else {
                        WorkflowStatus::Failed
                    };
                    wf.run_id = None;
                    wf.current_item_label = None;

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

                // Update info panel item status.
                if let Some(ref label) = item_label {
                    let new_status = if success {
                        ItemStatus::Success
                    } else {
                        ItemStatus::Error
                    };
                    self.update_item_status(&workflow_name, label, new_status);
                }
            }

            TuiEvent::ItemsSummary {
                workflow_name,
                items,
            } => {
                // Reset selection when items refresh for the selected workflow.
                if self
                    .selected_workflow()
                    .map(|wf| wf.name == workflow_name)
                    .unwrap_or(false)
                {
                    self.info_panel_selected = 0;
                }
                self.workflow_items.insert(workflow_name, items);
            }

            TuiEvent::Shutdown => {
                self.should_quit = true;
            }
        }
    }

    /// Handle a keyboard event.
    ///
    /// Keys are routed to the topmost active context.  Universal keys
    /// (`Ctrl+C`) are handled first, then the context stack is checked
    /// in priority order: CronEdit > Command > InfoPanel > Normal.
    pub fn handle_key(&mut self, key: KeyCode, modifiers: KeyModifiers) {
        // Universal: Ctrl+C always quits.
        if key == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL) {
            self.should_quit = true;
            return;
        }

        // Route to topmost context.
        match self.input_mode {
            InputMode::CronEdit => self.handle_key_cron_edit(key),
            InputMode::Command => self.handle_key_command(key),
            InputMode::Normal if self.info_panel_open => self.handle_key_info_panel(key),
            InputMode::Normal => self.handle_key_normal(key),
        }
    }

    /// Normal context: workflow selection, log scrolling, panel toggles.
    fn handle_key_normal(&mut self, key: KeyCode) {
        match key {
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
            KeyCode::PageUp | KeyCode::Char('b') => {
                let page = self.layout.logs.height.max(1) as usize;
                self.log_scroll = self.log_scroll.saturating_add(page);
            }
            KeyCode::PageDown | KeyCode::Char('f') => {
                let page = self.layout.logs.height.max(1) as usize;
                self.log_scroll = self.log_scroll.saturating_sub(page);
            }
            KeyCode::Home | KeyCode::Char('B') => {
                if let Some(wf) = self.selected_workflow() {
                    self.log_scroll = wf.logs.len().saturating_sub(1);
                }
            }
            KeyCode::End | KeyCode::Char('F') => {
                self.log_scroll = 0;
            }
            KeyCode::Char('i') => {
                self.info_panel_open = true;
                self.info_panel_selected = 0;
            }
            KeyCode::Char(':') => {
                self.input_mode = InputMode::Command;
            }
            _ => {}
        }
    }

    /// Info panel context: item navigation, clear, open URL.
    fn handle_key_info_panel(&mut self, key: KeyCode) {
        match key {
            KeyCode::Up => {
                self.info_panel_selected = self.info_panel_selected.saturating_sub(1);
            }
            KeyCode::Down => {
                let max = self.selected_items().len().saturating_sub(1);
                if self.info_panel_selected < max {
                    self.info_panel_selected += 1;
                }
            }
            KeyCode::Enter => {
                self.open_selected_info_item();
            }
            KeyCode::Char('c') => {
                self.clear_selected_item();
            }
            KeyCode::Char('C') => {
                self.clear_all_errored_items();
            }
            KeyCode::Char('i') | KeyCode::Esc => {
                self.info_panel_open = false;
            }
            KeyCode::Char(':') => {
                self.input_mode = InputMode::Command;
            }
            _ => {}
        }
    }

    /// Command menu context: single-key actions.
    fn handle_key_command(&mut self, key: KeyCode) {
        match key {
            KeyCode::Char('q') => {
                self.should_quit = true;
            }
            KeyCode::Char('p') => {
                self.input_mode = InputMode::Normal;
            }
            KeyCode::Char('e') => {
                self.toggle_selected_workflow();
                self.input_mode = InputMode::Normal;
            }
            KeyCode::Char('c') => {
                self.cron_input.clear();
                if let Some(wf) = self.selected_workflow() {
                    self.cron_input = wf.schedule.clone();
                }
                self.input_mode = InputMode::CronEdit;
            }
            KeyCode::Esc => {
                self.input_mode = InputMode::Normal;
            }
            _ => {
                self.input_mode = InputMode::Normal;
            }
        }
    }

    /// Cron edit context: text input.
    fn handle_key_cron_edit(&mut self, key: KeyCode) {
        match key {
            KeyCode::Enter => {
                self.apply_cron_input();
                self.input_mode = InputMode::Normal;
            }
            KeyCode::Esc => {
                self.input_mode = InputMode::Normal;
            }
            KeyCode::Backspace => {
                self.cron_input.pop();
            }
            KeyCode::Char(ch) => {
                self.cron_input.push(ch);
            }
            _ => {}
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

    /// Handle a mouse event.
    pub fn handle_mouse(&mut self, event: MouseEvent) {
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let panel = self.layout.hit_test(event.column, event.row);

                // Click in sidebar: select the clicked workflow.
                if panel == Panel::Sidebar {
                    if let Some(idx) = self.sidebar_row_to_workflow(event.row) {
                        if idx != self.selected {
                            self.selected = idx;
                            self.log_scroll = 0;
                            self.info_panel_selected = 0;
                        }
                    }
                }

                // Click in info panel: select the clicked item.
                if panel == Panel::InfoPanel {
                    self.select_info_panel_item(event.row);
                }

                self.selection = TextSelection {
                    active: true,
                    panel: Some(panel),
                    start_row: event.row,
                    start_col: event.column,
                    end_row: event.row,
                    end_col: event.column,
                };
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if self.selection.active {
                    // Clamp drag to the panel where the selection started.
                    let panel_rect = match self.selection.panel {
                        Some(Panel::Logs) => &self.layout.logs,
                        Some(Panel::Sidebar) => &self.layout.sidebar,
                        Some(Panel::InfoBar) => &self.layout.info_bar,
                        _ => return,
                    };
                    self.selection.end_col =
                        event.column.max(panel_rect.x).min(panel_rect.x + panel_rect.width - 1);
                    self.selection.end_row =
                        event.row.max(panel_rect.y).min(panel_rect.y + panel_rect.height - 1);
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if self.selection.active {
                    self.copy_selection_to_clipboard();
                    self.selection.active = false;
                }
            }
            MouseEventKind::ScrollUp => {
                self.log_scroll = self.log_scroll.saturating_add(3);
            }
            MouseEventKind::ScrollDown => {
                self.log_scroll = self.log_scroll.saturating_sub(3);
            }
            _ => {}
        }
    }

    /// Show a toast notification that auto-expires after the given duration.
    pub fn show_toast(&mut self, message: &str, duration: Duration) {
        self.toast = Some(Toast {
            message: message.to_string(),
            expires_at: std::time::Instant::now() + duration,
        });
    }

    /// Clear the toast if it has expired.
    pub fn tick_toast(&mut self) {
        if let Some(ref toast) = self.toast {
            if std::time::Instant::now() >= toast.expires_at {
                self.toast = None;
            }
        }
    }

    /// Extract selected text from the log pane and copy to clipboard.
    fn copy_selection_to_clipboard(&mut self) {
        if self.selection.panel != Some(Panel::Logs) {
            return;
        }

        let wf = match self.selected_workflow() {
            Some(wf) => wf,
            None => return,
        };

        if wf.logs.is_empty() {
            return;
        }

        let log_rect = &self.layout.logs;
        if log_rect.height == 0 {
            return;
        }

        // Determine which log lines are visible (same logic as log_view::draw_logs).
        let visible_height = log_rect.height as usize;
        let total = wf.logs.len();
        let start = if self.log_scroll == 0 {
            total.saturating_sub(visible_height)
        } else {
            total
                .saturating_sub(visible_height)
                .saturating_sub(self.log_scroll)
        };

        // Convert terminal rows to log-line indices.
        let sel_top = self.selection.start_row.min(self.selection.end_row);
        let sel_bot = self.selection.start_row.max(self.selection.end_row);
        let first_log_row = log_rect.y;

        let mut selected_lines = Vec::new();
        for screen_row in sel_top..=sel_bot {
            if screen_row < first_log_row {
                continue;
            }
            let line_idx = start + (screen_row - first_log_row) as usize;
            if line_idx < total {
                let log = &wf.logs[line_idx];
                selected_lines.push(format!(
                    "{:<5} [{}] {}",
                    log.level.to_uppercase(),
                    log.stage,
                    log.message
                ));
            }
        }

        if selected_lines.is_empty() {
            return;
        }

        let text = selected_lines.join("\n");
        if cli_clipboard::set_contents(text).is_ok() {
            self.show_toast("Copied to clipboard", Duration::from_secs(6));
        }
    }

    /// Clear the status of the currently selected info panel item.
    ///
    /// Resets the item to `None` and queues a failure cooldown clear so
    /// the cron runner will retry it on the next tick.
    pub fn clear_selected_item(&mut self) {
        let wf_name = match self.selected_workflow() {
            Some(wf) => wf.name.clone(),
            None => return,
        };
        if let Some(items) = self.workflow_items.get_mut(&wf_name) {
            if let Some(item) = items.get_mut(self.info_panel_selected) {
                if item.status == ItemStatus::Error || item.status == ItemStatus::Cooldown {
                    item.status = ItemStatus::None;
                    let item_key = extract_item_key(&item.id);
                    self.clear_requests.push((wf_name, item_key));
                    self.show_toast("Cleared — will retry next tick", Duration::from_secs(3));
                }
            }
        }
    }

    /// Clear all errored/cooldown items for the selected workflow.
    pub fn clear_all_errored_items(&mut self) {
        let wf_name = match self.selected_workflow() {
            Some(wf) => wf.name.clone(),
            None => return,
        };
        let mut count = 0usize;
        if let Some(items) = self.workflow_items.get_mut(&wf_name) {
            for item in items.iter_mut() {
                if item.status == ItemStatus::Error || item.status == ItemStatus::Cooldown {
                    item.status = ItemStatus::None;
                    let item_key = extract_item_key(&item.id);
                    self.clear_requests.push((wf_name.clone(), item_key));
                    count += 1;
                }
            }
        }
        if count > 0 {
            self.show_toast(
                &format!("Cleared {} item{}", count, if count == 1 { "" } else { "s" }),
                Duration::from_secs(3),
            );
        }
    }

    /// Open the URL of the currently selected info panel item in the browser.
    fn open_selected_info_item(&mut self) {
        let items = self.selected_items();
        if let Some(item) = items.get(self.info_panel_selected) {
            let url = item.url.clone();
            if !url.is_empty() {
                #[cfg(target_os = "macos")]
                let _ = std::process::Command::new("open").arg(&url).spawn();
                #[cfg(target_os = "linux")]
                let _ = std::process::Command::new("xdg-open").arg(&url).spawn();
                #[cfg(target_os = "windows")]
                let _ = std::process::Command::new("cmd").args(["/c", "start", &url]).spawn();
                self.show_toast("Opened in browser", Duration::from_secs(3));
            }
        }
    }

    /// Select an info panel item by terminal row (from mouse click).
    fn select_info_panel_item(&mut self, row: u16) {
        let panel = &self.layout.info_panel;
        if panel.height == 0 {
            return;
        }
        // Rows: 0 = title, 1 = separator, 2+ = items.
        let item_row = row.saturating_sub(panel.y + 2) as usize;
        let item_count = self.selected_items().len();
        if item_row < item_count {
            self.info_panel_selected = item_row;
        }
    }

    /// Map a terminal row in the sidebar to a workflow index.
    ///
    /// The sidebar layout is: 2 header lines (title + blank), then per
    /// workflow: name line + schedule line + N stage lines + 1 separator.
    fn sidebar_row_to_workflow(&self, row: u16) -> Option<usize> {
        let sidebar_y = self.layout.sidebar.y;
        if row < sidebar_y + 2 {
            return None; // Header area
        }
        let mut y = sidebar_y + 2; // After "Workflows" title + blank line
        for (i, wf) in self.workflows.iter().enumerate() {
            let height = 2 + wf.stages.len() as u16; // name + schedule + stages
            if row >= y && row < y + height {
                return Some(i);
            }
            y += height + 1; // +1 for separator blank line
        }
        None
    }

    /// Apply the cron edit input to the selected workflow.
    ///
    /// Parses the input as either a 6-field cron expression or a
    /// human-readable duration (e.g. "15m", "2hr", "2 hours").
    /// Updates the in-memory workflow state and marks config dirty.
    pub fn apply_cron_input(&mut self) {
        let input = self.cron_input.trim().to_string();
        if input.is_empty() {
            return;
        }

        let cron_expr = if looks_like_cron(&input) {
            input.clone()
        } else {
            match parse_human_cron(&input) {
                Some(expr) => expr,
                None => {
                    self.show_toast(
                        &format!("Invalid schedule: {}", input),
                        Duration::from_secs(4),
                    );
                    return;
                }
            }
        };

        if let Some(wf) = self.workflows.get_mut(self.selected) {
            let name = wf.name.clone();
            wf.schedule = cron_expr.clone();
            wf.logs.push(LogLine {
                level: "info".into(),
                stage: "config".into(),
                message: format!("Schedule updated to: {}", cron_expr),
            });

            // Persist to config.toml.
            if let Some(ref root) = self.project_root {
                update_config_toml_schedule(root, &name, &cron_expr);
            }
        }

        self.show_toast("Schedule updated", Duration::from_secs(4));
    }

    /// Update the status of an item in the info panel by matching the label.
    ///
    /// The label is matched against item IDs (e.g. "issue #42" matches "#42",
    /// "PR #12050" matches "PR #12050").
    fn update_item_status(&mut self, workflow_name: &str, item_label: &str, status: ItemStatus) {
        if let Some(items) = self.workflow_items.get_mut(workflow_name) {
            for item in items.iter_mut() {
                if item_label.contains(&item.id) {
                    item.status = status;
                    break;
                }
            }
        }
    }

    /// Return the items for the currently selected workflow's info panel.
    pub fn selected_items(&self) -> &[ItemSummary] {
        let empty: &[ItemSummary] = &[];
        self.selected_workflow()
            .and_then(|wf| self.workflow_items.get(&wf.name))
            .map(|v| v.as_slice())
            .unwrap_or(empty)
    }

    fn find_workflow_mut(&mut self, name: &str) -> Option<&mut WorkflowState> {
        self.workflows.iter_mut().find(|w| w.name == name)
    }
}

/// Extract a store-compatible item key from an info panel item ID.
///
/// - `"#42"` → `"42"` (issue number)
/// - `"PR #12050"` → `"pr-12050"` (PR key used in FailureCooldownStore)
fn extract_item_key(id: &str) -> String {
    if let Some(num) = id.strip_prefix("PR #") {
        format!("pr-{}", num)
    } else if let Some(num) = id.strip_prefix('#') {
        num.to_string()
    } else {
        id.to_string()
    }
}

/// Check if a string looks like a 6-field cron expression.
fn looks_like_cron(s: &str) -> bool {
    let fields: Vec<&str> = s.split_whitespace().collect();
    fields.len() == 6 && fields.iter().all(|f| {
        f.chars().all(|c| c.is_ascii_digit() || c == '*' || c == '/' || c == '-' || c == ',')
    })
}

/// Parse a human-readable duration string into a 6-field cron expression.
///
/// Supports formats like: "15m", "15 min", "15mins", "15 minutes",
/// "2h", "2hr", "2 hours", "30s", "30 sec", "30 seconds".
/// Case-insensitive.
///
/// Returns `None` if the input cannot be parsed.
fn parse_human_cron(input: &str) -> Option<String> {
    let input = input.trim().to_lowercase();

    // Try to extract a number and a unit.
    let mut num_str = String::new();
    let mut unit_str = String::new();
    let mut in_unit = false;

    for ch in input.chars() {
        if !in_unit && (ch.is_ascii_digit() || ch == '.') {
            num_str.push(ch);
        } else {
            in_unit = true;
            if ch.is_alphabetic() {
                unit_str.push(ch);
            }
        }
    }

    let n: u64 = num_str.parse().ok()?;
    if n == 0 {
        return None;
    }

    let unit = unit_str.trim();

    match unit {
        "s" | "sec" | "secs" | "second" | "seconds" => {
            if n > 59 {
                return None;
            }
            Some(format!("*/{} * * * * *", n))
        }
        "m" | "min" | "mins" | "minute" | "minutes" => {
            if n > 59 {
                return None;
            }
            Some(format!("0 */{} * * * *", n))
        }
        "h" | "hr" | "hrs" | "hour" | "hours" => {
            if n > 23 {
                return None;
            }
            Some(format!("0 0 */{} * * *", n))
        }
        _ => None,
    }
}

/// Update the schedule for a workflow in `config.toml`.
///
/// Reads the file, finds the `[[workflows]]` section with matching name,
/// and updates the `schedule` value.  Best-effort — if parsing fails,
/// the file is left unchanged.
fn update_config_toml_schedule(project_root: &std::path::Path, workflow_name: &str, new_schedule: &str) {
    let config_path = project_root.join("config.toml");
    let content = match std::fs::read_to_string(&config_path) {
        Ok(c) => c,
        Err(_) => return,
    };

    // Strategy: find the [[workflows]] block where name = "workflow_name",
    // then find the `schedule = "..."` line within that block and replace it.
    let mut result = String::new();
    let mut in_target_block = false;
    let mut schedule_replaced = false;

    for line in content.lines() {
        // Detect start of a [[workflows]] block.
        if line.trim() == "[[workflows]]" {
            in_target_block = false; // Reset — next name= determines if it's our target.
        }

        // Detect the name field.
        if line.trim().starts_with("name") {
            if let Some(val) = extract_toml_string_value(line) {
                in_target_block = val == workflow_name;
            }
        }

        // Replace the schedule line in the target block.
        if in_target_block && !schedule_replaced && line.trim().starts_with("schedule") {
            result.push_str(&format!("schedule = \"{}\"", new_schedule));
            result.push('\n');
            schedule_replaced = true;
            continue;
        }

        result.push_str(line);
        result.push('\n');
    }

    if schedule_replaced {
        let _ = std::fs::write(&config_path, result);
    }
}

/// Extract a quoted string value from a TOML `key = "value"` line.
fn extract_toml_string_value(line: &str) -> Option<String> {
    let after_eq = line.split('=').nth(1)?.trim();
    if after_eq.starts_with('"') && after_eq.ends_with('"') && after_eq.len() >= 2 {
        Some(after_eq[1..after_eq.len() - 1].to_string())
    } else {
        None
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
        terminal.draw(|f| ui::draw(f, &mut app))?;

        // Process all pending TUI events (non-blocking drain).
        while let Ok(tui_event) = rx.try_recv() {
            app.handle_tui_event(tui_event);
        }

        // Expire stale toasts.
        app.tick_toast();

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

        // Process pending failure cooldown clears.
        if !app.clear_requests.is_empty() {
            if let Some(ref root) = app.project_root {
                let store = crate::workflows::stores::FailureCooldownStore::new(root);
                for (wf_name, item_key) in app.clear_requests.drain(..) {
                    let _ = store.clear_failure(&wf_name, &item_key).await;
                }
            } else {
                app.clear_requests.clear();
            }
        }

        // Poll for terminal input events with a short timeout so we keep
        // re-rendering even when no keys are pressed (to show new log lines).
        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key_event) => {
                    app.handle_key(key_event.code, key_event.modifiers);
                }
                Event::Mouse(mouse_event) => {
                    app.handle_mouse(mouse_event);
                }
                _ => {}
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
    fn test_b_key_scrolls_up_one_page() {
        let mut app = App::new();
        app.layout.logs = ratatui::layout::Rect::new(27, 0, 80, 20);
        assert_eq!(app.log_scroll, 0);
        app.handle_key(KeyCode::Char('b'), KeyModifiers::empty());
        assert_eq!(app.log_scroll, 20); // one page = logs height
        app.handle_key(KeyCode::Char('b'), KeyModifiers::empty());
        assert_eq!(app.log_scroll, 40);
    }

    #[test]
    fn test_f_key_scrolls_down_one_page() {
        let mut app = App::new();
        app.layout.logs = ratatui::layout::Rect::new(27, 0, 80, 20);
        app.log_scroll = 40;
        app.handle_key(KeyCode::Char('f'), KeyModifiers::empty());
        assert_eq!(app.log_scroll, 20);
        app.handle_key(KeyCode::Char('f'), KeyModifiers::empty());
        assert_eq!(app.log_scroll, 0);
        // f at 0 stays at 0
        app.handle_key(KeyCode::Char('f'), KeyModifiers::empty());
        assert_eq!(app.log_scroll, 0);
    }

    #[test]
    fn test_shift_b_scrolls_to_beginning() {
        let mut app = app_with_workflow("p", WorkflowMode::Standard);
        for i in 0..50 {
            app.workflows[0].logs.push(LogLine {
                level: "info".into(),
                stage: "x".into(),
                message: format!("line {}", i),
            });
        }
        app.handle_key(KeyCode::Char('B'), KeyModifiers::empty());
        assert_eq!(app.log_scroll, 49); // logs.len() - 1
    }

    #[test]
    fn test_shift_f_scrolls_to_end() {
        let mut app = App::new();
        app.log_scroll = 999;
        app.handle_key(KeyCode::Char('F'), KeyModifiers::empty());
        assert_eq!(app.log_scroll, 0);
    }

    #[test]
    fn test_page_keys_still_work() {
        let mut app = App::new();
        app.layout.logs = ratatui::layout::Rect::new(27, 0, 80, 25);
        app.handle_key(KeyCode::PageUp, KeyModifiers::empty());
        assert_eq!(app.log_scroll, 25);
        app.handle_key(KeyCode::PageDown, KeyModifiers::empty());
        assert_eq!(app.log_scroll, 0);
        app.handle_key(KeyCode::Home, KeyModifiers::empty());
        assert_eq!(app.log_scroll, 0); // no workflows, no logs
        app.log_scroll = 50;
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

    // ── Mouse handling ─────────────────────────────────────────────────────

    fn make_mouse_event(kind: MouseEventKind, col: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column: col,
            row,
            modifiers: KeyModifiers::empty(),
        }
    }

    #[test]
    fn test_mouse_down_starts_selection() {
        let mut app = App::new();
        app.layout.logs = ratatui::layout::Rect::new(27, 0, 80, 20);

        app.handle_mouse(make_mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            30,
            5,
        ));

        assert!(app.selection.active);
        assert_eq!(app.selection.panel, Some(Panel::Logs));
        assert_eq!(app.selection.start_col, 30);
        assert_eq!(app.selection.start_row, 5);
    }

    #[test]
    fn test_mouse_drag_clamps_to_panel() {
        let mut app = App::new();
        app.layout.logs = ratatui::layout::Rect::new(27, 0, 80, 20);

        // Start selection in logs
        app.handle_mouse(make_mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            30,
            5,
        ));

        // Drag way past the panel boundary
        app.handle_mouse(make_mouse_event(
            MouseEventKind::Drag(MouseButton::Left),
            200,
            30,
        ));

        // Should be clamped to panel bounds
        assert!(app.selection.end_col <= 27 + 80 - 1);
        assert!(app.selection.end_row <= 0 + 20 - 1);
    }

    #[test]
    fn test_mouse_up_clears_active() {
        let mut app = App::new();
        app.layout.logs = ratatui::layout::Rect::new(27, 0, 80, 20);

        app.handle_mouse(make_mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            30,
            5,
        ));
        assert!(app.selection.active);

        app.handle_mouse(make_mouse_event(
            MouseEventKind::Up(MouseButton::Left),
            35,
            7,
        ));
        assert!(!app.selection.active);
    }

    #[test]
    fn test_mouse_click_in_sidebar() {
        let mut app = App::new();
        app.layout.sidebar = ratatui::layout::Rect::new(0, 0, 26, 30);
        app.layout.logs = ratatui::layout::Rect::new(27, 0, 80, 20);

        app.handle_mouse(make_mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            5,
            10,
        ));

        assert!(app.selection.active);
        assert_eq!(app.selection.panel, Some(Panel::Sidebar));
    }

    #[test]
    fn test_mouse_scroll_up_down() {
        let mut app = App::new();
        assert_eq!(app.log_scroll, 0);

        app.handle_mouse(make_mouse_event(MouseEventKind::ScrollUp, 30, 5));
        assert_eq!(app.log_scroll, 3);

        app.handle_mouse(make_mouse_event(MouseEventKind::ScrollUp, 30, 5));
        assert_eq!(app.log_scroll, 6);

        app.handle_mouse(make_mouse_event(MouseEventKind::ScrollDown, 30, 5));
        assert_eq!(app.log_scroll, 3);

        app.handle_mouse(make_mouse_event(MouseEventKind::ScrollDown, 30, 5));
        assert_eq!(app.log_scroll, 0);

        // Can't go below 0
        app.handle_mouse(make_mouse_event(MouseEventKind::ScrollDown, 30, 5));
        assert_eq!(app.log_scroll, 0);
    }

    #[test]
    fn test_panel_hit_test() {
        let layout = PanelLayout {
            sidebar: ratatui::layout::Rect::new(0, 0, 26, 30),
            logs: ratatui::layout::Rect::new(27, 0, 80, 27),
            info_bar: ratatui::layout::Rect::new(27, 28, 80, 2),
            info_panel: ratatui::layout::Rect::default(),
        };

        assert_eq!(layout.hit_test(0, 0), Panel::Sidebar);
        assert_eq!(layout.hit_test(25, 15), Panel::Sidebar);
        assert_eq!(layout.hit_test(27, 0), Panel::Logs);
        assert_eq!(layout.hit_test(50, 15), Panel::Logs);
        assert_eq!(layout.hit_test(30, 28), Panel::InfoBar);
        assert_eq!(layout.hit_test(26, 15), Panel::None); // the gap
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

    // ── parse_human_cron ──────────────────────────────────────────────────

    #[test]
    fn test_parse_human_cron_minutes() {
        assert_eq!(parse_human_cron("5m"), Some("0 */5 * * * *".into()));
        assert_eq!(parse_human_cron("15min"), Some("0 */15 * * * *".into()));
        assert_eq!(parse_human_cron("30 mins"), Some("0 */30 * * * *".into()));
        assert_eq!(parse_human_cron("1 minute"), Some("0 */1 * * * *".into()));
        assert_eq!(parse_human_cron("45minutes"), Some("0 */45 * * * *".into()));
    }

    #[test]
    fn test_parse_human_cron_hours() {
        assert_eq!(parse_human_cron("1h"), Some("0 0 */1 * * *".into()));
        assert_eq!(parse_human_cron("2hr"), Some("0 0 */2 * * *".into()));
        assert_eq!(parse_human_cron("2 hrs"), Some("0 0 */2 * * *".into()));
        assert_eq!(parse_human_cron("6 hours"), Some("0 0 */6 * * *".into()));
        assert_eq!(parse_human_cron("12HRS"), Some("0 0 */12 * * *".into()));
    }

    #[test]
    fn test_parse_human_cron_seconds() {
        assert_eq!(parse_human_cron("30s"), Some("*/30 * * * * *".into()));
        assert_eq!(parse_human_cron("10 sec"), Some("*/10 * * * * *".into()));
        assert_eq!(parse_human_cron("5seconds"), Some("*/5 * * * * *".into()));
    }

    #[test]
    fn test_parse_human_cron_case_insensitive() {
        assert_eq!(parse_human_cron("5M"), Some("0 */5 * * * *".into()));
        assert_eq!(parse_human_cron("2HR"), Some("0 0 */2 * * *".into()));
        assert_eq!(parse_human_cron("10Sec"), Some("*/10 * * * * *".into()));
    }

    #[test]
    fn test_parse_human_cron_invalid() {
        assert_eq!(parse_human_cron(""), None);
        assert_eq!(parse_human_cron("abc"), None);
        assert_eq!(parse_human_cron("0m"), None);
        assert_eq!(parse_human_cron("60m"), None); // >59 minutes
        assert_eq!(parse_human_cron("24h"), None); // >23 hours
        assert_eq!(parse_human_cron("60s"), None); // >59 seconds
        assert_eq!(parse_human_cron("5 days"), None);
    }

    #[test]
    fn test_looks_like_cron() {
        assert!(looks_like_cron("0 */5 * * * *"));
        assert!(looks_like_cron("0 0 * * * *"));
        assert!(looks_like_cron("*/10 * * * * *"));
        assert!(!looks_like_cron("5m"));
        assert!(!looks_like_cron("every 5 minutes"));
        assert!(!looks_like_cron("0 * * * *")); // 5 fields, not 6
    }

    // ── cron edit mode ────────────────────────────────────────────────────

    #[test]
    fn test_c_command_enters_cron_edit() {
        let mut app = app_with_workflow("p", WorkflowMode::Standard);
        app.workflows[0].schedule = "0 */5 * * * *".into();
        app.handle_key(KeyCode::Char(':'), KeyModifiers::empty());
        app.handle_key(KeyCode::Char('c'), KeyModifiers::empty());
        assert_eq!(app.input_mode, InputMode::CronEdit);
        assert_eq!(app.cron_input, "0 */5 * * * *");
    }

    #[test]
    fn test_cron_edit_typing() {
        let mut app = App::new();
        app.input_mode = InputMode::CronEdit;
        app.cron_input.clear();
        app.handle_key(KeyCode::Char('1'), KeyModifiers::empty());
        app.handle_key(KeyCode::Char('5'), KeyModifiers::empty());
        app.handle_key(KeyCode::Char('m'), KeyModifiers::empty());
        assert_eq!(app.cron_input, "15m");
    }

    #[test]
    fn test_cron_edit_backspace() {
        let mut app = App::new();
        app.input_mode = InputMode::CronEdit;
        app.cron_input = "15m".into();
        app.handle_key(KeyCode::Backspace, KeyModifiers::empty());
        assert_eq!(app.cron_input, "15");
    }

    #[test]
    fn test_cron_edit_esc_cancels() {
        let mut app = App::new();
        app.input_mode = InputMode::CronEdit;
        app.handle_key(KeyCode::Esc, KeyModifiers::empty());
        assert_eq!(app.input_mode, InputMode::Normal);
    }

    #[test]
    fn test_apply_cron_input_human() {
        let mut app = app_with_workflow("p", WorkflowMode::Standard);
        app.cron_input = "15m".into();
        app.apply_cron_input();
        assert_eq!(app.workflows[0].schedule, "0 */15 * * * *");
    }

    #[test]
    fn test_apply_cron_input_raw_cron() {
        let mut app = app_with_workflow("p", WorkflowMode::Standard);
        app.cron_input = "0 0 */2 * * *".into();
        app.apply_cron_input();
        assert_eq!(app.workflows[0].schedule, "0 0 */2 * * *");
    }

    #[test]
    fn test_apply_cron_input_invalid_shows_toast() {
        let mut app = app_with_workflow("p", WorkflowMode::Standard);
        let old_schedule = app.workflows[0].schedule.clone();
        app.cron_input = "invalid".into();
        app.apply_cron_input();
        // Schedule should not change.
        assert_eq!(app.workflows[0].schedule, old_schedule);
        // Toast should show error.
        assert!(app.toast.is_some());
        assert!(app.toast.as_ref().unwrap().message.contains("Invalid"));
    }

    // ── click to select ───────────────────────────────────────────────────

    #[test]
    fn test_click_sidebar_selects_workflow() {
        let mut app = App::new();
        for name in &["a", "b", "c"] {
            app.handle_tui_event(TuiEvent::WorkflowRegistered {
                name: (*name).into(),
                schedule: "* * * * * *".into(),
                mode: WorkflowMode::Standard,
                repo: None,
            });
        }
        app.layout.sidebar = ratatui::layout::Rect::new(0, 0, 26, 30);
        assert_eq!(app.selected, 0);

        // "a" starts at row 2 (after title + blank), has 2+4=6 lines
        // "b" starts at row 9 (6 + 1 separator + 2 header)
        // Click on row 9 should select "b"
        app.handle_mouse(make_mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            5,
            9,
        ));
        // Release immediately so we don't trigger clipboard
        app.handle_mouse(make_mouse_event(
            MouseEventKind::Up(MouseButton::Left),
            5,
            9,
        ));
        assert_eq!(app.selected, 1);
    }

    // ── extract_toml_string_value ─────────────────────────────────────────

    #[test]
    fn test_extract_toml_string_value() {
        assert_eq!(
            extract_toml_string_value(r#"name = "bug-autofix""#),
            Some("bug-autofix".into())
        );
        assert_eq!(
            extract_toml_string_value(r#"schedule = "0 */5 * * * *""#),
            Some("0 */5 * * * *".into())
        );
        assert_eq!(extract_toml_string_value("name = 42"), None);
        assert_eq!(extract_toml_string_value("# comment"), None);
    }

    // ── Info panel ────────────────────────────────────────────────────────

    #[test]
    fn test_i_key_toggles_info_panel() {
        let mut app = App::new();
        assert!(!app.info_panel_open);
        app.handle_key(KeyCode::Char('i'), KeyModifiers::empty());
        assert!(app.info_panel_open);
        app.handle_key(KeyCode::Char('i'), KeyModifiers::empty());
        assert!(!app.info_panel_open);
    }

    #[test]
    fn test_items_summary_event_updates_workflow_items() {
        let mut app = app_with_workflow("p", WorkflowMode::Issues);
        app.handle_tui_event(TuiEvent::ItemsSummary {
            workflow_name: "p".into(),
            items: vec![
                ItemSummary {
                    id: "#42".into(),
                    title: "Fix bug".into(),
                    url: "https://github.com/o/r/issues/42".into(),
                    status: ItemStatus::None,
                },
                ItemSummary {
                    id: "#43".into(),
                    title: "Add feature".into(),
                    url: "https://github.com/o/r/issues/43".into(),
                    status: ItemStatus::Success,
                },
            ],
        });
        assert_eq!(app.workflow_items.get("p").unwrap().len(), 2);
        assert_eq!(app.selected_items().len(), 2);
        assert_eq!(app.selected_items()[0].id, "#42");
    }

    #[test]
    fn test_run_started_sets_item_in_progress() {
        let mut app = app_with_workflow("p", WorkflowMode::Issues);
        app.handle_tui_event(TuiEvent::ItemsSummary {
            workflow_name: "p".into(),
            items: vec![ItemSummary {
                id: "#42".into(),
                title: "Fix".into(),
                url: "u".into(),
                status: ItemStatus::None,
            }],
        });
        app.handle_tui_event(TuiEvent::RunStarted {
            workflow_name: "p".into(),
            run_id: "r".into(),
            mode: WorkflowMode::Issues,
            item_label: Some("issue #42".into()),
        });
        assert_eq!(app.selected_items()[0].status, ItemStatus::InProgress);
    }

    #[test]
    fn test_run_completed_sets_item_success() {
        let mut app = app_with_workflow("p", WorkflowMode::Issues);
        app.handle_tui_event(TuiEvent::ItemsSummary {
            workflow_name: "p".into(),
            items: vec![ItemSummary {
                id: "#42".into(),
                title: "Fix".into(),
                url: "u".into(),
                status: ItemStatus::InProgress,
            }],
        });
        // First a RunStarted to set current_item_label
        app.handle_tui_event(TuiEvent::RunStarted {
            workflow_name: "p".into(),
            run_id: "r".into(),
            mode: WorkflowMode::Issues,
            item_label: Some("issue #42".into()),
        });
        app.handle_tui_event(TuiEvent::RunCompleted {
            workflow_name: "p".into(),
            run_id: "r".into(),
            success: true,
            skipped: false,
            error: None,
            pr_url: Some("url".into()),
        });
        assert_eq!(app.selected_items()[0].status, ItemStatus::Success);
    }

    #[test]
    fn test_run_completed_failure_sets_item_error() {
        let mut app = app_with_workflow("p", WorkflowMode::Issues);
        app.handle_tui_event(TuiEvent::ItemsSummary {
            workflow_name: "p".into(),
            items: vec![ItemSummary {
                id: "#42".into(),
                title: "Fix".into(),
                url: "u".into(),
                status: ItemStatus::None,
            }],
        });
        app.handle_tui_event(TuiEvent::RunStarted {
            workflow_name: "p".into(),
            run_id: "r".into(),
            mode: WorkflowMode::Issues,
            item_label: Some("issue #42".into()),
        });
        app.handle_tui_event(TuiEvent::RunCompleted {
            workflow_name: "p".into(),
            run_id: "r".into(),
            success: false,
            skipped: false,
            error: Some("fail".into()),
            pr_url: None,
        });
        assert_eq!(app.selected_items()[0].status, ItemStatus::Error);
    }

    #[test]
    fn test_selected_items_empty_when_no_data() {
        let app = app_with_workflow("p", WorkflowMode::Standard);
        assert!(app.selected_items().is_empty());
    }

    #[test]
    fn test_items_summary_replaces_previous() {
        let mut app = app_with_workflow("p", WorkflowMode::Issues);
        app.handle_tui_event(TuiEvent::ItemsSummary {
            workflow_name: "p".into(),
            items: vec![ItemSummary {
                id: "#1".into(),
                title: "old".into(),
                url: "u".into(),
                status: ItemStatus::None,
            }],
        });
        assert_eq!(app.selected_items().len(), 1);

        app.handle_tui_event(TuiEvent::ItemsSummary {
            workflow_name: "p".into(),
            items: vec![
                ItemSummary {
                    id: "#1".into(),
                    title: "new".into(),
                    url: "u".into(),
                    status: ItemStatus::Success,
                },
                ItemSummary {
                    id: "#2".into(),
                    title: "added".into(),
                    url: "u".into(),
                    status: ItemStatus::None,
                },
            ],
        });
        assert_eq!(app.selected_items().len(), 2);
        assert_eq!(app.selected_items()[0].title, "new");
    }

    // ── Context-based input routing ───────────────────────────────────────

    /// Helper: create an app with info panel open and items loaded.
    fn app_with_info_panel() -> App {
        let mut app = app_with_workflow("p", WorkflowMode::Issues);
        app.handle_tui_event(TuiEvent::ItemsSummary {
            workflow_name: "p".into(),
            items: vec![
                ItemSummary {
                    id: "#1".into(),
                    title: "First".into(),
                    url: "https://github.com/o/r/issues/1".into(),
                    status: ItemStatus::None,
                },
                ItemSummary {
                    id: "#2".into(),
                    title: "Second".into(),
                    url: "https://github.com/o/r/issues/2".into(),
                    status: ItemStatus::Error,
                },
                ItemSummary {
                    id: "#3".into(),
                    title: "Third".into(),
                    url: "https://github.com/o/r/issues/3".into(),
                    status: ItemStatus::Cooldown,
                },
            ],
        });
        app.info_panel_open = true;
        app.info_panel_selected = 0;
        app
    }

    #[test]
    fn test_up_down_navigates_info_panel_when_open() {
        let mut app = app_with_info_panel();
        assert_eq!(app.info_panel_selected, 0);
        app.handle_key(KeyCode::Down, KeyModifiers::empty());
        assert_eq!(app.info_panel_selected, 1);
        app.handle_key(KeyCode::Down, KeyModifiers::empty());
        assert_eq!(app.info_panel_selected, 2);
        // Can't go past last item.
        app.handle_key(KeyCode::Down, KeyModifiers::empty());
        assert_eq!(app.info_panel_selected, 2);
        app.handle_key(KeyCode::Up, KeyModifiers::empty());
        assert_eq!(app.info_panel_selected, 1);
        app.handle_key(KeyCode::Up, KeyModifiers::empty());
        assert_eq!(app.info_panel_selected, 0);
        // Can't go below 0.
        app.handle_key(KeyCode::Up, KeyModifiers::empty());
        assert_eq!(app.info_panel_selected, 0);
    }

    #[test]
    fn test_up_down_switches_workflow_when_panel_closed() {
        let mut app = App::new();
        for name in &["a", "b"] {
            app.handle_tui_event(TuiEvent::WorkflowRegistered {
                name: (*name).into(),
                schedule: "* * * * * *".into(),
                mode: WorkflowMode::Standard,
                repo: None,
            });
        }
        assert!(!app.info_panel_open);
        assert_eq!(app.selected, 0);
        app.handle_key(KeyCode::Down, KeyModifiers::empty());
        assert_eq!(app.selected, 1); // Workflow switched, not info panel.
    }

    #[test]
    fn test_info_panel_selected_resets_on_toggle() {
        let mut app = app_with_info_panel();
        app.info_panel_selected = 2;
        // Close and reopen.
        app.handle_key(KeyCode::Char('i'), KeyModifiers::empty());
        assert!(!app.info_panel_open);
        // Open again from normal mode.
        app.handle_key(KeyCode::Char('i'), KeyModifiers::empty());
        assert!(app.info_panel_open);
        assert_eq!(app.info_panel_selected, 0);
    }

    #[test]
    fn test_info_panel_selected_resets_on_items_summary() {
        let mut app = app_with_info_panel();
        app.info_panel_selected = 2;
        app.handle_tui_event(TuiEvent::ItemsSummary {
            workflow_name: "p".into(),
            items: vec![ItemSummary {
                id: "#99".into(),
                title: "New".into(),
                url: "u".into(),
                status: ItemStatus::None,
            }],
        });
        assert_eq!(app.info_panel_selected, 0);
    }

    #[test]
    fn test_c_clears_selected_errored_item() {
        let mut app = app_with_info_panel();
        app.info_panel_selected = 1; // #2 has Error status.
        assert_eq!(app.selected_items()[1].status, ItemStatus::Error);
        app.handle_key(KeyCode::Char('c'), KeyModifiers::empty());
        assert_eq!(app.selected_items()[1].status, ItemStatus::None);
        assert_eq!(app.clear_requests.len(), 1);
        assert_eq!(app.clear_requests[0], ("p".into(), "2".into()));
    }

    #[test]
    fn test_c_does_not_clear_non_error_item() {
        let mut app = app_with_info_panel();
        app.info_panel_selected = 0; // #1 has None status.
        app.handle_key(KeyCode::Char('c'), KeyModifiers::empty());
        // Should not clear — it's already None.
        assert!(app.clear_requests.is_empty());
    }

    #[test]
    fn test_shift_c_clears_all_errored() {
        let mut app = app_with_info_panel();
        app.handle_key(KeyCode::Char('C'), KeyModifiers::empty());
        // #2 (Error) and #3 (Cooldown) should both be cleared.
        assert_eq!(app.selected_items()[1].status, ItemStatus::None);
        assert_eq!(app.selected_items()[2].status, ItemStatus::None);
        assert_eq!(app.clear_requests.len(), 2);
    }

    #[test]
    fn test_c_does_nothing_when_panel_closed() {
        let mut app = app_with_workflow("p", WorkflowMode::Issues);
        assert!(!app.info_panel_open);
        // 'c' in Normal mode is unbound — should do nothing.
        app.handle_key(KeyCode::Char('c'), KeyModifiers::empty());
        assert!(app.clear_requests.is_empty());
    }

    #[test]
    fn test_enter_in_info_panel_shows_toast() {
        let mut app = app_with_info_panel();
        app.handle_key(KeyCode::Enter, KeyModifiers::empty());
        // Should show "Opened in browser" toast (browser open may fail
        // in test env but toast is set regardless).
        assert!(app.toast.is_some());
        assert!(app.toast.as_ref().unwrap().message.contains("Opened"));
    }

    #[test]
    fn test_esc_closes_info_panel() {
        let mut app = app_with_info_panel();
        assert!(app.info_panel_open);
        app.handle_key(KeyCode::Esc, KeyModifiers::empty());
        assert!(!app.info_panel_open);
    }

    #[test]
    fn test_colon_opens_menu_from_info_panel() {
        let mut app = app_with_info_panel();
        app.handle_key(KeyCode::Char(':'), KeyModifiers::empty());
        assert_eq!(app.input_mode, InputMode::Command);
        // Info panel should stay open.
        assert!(app.info_panel_open);
    }

    #[test]
    fn test_esc_from_command_returns_to_info_panel_context() {
        let mut app = app_with_info_panel();
        app.handle_key(KeyCode::Char(':'), KeyModifiers::empty());
        assert_eq!(app.input_mode, InputMode::Command);
        assert!(app.info_panel_open);
        // Esc from command menu returns to Normal, but info panel is still open.
        app.handle_key(KeyCode::Esc, KeyModifiers::empty());
        assert_eq!(app.input_mode, InputMode::Normal);
        assert!(app.info_panel_open);
        // Next key should be handled by info panel context.
        app.handle_key(KeyCode::Down, KeyModifiers::empty());
        assert_eq!(app.info_panel_selected, 1); // Info panel navigation, not workflow.
    }

    #[test]
    fn test_extract_item_key() {
        assert_eq!(extract_item_key("#42"), "42");
        assert_eq!(extract_item_key("PR #12050"), "pr-12050");
        assert_eq!(extract_item_key("ENG-42"), "ENG-42");
    }
}
