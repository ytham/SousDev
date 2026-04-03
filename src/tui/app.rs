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

use crate::workflows::cron_runner::{CronRunner, ScheduleUpdate, ScheduleUpdateSender};
use crate::tui::events::{ItemStatus, ItemSummary, WorkflowMode, TuiEvent, TuiEventSender};

/// Maximum log lines retained per workflow.  Oldest entries are evicted
/// when the cap is exceeded.
const MAX_LOG_LINES_PER_WORKFLOW: usize = 10_000;
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

/// Which left-side pane is currently active for keyboard input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeftPane {
    /// The Workflows pane (workflow selection, stage flowcharts).
    Workflows,
    /// The Info pane (compact item status list).
    Info,
}

/// A single log line stored per-workflow.
#[derive(Debug, Clone)]
pub struct LogLine {
    pub level: String,
    pub stage: String,
    pub message: String,
    /// The run ID this log line belongs to (for filtering by item).
    pub run_id: Option<String>,
    /// Item label (e.g. `"PR #12050"`, `"issue #42"`) stamped at creation
    /// time for color-coding concurrent runs.  `None` for log lines not
    /// tied to a specific item.
    pub item_label: Option<String>,
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

/// A structured log entry for pretty mode rendering.
#[derive(Debug, Clone)]
pub struct LogEntry {
    /// What kind of entry this is.
    pub kind: LogEntryKind,
    /// The raw log lines that make up this entry.
    pub lines: Vec<LogLine>,
    /// Whether the user has expanded this entry.
    pub expanded: bool,
}

impl LogEntry {
    /// Number of visible screen rows this entry occupies.
    pub fn row_count(&self) -> usize {
        match self.kind {
            LogEntryKind::Thought => {
                // Count total visual lines (LogLines + embedded newlines).
                let total_visual: usize = self
                    .lines
                    .iter()
                    .map(|l| l.message.split('\n').count())
                    .sum();
                if total_visual <= 1 {
                    1 // Truly single-line — always fully shown.
                } else if self.expanded {
                    total_visual
                } else {
                    2 // First line + "[…] N more lines" indicator.
                }
            }
            LogEntryKind::ToolCall => {
                if self.expanded {
                    self.lines.len() // tool + result lines
                } else {
                    1 // just the tool line
                }
            }
            LogEntryKind::ConsolidatedTools => {
                if self.expanded {
                    // Each tool call shows 1 line (result still hidden).
                    let tool_count = self.lines.iter().filter(|l| l.stage == "tool").count();
                    tool_count
                } else {
                    2 // last tool line + "[+N] N consolidated"
                }
            }
            LogEntryKind::System => 1,
        }
    }
}

/// The kind of a log entry — determines rendering behavior.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogEntryKind {
    /// Agent thinking text (multi-line, shows first 4 lines collapsed).
    Thought,
    /// A single tool call, optionally with its result.
    ToolCall,
    /// A group of 3+ consecutive tool calls, consolidated.
    ConsolidatedTools,
    /// Stage transition, executor message, scheduler, etc.
    System,
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
    /// Structured log entries for pretty mode rendering.
    pub log_entries: Vec<LogEntry>,
    /// Cached total row count for log_entries (avoids recomputing every frame).
    pub log_entries_total_rows: usize,
    /// Whether this workflow is enabled (cron ticks are skipped when disabled).
    pub enabled: bool,
    /// The item label of the currently running item (for info expanded panel updates).
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
    Info,
    Logs,
    StatusBar,
    InfoExpanded,
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
    pub info: ratatui::layout::Rect,
    pub logs: ratatui::layout::Rect,
    pub status_bar: ratatui::layout::Rect,
    pub info_expanded: ratatui::layout::Rect,
}

impl PanelLayout {
    /// Determine which panel a terminal coordinate falls in.
    pub fn hit_test(&self, col: u16, row: u16) -> Panel {
        // Info expanded panel takes priority (it floats over everything).
        if self.info_expanded.width > 0 && contains(&self.info_expanded, col, row) {
            Panel::InfoExpanded
        } else if self.info.width > 0 && contains(&self.info, col, row) {
            Panel::Info
        } else if contains(&self.logs, col, row) {
            Panel::Logs
        } else if contains(&self.sidebar, col, row) {
            Panel::Sidebar
        } else if contains(&self.status_bar, col, row) {
            Panel::StatusBar
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
    /// Which left-side pane is active (Workflows or Info).
    pub active_left_pane: LeftPane,
    /// Selected item index in the Info pane (0 = "All logs", 1+ = items).
    pub info_selected: usize,
    /// Whether the info expanded panel is open.
    pub info_expanded_open: bool,
    /// Index of the highlighted item in the info expanded panel.
    pub info_expanded_selected: usize,
    /// Per-workflow item summaries for the info expanded panel.
    pub workflow_items: HashMap<String, Vec<ItemSummary>>,
    /// Pending failure cooldown clears (workflow_name, item_key).
    pub clear_requests: Vec<(String, String)>,
    /// Sender for schedule update commands to the cron runner.
    pub schedule_tx: Option<ScheduleUpdateSender>,
    /// Whether pretty log mode is enabled.
    pub pretty_logs: bool,
    /// Mouse down position for click vs drag detection.
    pub mouse_down_pos: Option<(u16, u16)>,
    /// Current log filter.  `None` = show all logs.  `Some(id)` = show only
    /// logs from runs associated with this item (e.g. `"#42"`, `"PR #120"`).
    pub log_filter: Option<String>,
    /// Map of run_id → item_label for associating logs with items.
    pub run_id_to_item: HashMap<String, String>,
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
            active_left_pane: LeftPane::Workflows,
            info_selected: 0,
            info_expanded_open: false,
            info_expanded_selected: 0,
            workflow_items: HashMap::new(),
            clear_requests: Vec::new(),
            schedule_tx: None,
            pretty_logs: true,
            mouse_down_pos: None,
            log_filter: None,
            run_id_to_item: HashMap::new(),
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

    /// Return the name of the currently selected workflow.
    pub fn selected_workflow_name(&self) -> Option<&str> {
        self.workflows.get(self.selected).map(|wf| wf.name.as_str())
    }

    /// Process a TUI event and update state accordingly.
    pub fn handle_tui_event(&mut self, event: TuiEvent) {
        let mut dirty_workflow: Option<String> = None;

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
                    log_entries: Vec::new(),
                    log_entries_total_rows: 0,
                    enabled,
                    current_item_label: None,
                });
            }

            TuiEvent::TickFired { .. } => {
                // Silently ignored — cron ticks are too noisy for the log pane.
            }

            TuiEvent::TickSkipped { .. } => {
                // Silently ignored — cron tick skips are too noisy for the log pane.
            }

            TuiEvent::RunStarted {
                workflow_name,
                run_id,
                item_label,
                ..
            } => {
                // Track run_id → item_label for log filtering.
                if let Some(ref label) = item_label {
                    self.run_id_to_item
                        .insert(run_id.clone(), label.clone());
                    self.update_item_status(&workflow_name, label, ItemStatus::InProgress);
                }

                let log_item_label = self.run_id_to_item.get(&run_id).cloned();
                if let Some(wf) = self.find_workflow_mut(&workflow_name) {
                    wf.status = WorkflowStatus::Running;
                    wf.run_id = Some(run_id.clone());
                    wf.current_item_label = item_label.clone();
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
                        run_id: Some(run_id),
                        item_label: log_item_label,
                    });
                }

                dirty_workflow = Some(workflow_name);

                // Auto-scroll to bottom when a run starts for the selected workflow.
                self.log_scroll = 0;
            }

            TuiEvent::StageStarted {
                workflow_name,
                run_id,
                stage_name,
            } => {
                let log_item_label = self.run_id_to_item.get(&run_id).cloned();
                if let Some(wf) = self.find_workflow_mut(&workflow_name) {
                    if let Some(idx) = wf.stages.iter().position(|s| *s == stage_name) {
                        wf.stage_statuses[idx] = StageStatus::Running;
                    }
                    wf.logs.push(LogLine {
                        level: "info".into(),
                        stage: stage_name.clone(),
                        message: format!("Stage started: {}", stage_name),
                        run_id: Some(run_id),
                        item_label: log_item_label,
                    });
                }
                dirty_workflow = Some(workflow_name);
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
                run_id,
                stage_name,
                error,
            } => {
                let log_item_label = self.run_id_to_item.get(&run_id).cloned();
                if let Some(wf) = self.find_workflow_mut(&workflow_name) {
                    if let Some(idx) = wf.stages.iter().position(|s| *s == stage_name) {
                        wf.stage_statuses[idx] = StageStatus::Failed;
                    }
                    wf.logs.push(LogLine {
                        level: "error".into(),
                        stage: stage_name,
                        message: error,
                        run_id: Some(run_id),
                        item_label: log_item_label,
                    });
                }
                dirty_workflow = Some(workflow_name);
            }

            TuiEvent::LogMessage {
                workflow_name,
                run_id,
                level,
                stage,
                message,
            } => {
                let log_item_label = if run_id.is_empty() {
                    None
                } else {
                    self.run_id_to_item.get(&run_id).cloned()
                };
                if let Some(wf) = self.find_workflow_mut(&workflow_name) {
                    let rid = if run_id.is_empty() { None } else { Some(run_id) };
                    wf.logs.push(LogLine {
                        level,
                        stage,
                        message,
                        run_id: rid,
                        item_label: log_item_label,
                    });
                }
                dirty_workflow = Some(workflow_name);
            }

            TuiEvent::RunCompleted {
                workflow_name,
                run_id,
                success,
                skipped,
                error,
                pr_url,
            } => {
                let item_label = self
                    .workflows
                    .iter()
                    .find(|w| w.name == workflow_name)
                    .and_then(|w| w.current_item_label.clone());

                let completed_run_id = if run_id.is_empty() { None } else { Some(run_id) };

                // Resolve item label BEFORE removing from map.
                let log_item_label = completed_run_id
                    .as_ref()
                    .and_then(|rid| self.run_id_to_item.get(rid))
                    .cloned();

                // Clean up the run_id → item_label mapping to prevent unbounded growth.
                if let Some(ref rid) = completed_run_id {
                    self.run_id_to_item.remove(rid);
                }

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

                    if skipped {
                        // For skipped runs, update the last log line in-place
                        // instead of appending a new one every tick.
                        let now = chrono::Local::now().format("%-I:%M:%S %p").to_string();
                        let msg = format!("No new items to process — last checked {}", now);
                        let is_last_skipped = wf
                            .logs
                            .last()
                            .map(|l| {
                                l.stage == "executor"
                                    && (l.message.starts_with("No new items")
                                        || l.message.starts_with("Run completed: skipped"))
                            })
                            .unwrap_or(false);
                        if is_last_skipped {
                            if let Some(last) = wf.logs.last_mut() {
                                last.message = msg;
                            }
                        } else {
                            wf.logs.push(LogLine {
                                level: "info".into(),
                                stage: "executor".into(),
                                message: msg,
                                run_id: completed_run_id.clone(),
                                item_label: log_item_label.clone(),
                            });
                        }
                    } else {
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
                            run_id: completed_run_id.clone(),
                            item_label: log_item_label.clone(),
                        });
                    }
                }

                dirty_workflow = Some(workflow_name.clone());

                // Update info expanded panel item status.
                if let Some(ref label) = item_label {
                    let wf_mode = self
                        .workflows
                        .iter()
                        .find(|w| w.name == workflow_name)
                        .map(|w| w.mode);
                    let new_status = if !success {
                        ItemStatus::Error
                    } else {
                        match wf_mode {
                            Some(WorkflowMode::PrReview) => ItemStatus::ReviewedApproved,
                            Some(WorkflowMode::PlanFirstIssues) => {
                                // Plan generation completes with PlanPending;
                                // plan execution completes with Success.
                                // The label is "plan #N" for generation, "exec #N" for execution.
                                if label.starts_with("plan ") {
                                    ItemStatus::PlanPending
                                } else {
                                    ItemStatus::Success
                                }
                            }
                            _ => ItemStatus::Success,
                        }
                    };
                    self.update_item_status(&workflow_name, label, new_status);
                }
            }

            TuiEvent::ItemsSummary {
                workflow_name,
                mut items,
            } => {
                // Reset selection when items refresh for the selected workflow.
                if self
                    .selected_workflow()
                    .map(|wf| wf.name == workflow_name)
                    .unwrap_or(false)
                {
                    self.info_expanded_selected = 0;
                }
                // Preserve InProgress status from the old list — a running agent
                // shouldn't have its status overwritten by a refresh.
                if let Some(old_items) = self.workflow_items.get(&workflow_name) {
                    for new_item in items.iter_mut() {
                        if let Some(old_item) = old_items.iter().find(|o| o.id == new_item.id) {
                            if old_item.status == ItemStatus::InProgress {
                                new_item.status = ItemStatus::InProgress;
                            }
                        }
                    }
                }
                self.workflow_items.insert(workflow_name, items);
            }

            TuiEvent::Shutdown => {
                self.should_quit = true;
            }
        }

        // Trim and rebuild structured log entries if any workflow's logs changed.
        if let Some(name) = dirty_workflow {
            if let Some(wf) = self.workflows.iter_mut().find(|w| w.name == name) {
                if wf.logs.len() > MAX_LOG_LINES_PER_WORKFLOW {
                    let drain_count = wf.logs.len() - MAX_LOG_LINES_PER_WORKFLOW;
                    wf.logs.drain(..drain_count);
                }
            }
            self.rebuild_entries_for(&name);
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
            InputMode::Normal if self.info_expanded_open => self.handle_key_info_expanded(key),
            InputMode::Normal if self.active_left_pane == LeftPane::Info => {
                self.handle_key_info(key)
            }
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
                    self.log_filter = None;
                    self.info_selected = 0;
                }
            }
            KeyCode::Down => {
                if self.selected + 1 < self.workflows.len() {
                    self.selected += 1;
                    self.log_scroll = 0;
                    self.log_filter = None;
                    self.info_selected = 0;
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
            KeyCode::Right => {
                self.active_left_pane = LeftPane::Info;
            }
            KeyCode::Char('i') => {
                self.info_expanded_open = true;
                self.info_expanded_selected = 0;
            }
            KeyCode::Char(':') => {
                self.input_mode = InputMode::Command;
            }
            _ => {}
        }
    }

    /// Info pane context: item navigation, filter, open URL, clear.
    fn handle_key_info(&mut self, key: KeyCode) {
        match key {
            KeyCode::Up => {
                self.info_selected = self.info_selected.saturating_sub(1);
            }
            KeyCode::Down => {
                let max = self.selected_items().len(); // items.len() = last valid index (0=All logs)
                if self.info_selected < max {
                    self.info_selected += 1;
                }
            }
            KeyCode::Enter => {
                self.set_log_filter_from_info();
            }
            KeyCode::Char('g') => {
                self.open_info_item();
            }
            KeyCode::Char('c') => {
                self.clear_info_item();
            }
            KeyCode::Char('C') => {
                self.clear_all_errored_items();
            }
            KeyCode::Left | KeyCode::Esc => {
                self.active_left_pane = LeftPane::Workflows;
            }
            KeyCode::Char('i') => {
                self.info_expanded_open = true;
                self.info_expanded_selected = 0;
            }
            KeyCode::Char(':') => {
                self.input_mode = InputMode::Command;
            }
            // Log scrolling still available in Info context.
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
            _ => {}
        }
    }

    /// Info expanded panel context: item navigation, clear, open URL.
    fn handle_key_info_expanded(&mut self, key: KeyCode) {
        match key {
            KeyCode::Up => {
                self.info_expanded_selected = self.info_expanded_selected.saturating_sub(1);
            }
            KeyCode::Down => {
                // +1 because index 0 is "All logs", then real items start at 1.
                let max = self.selected_items().len(); // items.len() = last valid index
                if self.info_expanded_selected < max {
                    self.info_expanded_selected += 1;
                }
            }
            KeyCode::Enter => {
                // Set log filter to the selected item (0 = All logs).
                self.set_log_filter_from_info_expanded();
            }
            KeyCode::Char('g') => {
                // Open URL in browser for the selected item.
                self.open_selected_info_item();
            }
            KeyCode::Char('c') => {
                self.clear_selected_item();
            }
            KeyCode::Char('C') => {
                self.clear_all_errored_items();
            }
            KeyCode::Char('i') | KeyCode::Esc => {
                self.info_expanded_open = false;
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
                    run_id: None,
                    item_label: None,
                });
            } else {
                wf.status = WorkflowStatus::Disabled;
                wf.logs.push(LogLine {
                    level: "warn".into(),
                    stage: "session".into(),
                    message: "Workflow disabled".into(),
                    run_id: None,
                    item_label: None,
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

                // Click outside a floating panel closes it and consumes
                // the click — no selection or sidebar action.
                if self.info_expanded_open && panel != Panel::InfoExpanded {
                    self.info_expanded_open = false;
                    return;
                }
                if self.input_mode == InputMode::Command && panel != Panel::None {
                    self.input_mode = InputMode::Normal;
                    return;
                }

                // Click in sidebar: select the clicked workflow.
                if panel == Panel::Sidebar {
                    if let Some(idx) = self.sidebar_row_to_workflow(event.row) {
                        if idx != self.selected {
                            self.selected = idx;
                            self.log_scroll = 0;
                            self.info_expanded_selected = 0;
                            self.log_filter = None;
                        }
                    }
                }

                // Click in Info pane: select item, activate pane, and show logs.
                // The first 2 rows are the workflow name header + spacer,
                // so clickable items start at row offset 2.
                if panel == Panel::Info {
                    self.active_left_pane = LeftPane::Info;
                    let info_rect = &self.layout.info;
                    let header_lines: u16 = 2; // workflow name + spacer
                    let raw_row = event.row.saturating_sub(info_rect.y);
                    if raw_row >= header_lines {
                        let item_row = (raw_row - header_lines) as usize;
                        let item_count = self.selected_items().len() + 1; // +1 for "All logs"
                        if item_row < item_count {
                            self.info_selected = item_row;
                            self.set_log_filter_from_info();
                        }
                    }
                }

                // Click in info expanded panel: select the clicked item.
                if panel == Panel::InfoExpanded {
                    self.select_info_expanded_item(event.row);
                }

                self.mouse_down_pos = Some((event.column, event.row));
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
                    let panel_rect = match self.selection.panel {
                        Some(Panel::Logs) => &self.layout.logs,
                        Some(Panel::Sidebar) => &self.layout.sidebar,
                        Some(Panel::Info) => &self.layout.info,
                        Some(Panel::StatusBar) => &self.layout.status_bar,
                        _ => return,
                    };
                    self.selection.end_col =
                        event.column.max(panel_rect.x).min(panel_rect.x + panel_rect.width - 1);
                    self.selection.end_row =
                        event.row.max(panel_rect.y).min(panel_rect.y + panel_rect.height - 1);
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let is_click = self
                    .mouse_down_pos
                    .map(|(dx, dy)| {
                        let dist = (event.column as i32 - dx as i32).unsigned_abs()
                            + (event.row as i32 - dy as i32).unsigned_abs();
                        dist < 3
                    })
                    .unwrap_or(false);

                if is_click && self.pretty_logs {
                    // Click: toggle expand on the entry at this row.
                    let panel = self.layout.hit_test(event.column, event.row);
                    if panel == Panel::Logs {
                        self.toggle_entry_at_row(event.row);
                    }
                } else if self.selection.active {
                    // Drag: copy selection.
                    self.copy_selection_to_clipboard();
                }

                self.selection.active = false;
                self.mouse_down_pos = None;
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

        let log_rect = &self.layout.logs;
        if log_rect.height == 0 {
            return;
        }

        let sel_top = self.selection.start_row.min(self.selection.end_row);
        let sel_bot = self.selection.start_row.max(self.selection.end_row);
        let first_log_row = log_rect.y;

        let mut selected_lines: Vec<String> = Vec::new();

        if self.pretty_logs && !wf.log_entries.is_empty() {
            // Pretty mode: walk entries, find which ones overlap the selection,
            // and output ALL their lines (fully expanded) regardless of
            // collapsed/visible state.
            let entries = &wf.log_entries;
            let total_rows = total_entry_rows(entries);
            let visible_height = log_rect.height as usize;
            let start_offset = if self.log_scroll == 0 {
                total_rows.saturating_sub(visible_height)
            } else {
                total_rows
                    .saturating_sub(visible_height)
                    .saturating_sub(self.log_scroll)
            };

            let mut accum = 0usize;
            for (i, entry) in entries.iter().enumerate() {
                let entry_rows = entry.row_count();
                let entry_screen_start = (accum as i64 - start_offset as i64 + first_log_row as i64) as i64;
                let entry_screen_end = entry_screen_start + entry_rows as i64 - 1;

                // Check if this entry overlaps the selection range.
                if entry_screen_end >= sel_top as i64 && entry_screen_start <= sel_bot as i64 {
                    // Output ALL lines from this entry (expanded content).
                    match entry.kind {
                        LogEntryKind::Thought => {
                            for log in &entry.lines {
                                // Split on embedded newlines for full content.
                                for sub_line in log.message.split('\n') {
                                    selected_lines.push(sub_line.to_string());
                                }
                            }
                        }
                        LogEntryKind::ToolCall | LogEntryKind::ConsolidatedTools => {
                            for log in &entry.lines {
                                let prefix = if log.stage == "tool" {
                                    "[tool] "
                                } else {
                                    "  └─ "
                                };
                                selected_lines.push(format!("{}{}", prefix, log.message));
                            }
                        }
                        LogEntryKind::System => {
                            for log in &entry.lines {
                                selected_lines.push(format!(
                                    "{:<5} [{}] {}",
                                    log.level.to_uppercase(),
                                    log.stage,
                                    log.message
                                ));
                            }
                        }
                    }
                    // Add blank line between entries of different kinds.
                    if i + 1 < entries.len() && entries[i].kind != entries[i + 1].kind {
                        selected_lines.push(String::new());
                    }
                }

                accum += entry_rows;
                if i + 1 < entries.len() && entries[i].kind != entries[i + 1].kind {
                    accum += 1;
                }
            }
        } else if !wf.logs.is_empty() {
            // Flat mode: simple 1:1 row-to-line mapping.
            let total = wf.logs.len();
            let visible_height = log_rect.height as usize;
            let start = if self.log_scroll == 0 {
                total.saturating_sub(visible_height)
            } else {
                total
                    .saturating_sub(visible_height)
                    .saturating_sub(self.log_scroll)
            };

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
        }

        if selected_lines.is_empty() {
            return;
        }

        let text = selected_lines.join("\n");
        if cli_clipboard::set_contents(text).is_ok() {
            self.show_toast("Copied to clipboard", Duration::from_secs(6));
        }
    }

    /// Clear the status of the currently selected info expanded panel item.
    ///
    /// Resets the item to `None` and queues a failure cooldown clear so
    /// the cron runner will retry it on the next tick.
    pub fn clear_selected_item(&mut self) {
        if self.info_expanded_selected == 0 {
            return; // "All logs" — nothing to clear.
        }
        let wf_name = match self.selected_workflow() {
            Some(wf) => wf.name.clone(),
            None => return,
        };
        let item_idx = self.info_expanded_selected - 1;
        if let Some(items) = self.workflow_items.get_mut(&wf_name) {
            if let Some(item) = items.get_mut(item_idx) {
                if item.status != ItemStatus::None && item.status != ItemStatus::InProgress {
                    let item_key = extract_item_key(&item.id);
                    item.status = ItemStatus::None;
                    self.clear_requests.push((wf_name, item_key));
                    self.show_toast("Cleared — will retry next tick", Duration::from_secs(3));
                }
            }
        }
    }

    /// Clear all non-pending items for the selected workflow.
    pub fn clear_all_errored_items(&mut self) {
        let wf_name = match self.selected_workflow() {
            Some(wf) => wf.name.clone(),
            None => return,
        };
        let mut count = 0usize;
        if let Some(items) = self.workflow_items.get_mut(&wf_name) {
            for item in items.iter_mut() {
                if item.status != ItemStatus::None && item.status != ItemStatus::InProgress {
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

    /// Set the log filter based on the Info pane selection.
    fn set_log_filter_from_info(&mut self) {
        let selected = self.info_selected;
        self.set_log_filter_by_index(selected);
    }

    /// Open the URL of the selected Info pane item.
    fn open_info_item(&mut self) {
        if self.info_selected == 0 {
            return;
        }
        let items = self.selected_items();
        let item_idx = self.info_selected - 1;
        if let Some(item) = items.get(item_idx) {
            let url = item.url.clone();
            if url.starts_with("https://") || url.starts_with("http://") {
                open_url_in_browser(&url);
                self.show_toast("Opened in browser", Duration::from_secs(3));
            }
        }
    }

    /// Clear the error status of the selected Info pane item.
    fn clear_info_item(&mut self) {
        if self.info_selected == 0 {
            return;
        }
        let wf_name = match self.selected_workflow() {
            Some(wf) => wf.name.clone(),
            None => return,
        };
        let item_idx = self.info_selected - 1;
        if let Some(items) = self.workflow_items.get_mut(&wf_name) {
            if let Some(item) = items.get_mut(item_idx) {
                // Allow clearing any non-None, non-InProgress status.
                if item.status != ItemStatus::None && item.status != ItemStatus::InProgress {
                    let item_key = extract_item_key(&item.id);
                    item.status = ItemStatus::None;
                    self.clear_requests.push((wf_name, item_key));
                    self.show_toast("Cleared — will retry next tick", Duration::from_secs(3));
                }
            }
        }
    }

    /// Shared log filter logic used by both Info and Info expanded panel.
    fn set_log_filter_by_index(&mut self, selected: usize) {
        if selected == 0 {
            if self.log_filter.is_some() {
                self.log_filter = None;
                self.log_scroll = 0;
                if let Some(name) = self.selected_workflow_name().map(|s| s.to_string()) {
                    self.rebuild_entries_for(&name);
                }
                self.show_toast("Showing all logs", Duration::from_secs(3));
            }
        } else {
            let items = self.selected_items();
            let item_idx = selected - 1;
            if let Some(item) = items.get(item_idx) {
                let item_id = item.id.clone();
                self.log_filter = Some(item_id.clone());
                self.log_scroll = 0;
                if let Some(name) = self.selected_workflow_name().map(|s| s.to_string()) {
                    self.rebuild_entries_for(&name);
                }
                self.show_toast(&format!("Filtering: {}", item_id), Duration::from_secs(3));
            }
        }
    }

    /// Set the log filter based on the info expanded panel selection.
    fn set_log_filter_from_info_expanded(&mut self) {
        let selected = self.info_expanded_selected;
        self.set_log_filter_by_index(selected);
    }

    /// Open the URL of the currently selected info expanded panel item in the browser.
    fn open_selected_info_item(&mut self) {
        if self.info_expanded_selected == 0 {
            return; // "All logs" has no URL.
        }
        let items = self.selected_items();
        let item_idx = self.info_expanded_selected - 1;
        if let Some(item) = items.get(item_idx) {
            let url = item.url.clone();
            // Only open URLs that look like real HTTP(S) links.
            if url.starts_with("https://") || url.starts_with("http://") {
                open_url_in_browser(&url);
                self.show_toast("Opened in browser", Duration::from_secs(3));
            }
        }
    }

    /// Select an info expanded panel item by terminal row (from mouse click).
    fn select_info_expanded_item(&mut self, row: u16) {
        let panel = &self.layout.info_expanded;
        if panel.height == 0 {
            return;
        }
        // Rows: 0 = title, 1 = separator, 2 = "All logs", 3+ = items.
        let item_row = row.saturating_sub(panel.y + 2) as usize;
        let item_count = self.selected_items().len() + 1; // +1 for "All logs"
        if item_row < item_count {
            self.info_expanded_selected = item_row;
            // Also set the log filter immediately (same as pressing Enter).
            self.set_log_filter_from_info_expanded();
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
                None => return, // Invalid input — silently cancel.
            }
        };

        if let Some(wf) = self.workflows.get_mut(self.selected) {
            let name = wf.name.clone();
            wf.schedule = cron_expr.clone();
            wf.logs.push(LogLine {
                level: "info".into(),
                stage: "config".into(),
                message: format!("Schedule updated to: {}", cron_expr),
                run_id: None,
                item_label: None,
            });

            // Persist to config.toml.
            if let Some(ref root) = self.project_root {
                update_config_toml_schedule(root, &name, &cron_expr);
            }

            // Send schedule update to the cron runner so it takes
            // effect immediately without restarting.
            if let Some(ref tx) = self.schedule_tx {
                let _ = tx.send(ScheduleUpdate {
                    workflow_name: name,
                    new_schedule: cron_expr.clone(),
                });
            }
        }

        self.show_toast("Schedule updated", Duration::from_secs(4));
    }

    /// Update the status of an item in the info expanded panel by matching the label.
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

    /// Return the items for the currently selected workflow's info expanded panel.
    pub fn selected_items(&self) -> &[ItemSummary] {
        let empty: &[ItemSummary] = &[];
        self.selected_workflow()
            .and_then(|wf| self.workflow_items.get(&wf.name))
            .map(|v| v.as_slice())
            .unwrap_or(empty)
    }

    /// Toggle expand/collapse on the log entry at a given screen row.
    fn toggle_entry_at_row(&mut self, row: u16) {
        let wf = match self.selected_workflow() {
            Some(wf) => wf,
            None => return,
        };

        if wf.log_entries.is_empty() {
            return;
        }

        let log_area = &self.layout.logs;
        if row < log_area.y || row >= log_area.y + log_area.height {
            return;
        }

        let entries = &wf.log_entries;

        // Calculate total rows including blank separators between different kinds
        // (must match the rendering logic exactly).
        let total_rows = total_entry_rows(entries);
        let visible_height = log_area.height as usize;

        let start_row_offset = if self.log_scroll == 0 {
            total_rows.saturating_sub(visible_height)
        } else {
            total_rows
                .saturating_sub(visible_height)
                .saturating_sub(self.log_scroll)
        };

        // Walk entries to find which one the clicked row belongs to.
        let click_offset = start_row_offset + (row - log_area.y) as usize;
        let mut accum = 0usize;
        let mut target_idx = None;

        for (i, entry) in entries.iter().enumerate() {
            let entry_rows = entry.row_count();
            if click_offset >= accum && click_offset < accum + entry_rows {
                // All expandable entries respond to clicks.
                match entry.kind {
                    LogEntryKind::Thought => {
                        // Expandable if multiple LogLines OR embedded newlines.
                        let has_content = entry.lines.len() > 1
                            || entry.lines.iter().any(|l| l.message.contains('\n'));
                        if has_content || entry.expanded {
                            target_idx = Some(i);
                        }
                    }
                    LogEntryKind::ToolCall if entry.lines.len() > 1 || entry.expanded => {
                        target_idx = Some(i);
                    }
                    LogEntryKind::ConsolidatedTools => {
                        target_idx = Some(i);
                    }
                    _ => {}
                }
                break;
            }
            accum += entry_rows;
            // Blank separator between entries of different kinds.
            if i + 1 < entries.len() && entries[i].kind != entries[i + 1].kind {
                accum += 1;
            }
        }

        if let Some(idx) = target_idx {
            let wf_name = self.workflows[self.selected].name.clone();
            if let Some(wf) = self.workflows.iter_mut().find(|w| w.name == wf_name) {
                wf.log_entries[idx].expanded = !wf.log_entries[idx].expanded;
                wf.log_entries_total_rows = total_entry_rows(&wf.log_entries);
            }
        }
    }

    /// Return the filtered logs for the selected workflow based on the
    /// current `log_filter`.  When `None`, returns all logs.
    ///
    /// Matching uses `LogLine.item_label` directly, so it works for both
    /// in-progress and completed runs (no dependency on `run_id_to_item`).
    pub fn filtered_logs_for(&self, wf: &WorkflowState) -> Vec<LogLine> {
        match &self.log_filter {
            None => wf.logs.clone(),
            Some(item_id) => {
                wf.logs
                    .iter()
                    .filter(|log| {
                        log.item_label
                            .as_ref()
                            .map(|label| {
                                label == item_id || label.ends_with(item_id.as_str())
                            })
                            .unwrap_or(false)
                    })
                    .cloned()
                    .collect()
            }
        }
    }

    /// Rebuild log entries for a workflow after its logs have changed.
    fn rebuild_entries_for(&mut self, workflow_name: &str) {
        if self.pretty_logs {
            // Determine which logs to use (filtered or all).
            let is_selected = self
                .workflows
                .get(self.selected)
                .map(|wf| wf.name == workflow_name)
                .unwrap_or(false);

            // Collect matching run_ids for filtering (avoids borrow issues).
            let matching_run_ids: Option<HashSet<String>> = if is_selected {
                self.log_filter.as_ref().map(|item_id| {
                    self.run_id_to_item
                        .iter()
                        .filter(|(_, label)| label.as_str() == item_id.as_str() || label.ends_with(item_id.as_str()))
                        .map(|(rid, _)| rid.clone())
                        .collect()
                })
            } else {
                None
            };

            if let Some(wf) = self.workflows.iter_mut().find(|w| w.name == workflow_name) {
                let filtered: Vec<LogLine>;
                let logs_to_use = match &matching_run_ids {
                    Some(run_ids) if !run_ids.is_empty() => {
                        filtered = wf
                            .logs
                            .iter()
                            .filter(|log| {
                                log.run_id
                                    .as_ref()
                                    .map(|rid| run_ids.contains(rid))
                                    .unwrap_or(false)
                            })
                            .cloned()
                            .collect();
                        &filtered
                    }
                    Some(_) => {
                        // Filter is active but no matching run_ids found.
                        filtered = Vec::new();
                        &filtered
                    }
                    None => &wf.logs,
                };

                let old_entries = &wf.log_entries;
                let mut new_entries = rebuild_log_entries(logs_to_use);

                // Preserve expanded state from old entries.  Match by index —
                // entries only grow (new ones are appended), so old[i] and
                // new[i] correspond to the same logical entry for all i < old.len().
                for (i, new_entry) in new_entries.iter_mut().enumerate() {
                    if let Some(old_entry) = old_entries.get(i) {
                        // If the entry kind matches and it was expanded, keep it expanded.
                        // Kind can change (e.g. 2 ToolCalls become ConsolidatedTools
                        // when a 3rd arrives) — only preserve if kind is the same.
                        if old_entry.kind == new_entry.kind && old_entry.expanded {
                            new_entry.expanded = true;
                        }
                    }
                }

                wf.log_entries_total_rows = total_entry_rows(&new_entries);
                wf.log_entries = new_entries;
            }
        }
    }

    fn find_workflow_mut(&mut self, name: &str) -> Option<&mut WorkflowState> {
        self.workflows.iter_mut().find(|w| w.name == name)
    }
}

/// Calculate the total screen rows for a list of log entries,
/// including blank separators between entries of different kinds.
/// Must match the rendering logic in `draw_logs_pretty` exactly.
pub fn total_entry_rows(entries: &[LogEntry]) -> usize {
    let mut total = 0usize;
    for (i, entry) in entries.iter().enumerate() {
        total += entry.row_count();
        if i + 1 < entries.len() && entries[i].kind != entries[i + 1].kind {
            total += 1; // blank separator
        }
    }
    total
}

/// Rebuild the structured `log_entries` from the flat `logs` list.
///
/// Groups consecutive same-type log lines into structured entries:
/// - Consecutive "thought"/"reflect" lines → single `Thought` entry
/// - "tool" + "result" pairs → `ToolCall` entries
/// - 3+ consecutive `ToolCall`s → `ConsolidatedTools` entry
/// - Everything else → `System` entries
pub fn rebuild_log_entries(logs: &[LogLine]) -> Vec<LogEntry> {
    if logs.is_empty() {
        return Vec::new();
    }

    // Phase 1: Build raw entries (tool+result pairs, thought groups, system).
    let mut raw_entries: Vec<LogEntry> = Vec::new();
    let mut i = 0;

    while i < logs.len() {
        let log = &logs[i];
        match log.stage.as_str() {
            "thought" | "reflect" => {
                // Collect consecutive thought/reflect lines into one Thought entry.
                let mut lines = vec![log.clone()];
                i += 1;
                while i < logs.len()
                    && (logs[i].stage == "thought" || logs[i].stage == "reflect")
                {
                    lines.push(logs[i].clone());
                    i += 1;
                }
                raw_entries.push(LogEntry {
                    kind: LogEntryKind::Thought,
                    lines,
                    expanded: false,
                });
            }
            "tool" => {
                // A tool line, possibly followed by a result line.
                let mut lines = vec![log.clone()];
                i += 1;
                if i < logs.len() && logs[i].stage == "result" {
                    lines.push(logs[i].clone());
                    i += 1;
                }
                raw_entries.push(LogEntry {
                    kind: LogEntryKind::ToolCall,
                    lines,
                    expanded: false,
                });
            }
            "result" => {
                // Orphaned result line (no preceding tool) — treat as system.
                raw_entries.push(LogEntry {
                    kind: LogEntryKind::System,
                    lines: vec![log.clone()],
                    expanded: false,
                });
                i += 1;
            }
            _ => {
                raw_entries.push(LogEntry {
                    kind: LogEntryKind::System,
                    lines: vec![log.clone()],
                    expanded: false,
                });
                i += 1;
            }
        }
    }

    // Phase 2: Consolidate 3+ consecutive ToolCall entries.
    let mut entries: Vec<LogEntry> = Vec::new();
    let mut j = 0;

    while j < raw_entries.len() {
        if raw_entries[j].kind == LogEntryKind::ToolCall {
            // Count consecutive ToolCall entries.
            let start = j;
            while j < raw_entries.len() && raw_entries[j].kind == LogEntryKind::ToolCall {
                j += 1;
            }
            let count = j - start;

            if count >= 3 {
                // Consolidate into a single ConsolidatedTools entry.
                let mut all_lines: Vec<LogLine> = Vec::new();
                for entry in &raw_entries[start..j] {
                    all_lines.extend(entry.lines.iter().cloned());
                }
                entries.push(LogEntry {
                    kind: LogEntryKind::ConsolidatedTools,
                    lines: all_lines,
                    expanded: false,
                });
            } else {
                // Keep individual ToolCall entries.
                for entry in raw_entries[start..j].iter() {
                    entries.push(entry.clone());
                }
            }
        } else {
            entries.push(raw_entries[j].clone());
            j += 1;
        }
    }

    entries
}

/// Open a URL in the system's default browser.
///
/// No-op during tests to prevent `cargo test` from opening real browser tabs.
fn open_url_in_browser(url: &str) {
    #[cfg(not(test))]
    {
        #[cfg(target_os = "macos")]
        let _ = std::process::Command::new("open").arg(url).spawn();
        #[cfg(target_os = "linux")]
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
        #[cfg(target_os = "windows")]
        let _ = std::process::Command::new("cmd").args(["/c", "start", url]).spawn();
    }
    #[cfg(test)]
    let _ = url; // Suppress unused variable warning in test builds.
}

/// Extract a store-compatible item key from an info expanded panel item ID.
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

/// Load historical item data from stores into the app's `workflow_items`.
///
/// This populates the info expanded panel on startup before the first cron tick
/// fetches fresh data.  Items are built from the handled-issues, reviewed-PRs,
/// and PR-response stores.  Failure cooldown items are overlaid with
/// `Error`/`Cooldown` status.
async fn load_initial_items(
    app: &mut App,
    project_root: &std::path::Path,
    workflow_infos: &[(String, WorkflowMode)],
) {
    use crate::workflows::stores::{
        FailureCooldownStore, HandledIssueStore, PrResponseStore, PrReviewStore,
    };

    let issue_store = HandledIssueStore::new(project_root);
    let review_store = PrReviewStore::new(project_root);
    let response_store = PrResponseStore::new(project_root);
    let failure_store = FailureCooldownStore::new(project_root);

    for (wf_name, wf_mode) in workflow_infos {
        let mut items: Vec<ItemSummary> = Vec::new();

        match wf_mode {
            WorkflowMode::Issues | WorkflowMode::PlanFirstIssues => {
                if let Ok(records) = issue_store.get_all_records(wf_name).await {
                    for (num_str, rec) in &records {
                        let item_key = num_str.clone();
                        let in_cooldown = failure_store
                            .is_in_cooldown(wf_name, &item_key)
                            .await
                            .unwrap_or(false);
                        let status = if in_cooldown {
                            ItemStatus::Cooldown
                        } else {
                            use crate::workflows::stores::plan_state;
                            match rec.state.as_deref() {
                                Some(plan_state::PLAN_POSTED) => ItemStatus::PlanPending,
                                Some(plan_state::PLAN_APPROVED) => ItemStatus::InProgress,
                                Some(plan_state::CODE_COMPLETE) => ItemStatus::Success,
                                _ => ItemStatus::Success,
                            }
                        };
                        items.push(ItemSummary {
                            id: format!("#{}", num_str),
                            title: rec.issue_title.clone(),
                            url: rec.issue_url.clone(),
                            status,
                    comment_count: 0,
                        });
                    }
                }
            }
            WorkflowMode::PrReview => {
                if let Ok(records) = review_store.get_all_records(wf_name).await {
                    for rec in records.values() {
                        let pr_key = format!("pr-{}", rec.pr_number);
                        let in_cooldown = failure_store
                            .is_in_cooldown(wf_name, &pr_key)
                            .await
                            .unwrap_or(false);
                        let status = if in_cooldown {
                            ItemStatus::Cooldown
                        } else {
                            ItemStatus::ReviewedApproved
                        };
                        items.push(ItemSummary {
                            id: format!("PR #{}", rec.pr_number),
                            title: rec.pr_title.clone(),
                            url: rec.pr_url.clone(),
                            status,
                    comment_count: 0,
                        });
                    }
                }
            }
            WorkflowMode::PrResponse => {
                if let Ok(records) = response_store.get_all_records(wf_name).await {
                    for rec in records.values() {
                        let status = ItemStatus::Success;
                        items.push(ItemSummary {
                            id: format!("PR #{}", rec.pr_number),
                            title: String::new(),
                            url: rec.pr_url.clone(),
                            status,
                    comment_count: 0,
                        });
                    }
                }
            }
            _ => {}
        }

        if !items.is_empty() {
            items.sort_by(|a, b| a.id.cmp(&b.id));
            app.workflow_items.insert(wf_name.clone(), items);
        }
    }
}

/// Load the most recent log files for each workflow into the TUI log pane.
///
/// Reads `output/logs/<workflow_name>/` and loads all log files, populating
/// `wf.logs` so the log pane has content on startup before the first cron tick.
async fn load_previous_logs(
    app: &mut App,
    project_root: &std::path::Path,
    workflow_infos: &[(String, WorkflowMode)],
) {
    for (wf_name, _) in workflow_infos {
        let log_dir = project_root.join("output").join("logs").join(wf_name);
        if !log_dir.exists() {
            continue;
        }

        // Collect all log files sorted by modification time (most recent last).
        let mut log_files: Vec<std::path::PathBuf> = Vec::new();
        if let Ok(mut entries) = tokio::fs::read_dir(&log_dir).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let path = entry.path();
                if path.extension().map(|e| e == "json").unwrap_or(false) {
                    log_files.push(path);
                }
            }
        }
        log_files.sort();

        // Load the most recent log files (up to 5 to avoid loading too much).
        let recent = if log_files.len() > 5 {
            &log_files[log_files.len() - 5..]
        } else {
            &log_files
        };

        let wf = match app.workflows.iter_mut().find(|w| w.name == *wf_name) {
            Some(wf) => wf,
            None => continue,
        };

        for log_path in recent {
            if let Ok(content) = tokio::fs::read_to_string(log_path).await {
                if let Ok(log_file) = serde_json::from_str::<crate::workflows::workflow_log::LogFile>(&content) {
                    // Extract the run_id from the header.
                    let run_id = log_file.header.run_id.clone();

                    // Add a separator line showing which log file this is from.
                    let filename = log_path
                        .file_stem()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_else(|| "unknown".into());
                    wf.logs.push(LogLine {
                        level: "info".into(),
                        stage: "history".into(),
                        message: format!("── {} ({}) ──", filename, log_file.header.status),
                        run_id: Some(run_id.clone()),
                        item_label: None,
                    });

                    // Add each log entry.
                    for entry in &log_file.entries {
                        wf.logs.push(LogLine {
                            level: entry.level.clone(),
                            stage: entry.stage.clone(),
                            message: entry.message.clone(),
                            run_id: Some(run_id.clone()),
                            item_label: None,
                        });
                    }
                }
            }
        }

        // Rebuild log entries for pretty mode.
        if app.pretty_logs && !wf.logs.is_empty() {
            wf.log_entries = rebuild_log_entries(&wf.logs);
            wf.log_entries_total_rows = total_entry_rows(&wf.log_entries);
        }
    }
}

/// Fetch current items from GitHub/Linear for each workflow and populate
/// the Info pane.  Runs once on startup so the Info pane has up-to-date
/// data before the first cron tick.
/// Fetch current items from GitHub/Linear for each workflow and send
/// `ItemsSummary` events via the TUI channel.
///
/// Designed to run as a background task so the TUI renders immediately.
/// Uses `TuiEventSender` to deliver results instead of mutating `App` directly.
async fn refresh_info_from_remote(
    config: crate::types::config::HarnessConfig,
    project_root: std::path::PathBuf,
    tui_tx: TuiEventSender,
) {
    use crate::tui::events::{ItemStatus, ItemSummary};
    use crate::workflows::github_issues::{fetch_github_issues, repo_to_gh_identifier, FetchIssuesOptions};
    use crate::workflows::github_prs::{fetch_github_prs, FetchPRsOptions};
    use crate::workflows::stores::{FailureCooldownStore, HandledIssueStore, PrReviewStore};

    let issue_store = HandledIssueStore::new(&project_root);
    let review_store = PrReviewStore::new(&project_root);
    let failure_store = FailureCooldownStore::new(&project_root);

    for wf_config in &config.workflows {
        let wf_name = &wf_config.name;

        // ── Issues mode ──────────────────────────────────────────────────
        if let Some(ref issues_cfg) = wf_config.github_issues {
            let gh_repo = issues_cfg
                .repo
                .clone()
                .or_else(|| repo_to_gh_identifier(config.target_repo.as_deref()));

            let items = fetch_github_issues(&FetchIssuesOptions {
                repo: gh_repo,
                assignees: issues_cfg.assignees.clone(),
                labels: issues_cfg.labels.clone(),
                limit: issues_cfg.limit,
            })
            .await
            .unwrap_or_default();

            let mut summaries: Vec<ItemSummary> = Vec::new();
            for item in &items {
                let is_handled = issue_store
                    .is_handled(wf_name, item.number)
                    .await
                    .unwrap_or(false);
                let in_cooldown = failure_store
                    .is_in_cooldown(wf_name, &item.number.to_string())
                    .await
                    .unwrap_or(false);
                let status = if in_cooldown {
                    ItemStatus::Cooldown
                } else if is_handled {
                    use crate::workflows::stores::plan_state;
                    let record = issue_store
                        .get_record(wf_name, item.number)
                        .await
                        .ok()
                        .flatten();
                    match record.as_ref().and_then(|r| r.state.as_deref()) {
                        Some(plan_state::PLAN_POSTED) => ItemStatus::PlanPending,
                        Some(plan_state::PLAN_APPROVED) => ItemStatus::InProgress,
                        Some(plan_state::CODE_COMPLETE) => ItemStatus::Success,
                        _ => ItemStatus::Success,
                    }
                } else {
                    ItemStatus::None
                };
                summaries.push(ItemSummary {
                    id: format!("#{}", item.number),
                    title: item.title.chars().take(50).collect(),
                    url: item.url.clone(),
                    status,
                    comment_count: 0,
                });
            }

            if !summaries.is_empty() {
                tui_tx.send(TuiEvent::ItemsSummary {
                    workflow_name: wf_name.clone(),
                    items: summaries,
                });
            }
            continue;
        }

        // ── PR review mode ───────────────────────────────────────────────
        if let Some(ref prs_cfg) = wf_config.github_prs {
            let gh_repo = prs_cfg
                .repo
                .clone()
                .or_else(|| repo_to_gh_identifier(config.target_repo.as_deref()));

            let all_prs = fetch_github_prs(&FetchPRsOptions {
                repo: gh_repo,
                search: prs_cfg.search.clone(),
                limit: prs_cfg.limit,
                raw_search: false,
            })
            .await
            .unwrap_or_default();

            let reviewer_login = crate::workflows::github_prs::detect_github_login()
                .await
                .unwrap_or_default();
            let prs: Vec<_> = all_prs
                .into_iter()
                .filter(|pr| {
                    pr.requested_reviewers.iter().any(|r| r == &reviewer_login)
                        || pr.assignees.iter().any(|a| a == &reviewer_login)
                })
                .collect();

            let mut summaries: Vec<ItemSummary> = Vec::new();
            for pr in &prs {
                let pr_key = format!("pr-{}", pr.number);
                let in_cooldown = failure_store
                    .is_in_cooldown(wf_name, &pr_key)
                    .await
                    .unwrap_or(false);
                let is_reviewed = review_store
                    .get_record(wf_name, pr.number)
                    .await
                    .ok()
                    .flatten()
                    .is_some();
                let pr_approved = pr.review_decision == "APPROVED";
                let status = if in_cooldown {
                    ItemStatus::Cooldown
                } else if pr_approved {
                    ItemStatus::Approved
                } else if is_reviewed {
                    ItemStatus::ReviewedApproved
                } else {
                    ItemStatus::None
                };
                summaries.push(ItemSummary {
                    id: format!("PR #{}", pr.number),
                    title: pr.title.chars().take(50).collect(),
                    url: pr.url.clone(),
                    status,
                    comment_count: 0,
                });
            }

            if !summaries.is_empty() {
                tui_tx.send(TuiEvent::ItemsSummary {
                    workflow_name: wf_name.clone(),
                    items: summaries,
                });
            }
            continue;
        }

        // ── PR response mode ─────────────────────────────────────────────
        if wf_config.github_pr_responses.is_some() {
            let gh_repo = wf_config
                .github_pr_responses
                .as_ref()
                .and_then(|c| c.repo.clone())
                .or_else(|| repo_to_gh_identifier(config.target_repo.as_deref()));

            let prs = fetch_github_prs(&FetchPRsOptions {
                repo: gh_repo,
                search: Some("author:@me".into()),
                limit: wf_config
                    .github_pr_responses
                    .as_ref()
                    .and_then(|c| c.limit),
                raw_search: true,
            })
            .await
            .unwrap_or_default();

            let summaries: Vec<ItemSummary> = prs
                .iter()
                .map(|pr| ItemSummary {
                    id: format!("PR #{}", pr.number),
                    title: pr.title.chars().take(50).collect(),
                    url: pr.url.clone(),
                    status: ItemStatus::None,
                    comment_count: 0,
                })
                .collect();

            if !summaries.is_empty() {
                tui_tx.send(TuiEvent::ItemsSummary {
                    workflow_name: wf_name.clone(),
                    items: summaries,
                });
            }
        }
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
        WorkflowMode::PlanFirstIssues => vec![
            "plan-generation".into(),
            "plan-approval".into(),
            "agent-loop".into(),
            "review-feedback-loop".into(),
            "pr-update".into(),
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

    // Capture workflow info before config is moved into the cron runner.
    let pretty_logs = config
        .logging
        .as_ref()
        .and_then(|l| l.pretty)
        .unwrap_or(true);

    let workflow_infos: Vec<(String, WorkflowMode)> = config
        .workflows
        .iter()
        .map(|wf| {
            let mode = if wf.github_pr_responses.is_some() {
                WorkflowMode::PrResponse
            } else if wf.github_prs.is_some() {
                WorkflowMode::PrReview
            } else if wf.github_issues.is_some() || wf.linear_issues.is_some() {
                WorkflowMode::Issues
            } else {
                WorkflowMode::Standard
            };
            (wf.name.clone(), mode)
        })
        .collect();

    // Clone workflow configs for the initial refresh (before config is moved).
    let config_for_refresh = config.clone();

    // Set up the TUI event channel.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<TuiEvent>();
    let tui_tx = TuiEventSender::new(tx);

    // Set up the schedule update channel.
    let (schedule_tx, schedule_rx) = tokio::sync::mpsc::unbounded_channel::<ScheduleUpdate>();

    // Spawn the cron runner in the background.
    let runner = CronRunner::new(config, no_workspace)
        .with_tui_sender(tui_tx.clone())
        .with_disabled_workflows(disabled_workflows.clone())
        .with_schedule_rx(schedule_rx);
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

    let mut app = App::with_session(sess, project_root.clone(), disabled_workflows);
    app.schedule_tx = Some(schedule_tx);
    app.pretty_logs = pretty_logs;

    // Load historical data from local stores (fast, disk-only).
    load_initial_items(&mut app, &project_root, &workflow_infos).await;
    load_previous_logs(&mut app, &project_root, &workflow_infos).await;

    // Fetch live data from GitHub in a background task so the TUI renders
    // immediately.  Results arrive via TuiEvent::ItemsSummary and update
    // the Info pane asynchronously.
    {
        let refresh_config = config_for_refresh.clone();
        let refresh_root = project_root.clone();
        let refresh_tx = tui_tx.clone();
        tokio::spawn(async move {
            refresh_info_from_remote(refresh_config, refresh_root, refresh_tx).await;
        });
    }

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

        // Process pending clear requests — clear from all relevant stores.
        if !app.clear_requests.is_empty() {
            if let Some(ref root) = app.project_root {
                let failure_store = crate::workflows::stores::FailureCooldownStore::new(root);
                let review_store = crate::workflows::stores::PrReviewStore::new(root);
                let issue_store = crate::workflows::stores::HandledIssueStore::new(root);
                for (wf_name, item_key) in app.clear_requests.drain(..) {
                    // Clear failure cooldown.
                    let _ = failure_store.clear_failure(&wf_name, &item_key).await;
                    // Clear PR review record (for pr-reviewer items like "pr-12073").
                    if let Some(pr_num) = item_key.strip_prefix("pr-") {
                        if let Ok(num) = pr_num.parse::<u64>() {
                            let _ = review_store.remove_record(&wf_name, num).await;
                        }
                    }
                    // Clear handled issue record (for issue items like "12345").
                    if let Ok(num) = item_key.parse::<u64>() {
                        let _ = issue_store.unmark_handled(&wf_name, num).await;
                    }
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
    #[test]
    fn test_tick_events_do_not_add_logs() {
        let mut app = app_with_workflow("p", WorkflowMode::Standard);
        app.handle_tui_event(TuiEvent::TickFired {
            workflow_name: "p".into(),
        });
        app.handle_tui_event(TuiEvent::TickSkipped {
            workflow_name: "p".into(),
            reason: "previous run still active".into(),
        });
        app.handle_tui_event(TuiEvent::TickFired {
            workflow_name: "nonexistent".into(),
        });
        // Tick events are silently ignored — no log lines.
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
        // run_started + (stage_started + log) * 4 stages + run_completed
        assert!(app.workflows[0].logs.len() >= 5);
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
                run_id: None,
                item_label: None,
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
        assert_eq!(stages_for_mode(WorkflowMode::PlanFirstIssues).len(), 5);
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
    fn test_stages_for_plan_first_issues_are_correct() {
        let stages = stages_for_mode(WorkflowMode::PlanFirstIssues);
        assert_eq!(stages[0], "plan-generation");
        assert_eq!(stages[1], "plan-approval");
        assert_eq!(stages[2], "agent-loop");
        assert_eq!(stages[3], "review-feedback-loop");
        assert_eq!(stages[4], "pr-update");
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
            info: ratatui::layout::Rect::new(27, 0, 24, 30),
            logs: ratatui::layout::Rect::new(52, 0, 60, 27),
            status_bar: ratatui::layout::Rect::new(52, 28, 60, 2),
            info_expanded: ratatui::layout::Rect::default(),
        };

        assert_eq!(layout.hit_test(0, 0), Panel::Sidebar);
        assert_eq!(layout.hit_test(27, 0), Panel::Info);
        assert_eq!(layout.hit_test(50, 15), Panel::Info);
        assert_eq!(layout.hit_test(25, 15), Panel::Sidebar);
        assert_eq!(layout.hit_test(27, 0), Panel::Info);
        assert_eq!(layout.hit_test(50, 15), Panel::Info);
        assert_eq!(layout.hit_test(52, 0), Panel::Logs);
        assert_eq!(layout.hit_test(80, 15), Panel::Logs);
        assert_eq!(layout.hit_test(55, 28), Panel::StatusBar);
        assert_eq!(layout.hit_test(26, 15), Panel::None); // the gap
        assert_eq!(layout.hit_test(51, 15), Panel::None); // gap between info and logs
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
        // Input starts empty so the user can type a fresh value.
        assert_eq!(app.cron_input, "");
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
    fn test_apply_cron_input_invalid_cancels_silently() {
        let mut app = app_with_workflow("p", WorkflowMode::Standard);
        let old_schedule = app.workflows[0].schedule.clone();
        app.cron_input = "invalid".into();
        app.apply_cron_input();
        // Schedule should not change.
        assert_eq!(app.workflows[0].schedule, old_schedule);
        // No toast — invalid input is a silent cancel.
        assert!(app.toast.is_none());
    }

    #[test]
    fn test_apply_cron_input_empty_cancels() {
        let mut app = app_with_workflow("p", WorkflowMode::Standard);
        let old_schedule = app.workflows[0].schedule.clone();
        app.cron_input = "".into();
        app.apply_cron_input();
        assert_eq!(app.workflows[0].schedule, old_schedule);
        assert!(app.toast.is_none());
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

    // ── Info expanded panel ────────────────────────────────────────────────────────

    #[test]
    fn test_i_key_toggles_info_expanded() {
        let mut app = App::new();
        assert!(!app.info_expanded_open);
        app.handle_key(KeyCode::Char('i'), KeyModifiers::empty());
        assert!(app.info_expanded_open);
        app.handle_key(KeyCode::Char('i'), KeyModifiers::empty());
        assert!(!app.info_expanded_open);
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
                    comment_count: 0,
                },
                ItemSummary {
                    id: "#43".into(),
                    title: "Add feature".into(),
                    url: "https://github.com/o/r/issues/43".into(),
                    status: ItemStatus::Success,
                    comment_count: 0,
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
                    comment_count: 0,
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
                    comment_count: 0,
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
                    comment_count: 0,
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
                    comment_count: 0,
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
                    comment_count: 0,
                },
                ItemSummary {
                    id: "#2".into(),
                    title: "added".into(),
                    url: "u".into(),
                    status: ItemStatus::None,
                    comment_count: 0,
                },
            ],
        });
        assert_eq!(app.selected_items().len(), 2);
        assert_eq!(app.selected_items()[0].title, "new");
    }

    // ── Context-based input routing ───────────────────────────────────────

    /// Helper: create an app with info expanded panel open and items loaded.
    fn app_with_info_expanded() -> App {
        let mut app = app_with_workflow("p", WorkflowMode::Issues);
        app.handle_tui_event(TuiEvent::ItemsSummary {
            workflow_name: "p".into(),
            items: vec![
                ItemSummary {
                    id: "#1".into(),
                    title: "First".into(),
                    url: "https://github.com/o/r/issues/1".into(),
                    status: ItemStatus::None,
                    comment_count: 0,
                },
                ItemSummary {
                    id: "#2".into(),
                    title: "Second".into(),
                    url: "https://github.com/o/r/issues/2".into(),
                    status: ItemStatus::Error,
                    comment_count: 0,
                },
                ItemSummary {
                    id: "#3".into(),
                    title: "Third".into(),
                    url: "https://github.com/o/r/issues/3".into(),
                    status: ItemStatus::Cooldown,
                    comment_count: 0,
                },
            ],
        });
        app.info_expanded_open = true;
        app.info_expanded_selected = 0;
        app
    }

    #[test]
    fn test_up_down_navigates_info_expanded_when_open() {
        let mut app = app_with_info_expanded();
        // 0 = "All logs", 1 = #1, 2 = #2, 3 = #3
        assert_eq!(app.info_expanded_selected, 0);
        app.handle_key(KeyCode::Down, KeyModifiers::empty());
        assert_eq!(app.info_expanded_selected, 1);
        app.handle_key(KeyCode::Down, KeyModifiers::empty());
        assert_eq!(app.info_expanded_selected, 2);
        app.handle_key(KeyCode::Down, KeyModifiers::empty());
        assert_eq!(app.info_expanded_selected, 3);
        // Can't go past last item.
        app.handle_key(KeyCode::Down, KeyModifiers::empty());
        assert_eq!(app.info_expanded_selected, 3);
        app.handle_key(KeyCode::Up, KeyModifiers::empty());
        assert_eq!(app.info_expanded_selected, 2);
        app.handle_key(KeyCode::Up, KeyModifiers::empty());
        assert_eq!(app.info_expanded_selected, 1);
        app.handle_key(KeyCode::Up, KeyModifiers::empty());
        assert_eq!(app.info_expanded_selected, 0);
        // Can't go below 0.
        app.handle_key(KeyCode::Up, KeyModifiers::empty());
        assert_eq!(app.info_expanded_selected, 0);
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
        assert!(!app.info_expanded_open);
        assert_eq!(app.selected, 0);
        app.handle_key(KeyCode::Down, KeyModifiers::empty());
        assert_eq!(app.selected, 1); // Workflow switched, not info expanded panel.
    }

    #[test]
    fn test_info_expanded_selected_resets_on_toggle() {
        let mut app = app_with_info_expanded();
        app.info_expanded_selected = 2;
        // Close and reopen.
        app.handle_key(KeyCode::Char('i'), KeyModifiers::empty());
        assert!(!app.info_expanded_open);
        // Open again from normal mode.
        app.handle_key(KeyCode::Char('i'), KeyModifiers::empty());
        assert!(app.info_expanded_open);
        assert_eq!(app.info_expanded_selected, 0);
    }

    #[test]
    fn test_info_expanded_selected_resets_on_items_summary() {
        let mut app = app_with_info_expanded();
        app.info_expanded_selected = 2;
        app.handle_tui_event(TuiEvent::ItemsSummary {
            workflow_name: "p".into(),
            items: vec![ItemSummary {
                id: "#99".into(),
                title: "New".into(),
                url: "u".into(),
                status: ItemStatus::None,
                    comment_count: 0,
            }],
        });
        assert_eq!(app.info_expanded_selected, 0);
    }

    #[test]
    fn test_c_clears_selected_errored_item() {
        let mut app = app_with_info_expanded();
        app.info_expanded_selected = 2; // index 2 = item #2 (offset 1 for "All logs")
        assert_eq!(app.selected_items()[1].status, ItemStatus::Error);
        app.handle_key(KeyCode::Char('c'), KeyModifiers::empty());
        assert_eq!(app.selected_items()[1].status, ItemStatus::None);
        assert_eq!(app.clear_requests.len(), 1);
        assert_eq!(app.clear_requests[0], ("p".into(), "2".into()));
    }

    #[test]
    fn test_c_does_not_clear_non_error_item() {
        let mut app = app_with_info_expanded();
        app.info_expanded_selected = 1; // index 1 = item #1 which has None status.
        app.handle_key(KeyCode::Char('c'), KeyModifiers::empty());
        // Should not clear — it's already None.
        assert!(app.clear_requests.is_empty());
    }

    #[test]
    fn test_shift_c_clears_all_errored() {
        let mut app = app_with_info_expanded();
        app.handle_key(KeyCode::Char('C'), KeyModifiers::empty());
        // #2 (Error) and #3 (Cooldown) should both be cleared.
        assert_eq!(app.selected_items()[1].status, ItemStatus::None);
        assert_eq!(app.selected_items()[2].status, ItemStatus::None);
        assert_eq!(app.clear_requests.len(), 2);
    }

    #[test]
    fn test_c_does_nothing_when_panel_closed() {
        let mut app = app_with_workflow("p", WorkflowMode::Issues);
        assert!(!app.info_expanded_open);
        // 'c' in Normal mode is unbound — should do nothing.
        app.handle_key(KeyCode::Char('c'), KeyModifiers::empty());
        assert!(app.clear_requests.is_empty());
    }

    #[test]
    fn test_enter_on_all_logs_clears_filter() {
        let mut app = app_with_info_expanded();
        app.log_filter = Some("#1".into());
        app.info_expanded_selected = 0; // "All logs"
        app.handle_key(KeyCode::Enter, KeyModifiers::empty());
        assert!(app.log_filter.is_none());
        assert!(app.toast.is_some());
        assert!(app.toast.as_ref().unwrap().message.contains("all logs"));
    }

    #[test]
    fn test_enter_on_item_sets_filter() {
        let mut app = app_with_info_expanded();
        app.info_expanded_selected = 1; // item #1
        app.handle_key(KeyCode::Enter, KeyModifiers::empty());
        assert_eq!(app.log_filter, Some("#1".into()));
        assert!(app.toast.is_some());
        assert!(app.toast.as_ref().unwrap().message.contains("#1"));
    }

    #[test]
    fn test_g_opens_url_in_info_expanded() {
        let mut app = app_with_info_expanded();
        app.info_expanded_selected = 1; // item #1
        app.handle_key(KeyCode::Char('g'), KeyModifiers::empty());
        assert!(app.toast.is_some());
        assert!(app.toast.as_ref().unwrap().message.contains("Opened"));
    }

    #[test]
    fn test_g_on_all_logs_does_nothing() {
        let mut app = app_with_info_expanded();
        app.info_expanded_selected = 0; // "All logs"
        app.handle_key(KeyCode::Char('g'), KeyModifiers::empty());
        assert!(app.toast.is_none());
    }

    #[test]
    fn test_esc_closes_info_expanded() {
        let mut app = app_with_info_expanded();
        assert!(app.info_expanded_open);
        app.handle_key(KeyCode::Esc, KeyModifiers::empty());
        assert!(!app.info_expanded_open);
    }

    #[test]
    fn test_colon_opens_menu_from_info_expanded() {
        let mut app = app_with_info_expanded();
        app.handle_key(KeyCode::Char(':'), KeyModifiers::empty());
        assert_eq!(app.input_mode, InputMode::Command);
        // Info expanded panel should stay open.
        assert!(app.info_expanded_open);
    }

    #[test]
    fn test_esc_from_command_returns_to_info_expanded_context() {
        let mut app = app_with_info_expanded();
        app.handle_key(KeyCode::Char(':'), KeyModifiers::empty());
        assert_eq!(app.input_mode, InputMode::Command);
        assert!(app.info_expanded_open);
        // Esc from command menu returns to Normal, but info expanded panel is still open.
        app.handle_key(KeyCode::Esc, KeyModifiers::empty());
        assert_eq!(app.input_mode, InputMode::Normal);
        assert!(app.info_expanded_open);
        // Next key should be handled by info expanded panel context.
        app.handle_key(KeyCode::Down, KeyModifiers::empty());
        assert_eq!(app.info_expanded_selected, 1); // Info expanded panel navigation, not workflow.
    }

    #[test]
    fn test_extract_item_key() {
        assert_eq!(extract_item_key("#42"), "42");
        assert_eq!(extract_item_key("PR #12050"), "pr-12050");
        assert_eq!(extract_item_key("ENG-42"), "ENG-42");
    }

    // ── Pretty log mode ───────────────────────────────────────────────────

    fn make_log(stage: &str, msg: &str) -> LogLine {
        LogLine {
            level: "info".into(),
            stage: stage.into(),
            message: msg.into(),
            run_id: None,
            item_label: None,
        }
    }

    #[test]
    fn test_rebuild_groups_consecutive_thoughts() {
        let logs = vec![
            make_log("thought", "line 1"),
            make_log("thought", "line 2"),
            make_log("thought", "line 3"),
        ];
        let entries = rebuild_log_entries(&logs);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, LogEntryKind::Thought);
        assert_eq!(entries[0].lines.len(), 3);
    }

    #[test]
    fn test_rebuild_pairs_tool_and_result() {
        let logs = vec![
            make_log("tool", "Read(\"src/main.rs\")"),
            make_log("result", "fn main() {}"),
        ];
        let entries = rebuild_log_entries(&logs);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, LogEntryKind::ToolCall);
        assert_eq!(entries[0].lines.len(), 2);
    }

    #[test]
    fn test_rebuild_consolidates_3_plus_tools() {
        let logs = vec![
            make_log("tool", "Read a"),
            make_log("result", "..."),
            make_log("tool", "Read b"),
            make_log("result", "..."),
            make_log("tool", "Read c"),
            make_log("result", "..."),
        ];
        let entries = rebuild_log_entries(&logs);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, LogEntryKind::ConsolidatedTools);
        assert_eq!(entries[0].lines.len(), 6); // 3 tool + 3 result
    }

    #[test]
    fn test_rebuild_no_consolidation_under_3() {
        let logs = vec![
            make_log("tool", "Read a"),
            make_log("result", "..."),
            make_log("tool", "Read b"),
            make_log("result", "..."),
        ];
        let entries = rebuild_log_entries(&logs);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].kind, LogEntryKind::ToolCall);
        assert_eq!(entries[1].kind, LogEntryKind::ToolCall);
    }

    #[test]
    fn test_rebuild_system_lines_stay_separate() {
        let logs = vec![
            make_log("executor", "Run started"),
            make_log("agent-loop", "Stage started"),
        ];
        let entries = rebuild_log_entries(&logs);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].kind, LogEntryKind::System);
        assert_eq!(entries[1].kind, LogEntryKind::System);
    }

    #[test]
    fn test_rebuild_mixed_sequence() {
        let logs = vec![
            make_log("executor", "Run started"),
            make_log("thought", "thinking..."),
            make_log("tool", "Read a"),
            make_log("result", "..."),
            make_log("tool", "Read b"),
            make_log("result", "..."),
            make_log("tool", "Read c"),
            make_log("result", "..."),
            make_log("thought", "done thinking"),
        ];
        let entries = rebuild_log_entries(&logs);
        assert_eq!(entries.len(), 4); // system, thought, consolidated, thought
        assert_eq!(entries[0].kind, LogEntryKind::System);
        assert_eq!(entries[1].kind, LogEntryKind::Thought);
        assert_eq!(entries[2].kind, LogEntryKind::ConsolidatedTools);
        assert_eq!(entries[3].kind, LogEntryKind::Thought);
    }

    #[test]
    fn test_entry_row_count_thought_collapsed() {
        let entry = LogEntry {
            kind: LogEntryKind::Thought,
            lines: (0..10).map(|i| make_log("thought", &format!("line {}", i))).collect(),
            expanded: false,
        };
        assert_eq!(entry.row_count(), 2); // first line + "[…] N more" indicator
    }

    #[test]
    fn test_entry_row_count_thought_single_line() {
        let entry = LogEntry {
            kind: LogEntryKind::Thought,
            lines: vec![make_log("thought", "short")],
            expanded: false,
        };
        assert_eq!(entry.row_count(), 1); // single line, always fully shown
    }

    #[test]
    fn test_entry_row_count_thought_two_lines_collapsed() {
        let entry = LogEntry {
            kind: LogEntryKind::Thought,
            lines: vec![make_log("thought", "a"), make_log("thought", "b")],
            expanded: false,
        };
        assert_eq!(entry.row_count(), 2); // first line + "[…] 1 more line"
    }

    #[test]
    fn test_entry_row_count_thought_expanded() {
        let entry = LogEntry {
            kind: LogEntryKind::Thought,
            lines: (0..10).map(|i| make_log("thought", &format!("line {}", i))).collect(),
            expanded: true,
        };
        assert_eq!(entry.row_count(), 10);
    }

    #[test]
    fn test_entry_row_count_tool_call_collapsed() {
        let entry = LogEntry {
            kind: LogEntryKind::ToolCall,
            lines: vec![make_log("tool", "Read"), make_log("result", "...")],
            expanded: false,
        };
        assert_eq!(entry.row_count(), 1);
    }

    #[test]
    fn test_entry_row_count_tool_call_expanded() {
        let entry = LogEntry {
            kind: LogEntryKind::ToolCall,
            lines: vec![make_log("tool", "Read"), make_log("result", "...")],
            expanded: true,
        };
        assert_eq!(entry.row_count(), 2);
    }

    #[test]
    fn test_entry_row_count_consolidated_collapsed() {
        let entry = LogEntry {
            kind: LogEntryKind::ConsolidatedTools,
            lines: vec![
                make_log("tool", "a"), make_log("result", "..."),
                make_log("tool", "b"), make_log("result", "..."),
                make_log("tool", "c"), make_log("result", "..."),
            ],
            expanded: false,
        };
        assert_eq!(entry.row_count(), 2); // last tool + count line
    }

    #[test]
    fn test_entry_row_count_consolidated_expanded() {
        let entry = LogEntry {
            kind: LogEntryKind::ConsolidatedTools,
            lines: vec![
                make_log("tool", "a"), make_log("result", "..."),
                make_log("tool", "b"), make_log("result", "..."),
                make_log("tool", "c"), make_log("result", "..."),
            ],
            expanded: true,
        };
        assert_eq!(entry.row_count(), 3); // 3 tool lines (results hidden)
    }

    #[test]
    fn test_rebuild_empty_logs() {
        let entries = rebuild_log_entries(&[]);
        assert!(entries.is_empty());
    }

    #[test]
    fn test_rebuild_orphaned_result() {
        let logs = vec![make_log("result", "orphaned")];
        let entries = rebuild_log_entries(&logs);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, LogEntryKind::System);
    }

    #[test]
    fn test_rebuild_preserves_expanded_state() {
        let mut app = app_with_workflow("p", WorkflowMode::Issues);
        app.pretty_logs = true;

        // Add 3 tool calls to trigger consolidation.
        for name in &["Read a", "Read b", "Read c"] {
            app.workflows[0].logs.push(make_log("tool", name));
            app.workflows[0].logs.push(make_log("result", "..."));
        }
        app.rebuild_entries_for("p");
        assert_eq!(app.workflows[0].log_entries.len(), 1);
        assert_eq!(app.workflows[0].log_entries[0].kind, LogEntryKind::ConsolidatedTools);
        assert!(!app.workflows[0].log_entries[0].expanded);

        // User expands the consolidated block.
        app.workflows[0].log_entries[0].expanded = true;

        // A 4th tool call arrives.
        app.workflows[0].logs.push(make_log("tool", "Read d"));
        app.workflows[0].logs.push(make_log("result", "..."));
        app.rebuild_entries_for("p");

        // The consolidated block should still be expanded.
        assert_eq!(app.workflows[0].log_entries.len(), 1);
        assert_eq!(app.workflows[0].log_entries[0].kind, LogEntryKind::ConsolidatedTools);
        assert!(app.workflows[0].log_entries[0].expanded);
        // And it should now have 4 tool calls.
        let tool_count = app.workflows[0].log_entries[0]
            .lines
            .iter()
            .filter(|l| l.stage == "tool")
            .count();
        assert_eq!(tool_count, 4);
    }

    #[test]
    fn test_rebuild_preserves_expanded_thought() {
        let mut app = app_with_workflow("p", WorkflowMode::Issues);
        app.pretty_logs = true;

        // Add a long thought block.
        for i in 0..8 {
            app.workflows[0].logs.push(make_log("thought", &format!("line {}", i)));
        }
        app.rebuild_entries_for("p");
        assert_eq!(app.workflows[0].log_entries[0].kind, LogEntryKind::Thought);

        // User expands it.
        app.workflows[0].log_entries[0].expanded = true;

        // A new system log arrives (different kind, so a new entry is added).
        app.workflows[0].logs.push(make_log("executor", "Run completed"));
        app.rebuild_entries_for("p");

        // The thought block should still be expanded.
        assert!(app.workflows[0].log_entries[0].expanded);
        assert_eq!(app.workflows[0].log_entries.len(), 2);
    }

    // ── Log filtering ─────────────────────────────────────────────────────

    #[test]
    fn test_run_id_stored_on_log_line() {
        let mut app = app_with_workflow("p", WorkflowMode::Issues);
        app.handle_tui_event(TuiEvent::LogMessage {
            workflow_name: "p".into(),
            run_id: "run-42".into(),
            level: "info".into(),
            stage: "tool".into(),
            message: "hello".into(),
        });
        assert_eq!(app.workflows[0].logs[0].run_id, Some("run-42".into()));
    }

    #[test]
    fn test_run_id_to_item_mapping() {
        let mut app = app_with_workflow("p", WorkflowMode::Issues);
        app.handle_tui_event(TuiEvent::RunStarted {
            workflow_name: "p".into(),
            run_id: "run-42".into(),
            mode: WorkflowMode::Issues,
            item_label: Some("issue #42".into()),
        });
        assert_eq!(
            app.run_id_to_item.get("run-42"),
            Some(&"issue #42".to_string())
        );
    }

    #[test]
    fn test_filter_clears_on_workflow_switch() {
        let mut app = App::new();
        for name in &["a", "b"] {
            app.handle_tui_event(TuiEvent::WorkflowRegistered {
                name: (*name).into(),
                schedule: "* * * * * *".into(),
                mode: WorkflowMode::Standard,
                repo: None,
            });
        }
        app.log_filter = Some("#42".into());
        app.handle_key(KeyCode::Down, KeyModifiers::empty());
        assert!(app.log_filter.is_none());
    }

    // ── Info pane ───────────────────────────────────────────────────────

    #[test]
    fn test_right_switches_to_info() {
        let mut app = App::new();
        assert_eq!(app.active_left_pane, LeftPane::Workflows);
        app.handle_key(KeyCode::Right, KeyModifiers::empty());
        assert_eq!(app.active_left_pane, LeftPane::Info);
    }

    #[test]
    fn test_left_switches_to_workflows() {
        let mut app = App::new();
        app.active_left_pane = LeftPane::Info;
        app.handle_key(KeyCode::Left, KeyModifiers::empty());
        assert_eq!(app.active_left_pane, LeftPane::Workflows);
    }

    #[test]
    fn test_esc_in_info_returns_to_workflows() {
        let mut app = App::new();
        app.active_left_pane = LeftPane::Info;
        app.handle_key(KeyCode::Esc, KeyModifiers::empty());
        assert_eq!(app.active_left_pane, LeftPane::Workflows);
    }

    #[test]
    fn test_up_down_in_info_pane_navigates_items() {
        let mut app = app_with_info_expanded(); // has 3 items
        app.info_expanded_open = false; // close floating panel so Glance gets input
        app.active_left_pane = LeftPane::Info;
        app.info_selected = 0;
        // 0=All logs, 1=#1, 2=#2, 3=#3
        app.handle_key(KeyCode::Down, KeyModifiers::empty());
        assert_eq!(app.info_selected, 1);
        app.handle_key(KeyCode::Down, KeyModifiers::empty());
        assert_eq!(app.info_selected, 2);
        app.handle_key(KeyCode::Down, KeyModifiers::empty());
        assert_eq!(app.info_selected, 3);
        app.handle_key(KeyCode::Down, KeyModifiers::empty());
        assert_eq!(app.info_selected, 3); // clamped
        app.handle_key(KeyCode::Up, KeyModifiers::empty());
        assert_eq!(app.info_selected, 2);
    }

    #[test]
    fn test_up_down_in_workflows_selects_workflow_not_glance() {
        let mut app = App::new();
        for name in &["a", "b"] {
            app.handle_tui_event(TuiEvent::WorkflowRegistered {
                name: (*name).into(),
                schedule: "* * * * * *".into(),
                mode: WorkflowMode::Standard,
                repo: None,
            });
        }
        assert_eq!(app.active_left_pane, LeftPane::Workflows);
        app.handle_key(KeyCode::Down, KeyModifiers::empty());
        assert_eq!(app.selected, 1); // workflow changed
    }

    #[test]
    fn test_enter_in_info_pane_sets_filter() {
        let mut app = app_with_info_expanded();
        app.info_expanded_open = false;
        app.active_left_pane = LeftPane::Info;
        app.info_selected = 1; // item #1
        app.handle_key(KeyCode::Enter, KeyModifiers::empty());
        assert_eq!(app.log_filter, Some("#1".into()));
    }

    #[test]
    fn test_info_resets_on_workflow_switch() {
        let mut app = App::new();
        for name in &["a", "b"] {
            app.handle_tui_event(TuiEvent::WorkflowRegistered {
                name: (*name).into(),
                schedule: "* * * * * *".into(),
                mode: WorkflowMode::Standard,
                repo: None,
            });
        }
        app.info_selected = 3;
        app.handle_key(KeyCode::Down, KeyModifiers::empty());
        assert_eq!(app.info_selected, 0); // reset
    }

    #[test]
    fn test_scroll_works_in_info_context() {
        let mut app = App::new();
        app.active_left_pane = LeftPane::Info;
        app.layout.logs = ratatui::layout::Rect::new(52, 0, 60, 20);
        app.handle_key(KeyCode::Char('b'), KeyModifiers::empty());
        assert_eq!(app.log_scroll, 20); // page up works
        app.handle_key(KeyCode::Char('f'), KeyModifiers::empty());
        assert_eq!(app.log_scroll, 0); // page down works
    }

    // ── Info pane key handlers ────────────────────────────────────────────

    #[test]
    fn test_g_opens_url_in_info_pane() {
        let mut app = app_with_info_expanded();
        app.info_expanded_open = false;
        app.active_left_pane = LeftPane::Info;
        app.info_selected = 1; // item #1
        app.handle_key(KeyCode::Char('g'), KeyModifiers::empty());
        assert!(app.toast.is_some());
        assert!(app.toast.as_ref().unwrap().message.contains("Opened"));
    }

    #[test]
    fn test_c_clears_errored_in_info_pane() {
        let mut app = app_with_info_expanded();
        app.info_expanded_open = false;
        app.active_left_pane = LeftPane::Info;
        app.info_selected = 2; // item #2 has Error status
        app.handle_key(KeyCode::Char('c'), KeyModifiers::empty());
        assert_eq!(app.selected_items()[1].status, ItemStatus::None);
        assert!(!app.clear_requests.is_empty());
    }

    #[test]
    fn test_shift_c_clears_all_from_info_pane() {
        let mut app = app_with_info_expanded();
        app.info_expanded_open = false;
        app.active_left_pane = LeftPane::Info;
        app.handle_key(KeyCode::Char('C'), KeyModifiers::empty());
        // #2 (Error) and #3 (Cooldown) should both be cleared.
        assert_eq!(app.selected_items()[1].status, ItemStatus::None);
        assert_eq!(app.selected_items()[2].status, ItemStatus::None);
        assert_eq!(app.clear_requests.len(), 2);
    }

    #[test]
    fn test_colon_from_info_pane_enters_command() {
        let mut app = App::new();
        app.active_left_pane = LeftPane::Info;
        app.handle_key(KeyCode::Char(':'), KeyModifiers::empty());
        assert_eq!(app.input_mode, InputMode::Command);
    }

    #[test]
    fn test_i_from_info_pane_opens_expanded() {
        let mut app = App::new();
        app.active_left_pane = LeftPane::Info;
        app.handle_key(KeyCode::Char('i'), KeyModifiers::empty());
        assert!(app.info_expanded_open);
    }

    // ── flatten_newlines / embedded newlines ──────────────────────────────

    #[test]
    fn test_thought_row_count_with_embedded_newlines() {
        let entry = LogEntry {
            kind: LogEntryKind::Thought,
            lines: vec![
                make_log("thought", "line1\nline2\nline3"),
                make_log("thought", "line4"),
            ],
            expanded: true,
        };
        // "line1\nline2\nline3" splits into 3, "line4" is 1 = total 4
        assert_eq!(entry.row_count(), 4);
    }

    #[test]
    fn test_thought_row_count_collapsed_ignores_embedded_newlines() {
        let entry = LogEntry {
            kind: LogEntryKind::Thought,
            lines: vec![
                make_log("thought", "line1\nline2\nline3"),
                make_log("thought", "line4"),
            ],
            expanded: false,
        };
        // Collapsed: first line + "[…]" = 2
        assert_eq!(entry.row_count(), 2);
    }

    // ── Click-outside-to-close ────────────────────────────────────────────

    #[test]
    fn test_click_outside_info_expanded_closes_it() {
        let mut app = App::new();
        app.info_expanded_open = true;
        app.layout.info_expanded = ratatui::layout::Rect::new(10, 1, 60, 30);
        app.layout.logs = ratatui::layout::Rect::new(52, 0, 60, 30);
        // Click in the logs area (outside info expanded).
        app.handle_mouse(make_mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            80,
            15,
        ));
        assert!(!app.info_expanded_open);
    }

    #[test]
    fn test_click_outside_command_menu_dismisses() {
        let mut app = App::new();
        app.input_mode = InputMode::Command;
        app.layout.sidebar = ratatui::layout::Rect::new(0, 0, 26, 30);
        // Click in the sidebar (not Panel::None).
        app.handle_mouse(make_mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            5,
            10,
        ));
        assert_eq!(app.input_mode, InputMode::Normal);
    }

    // ── Mouse click in Info pane ──────────────────────────────────────────

    #[test]
    fn test_mouse_click_in_info_pane_selects_item() {
        let mut app = app_with_info_expanded();
        app.info_expanded_open = false;
        app.active_left_pane = LeftPane::Workflows;
        app.layout.info = ratatui::layout::Rect::new(27, 0, 34, 30);
        // Row 0 = workflow name header, row 1 = spacer, row 2 = "All logs",
        // row 3 = first item, row 4 = second item.
        // Click row 4 (info_selected = 2 = second real item)
        app.handle_mouse(make_mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            30,
            4,
        ));
        assert_eq!(app.active_left_pane, LeftPane::Info);
        assert_eq!(app.info_selected, 2);
    }

    #[test]
    fn test_mouse_click_in_info_pane_sets_log_filter() {
        let mut app = app_with_info_expanded();
        app.info_expanded_open = false;
        app.active_left_pane = LeftPane::Workflows;
        app.layout.info = ratatui::layout::Rect::new(27, 0, 34, 30);
        // Click row 3 (info_selected = 1 = first real item "#1")
        app.handle_mouse(make_mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            30,
            3,
        ));
        assert_eq!(app.info_selected, 1);
        assert_eq!(app.log_filter, Some("#1".into()));
    }

    #[test]
    fn test_mouse_click_all_logs_clears_filter() {
        let mut app = app_with_info_expanded();
        app.info_expanded_open = false;
        app.log_filter = Some("#1".into());
        app.active_left_pane = LeftPane::Workflows;
        app.layout.info = ratatui::layout::Rect::new(27, 0, 34, 30);
        // Click row 2 ("All logs") — rows 0-1 are header+spacer.
        app.handle_mouse(make_mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            30,
            2,
        ));
        assert!(app.log_filter.is_none());
    }

    // ── Skipped run dedup ─────────────────────────────────────────────────

    #[test]
    fn test_skipped_run_dedup_updates_in_place() {
        let mut app = app_with_workflow("p", WorkflowMode::Issues);
        // First skipped run creates a new log line.
        app.handle_tui_event(TuiEvent::RunCompleted {
            workflow_name: "p".into(),
            run_id: "r1".into(),
            success: true,
            skipped: true,
            error: None,
            pr_url: None,
        });
        let log_count_after_first = app.workflows[0].logs.len();
        assert_eq!(log_count_after_first, 1);
        assert!(app.workflows[0].logs[0].message.contains("No new items"));

        // Second skipped run updates the same line (no new log added).
        app.handle_tui_event(TuiEvent::RunCompleted {
            workflow_name: "p".into(),
            run_id: "r2".into(),
            success: true,
            skipped: true,
            error: None,
            pr_url: None,
        });
        assert_eq!(app.workflows[0].logs.len(), log_count_after_first);
        assert!(app.workflows[0].logs[0].message.contains("No new items"));
    }

    #[test]
    fn test_skipped_after_non_skipped_creates_new_line() {
        let mut app = app_with_workflow("p", WorkflowMode::Issues);
        // Non-skipped run.
        app.handle_tui_event(TuiEvent::RunCompleted {
            workflow_name: "p".into(),
            run_id: "r1".into(),
            success: true,
            skipped: false,
            error: None,
            pr_url: Some("https://github.com/o/r/pull/1".into()),
        });
        let count_after_success = app.workflows[0].logs.len();

        // Skipped run creates a NEW line (previous was not skipped).
        app.handle_tui_event(TuiEvent::RunCompleted {
            workflow_name: "p".into(),
            run_id: "r2".into(),
            success: true,
            skipped: true,
            error: None,
            pr_url: None,
        });
        assert_eq!(app.workflows[0].logs.len(), count_after_success + 1);
    }

    // ── Single-line thought with embedded newlines ────────────────────────

    #[test]
    fn test_single_logline_thought_with_newlines_is_expandable() {
        let entry = LogEntry {
            kind: LogEntryKind::Thought,
            lines: vec![make_log("thought", "line1\nline2\nline3")],
            expanded: false,
        };
        // Single LogLine but 3 visual lines -> collapsed = 2 (first + indicator)
        assert_eq!(entry.row_count(), 2);
    }

    #[test]
    fn test_single_logline_thought_with_newlines_expanded() {
        let entry = LogEntry {
            kind: LogEntryKind::Thought,
            lines: vec![make_log("thought", "line1\nline2\nline3")],
            expanded: true,
        };
        assert_eq!(entry.row_count(), 3);
    }

    #[test]
    fn test_truly_single_line_thought_not_expandable() {
        let entry = LogEntry {
            kind: LogEntryKind::Thought,
            lines: vec![make_log("thought", "just one line")],
            expanded: false,
        };
        assert_eq!(entry.row_count(), 1);
    }
}
