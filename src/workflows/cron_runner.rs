use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_cron_scheduler::{Job, JobScheduler};
use uuid::Uuid;

use crate::providers::resolve_provider;
use crate::tools::registry::ToolRegistry;
use crate::tui::events::{TuiEvent, TuiEventSender, WorkflowMode};
use crate::types::config::HarnessConfig;
use crate::utils::logger::Logger;
use crate::workflows::executor::{resolve_system_prompt, ExecutorOptions, WorkflowExecutor};
use crate::workflows::stores::{RunStore, WorkflowResult};

/// Callback invoked after each workflow run completes (success or failure).
pub type RunCompleteCallback = Arc<dyn Fn(WorkflowResult) + Send + Sync>;

/// A command sent from the TUI to the cron runner to update a workflow's
/// schedule at runtime.
#[derive(Debug, Clone)]
pub struct ScheduleUpdate {
    /// Name of the workflow to reschedule.
    pub workflow_name: String,
    /// New 6-field cron expression.
    pub new_schedule: String,
}

/// Sender handle for schedule update commands.
pub type ScheduleUpdateSender = tokio::sync::mpsc::UnboundedSender<ScheduleUpdate>;

/// Schedules all configured workflows on their cron expressions, with a
/// per-workflow overlap guard that drops a tick if the previous run is still
/// active.
pub struct CronRunner {
    config: HarnessConfig,
    no_workspace: bool,
    on_run_complete: Option<RunCompleteCallback>,
    store: Arc<RunStore>,
    tui_tx: TuiEventSender,
    /// Workflows in this set are skipped on each cron tick.
    /// Shared with the TUI so toggles take effect immediately.
    disabled_workflows: Arc<Mutex<HashSet<String>>>,
    /// Receiver for schedule update commands from the TUI.
    schedule_rx: Option<tokio::sync::mpsc::UnboundedReceiver<ScheduleUpdate>>,
}

impl CronRunner {
    /// Create a new [`CronRunner`] from a fully-loaded harness config.
    pub fn new(config: HarnessConfig, no_workspace: bool) -> Self {
        let store_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self {
            config,
            no_workspace,
            on_run_complete: None,
            store: Arc::new(RunStore::new(&store_dir)),
            tui_tx: TuiEventSender::noop(),
            disabled_workflows: Arc::new(Mutex::new(HashSet::new())),
            schedule_rx: None,
        }
    }

    /// Attach a TUI event sender so the cron runner emits events for the TUI.
    pub fn with_tui_sender(mut self, tx: TuiEventSender) -> Self {
        self.tui_tx = tx;
        self
    }

    /// Attach a shared set of disabled workflow names.
    pub fn with_disabled_workflows(mut self, set: Arc<Mutex<HashSet<String>>>) -> Self {
        self.disabled_workflows = set;
        self
    }

    /// Attach a receiver for schedule update commands.
    pub fn with_schedule_rx(
        mut self,
        rx: tokio::sync::mpsc::UnboundedReceiver<ScheduleUpdate>,
    ) -> Self {
        self.schedule_rx = Some(rx);
        self
    }

    /// Register a callback to be invoked after every workflow run completes.
    pub fn on_run_complete(&mut self, callback: RunCompleteCallback) {
        self.on_run_complete = Some(callback);
    }

    /// Determine the [`WorkflowMode`] for a workflow config.
    fn workflow_mode(config: &crate::types::config::WorkflowConfig) -> WorkflowMode {
        if config.github_pr_responses.is_some() {
            WorkflowMode::PrResponse
        } else if config.github_prs.is_some() {
            WorkflowMode::PrReview
        } else if config.github_issues.is_some() || config.linear_issues.is_some() {
            WorkflowMode::Issues
        } else {
            WorkflowMode::Standard
        }
    }

    /// Start the scheduler.  Blocks until `Ctrl+C` is received, then performs
    /// a clean shutdown.
    pub async fn start(mut self) -> Result<()> {
        let mut sched = JobScheduler::new().await?;
        let logger = Logger::new("cron-runner");

        // Map of workflow name → (job UUID, per-workflow shared state).
        // Used to remove old jobs and create replacement jobs on schedule updates.
        let job_ids: Arc<Mutex<HashMap<String, Uuid>>> =
            Arc::new(Mutex::new(HashMap::new()));

        // Per-workflow shared state needed to rebuild jobs on reschedule.
        // Keyed by workflow name.
        let job_state: Arc<Mutex<HashMap<String, JobSharedState>>> =
            Arc::new(Mutex::new(HashMap::new()));

        for workflow_config in &self.config.workflows {
            let name = workflow_config.name.clone();
            let schedule = workflow_config.schedule.clone();

            let mode = Self::workflow_mode(workflow_config);
            let repo = workflow_config
                .github_issues
                .as_ref()
                .and_then(|c| c.repo.clone())
                .or_else(|| {
                    workflow_config
                        .github_prs
                        .as_ref()
                        .and_then(|c| c.repo.clone())
                })
                .or_else(|| {
                    workflow_config
                        .github_pr_responses
                        .as_ref()
                        .and_then(|c| c.repo.clone())
                })
                .or_else(|| self.config.target_repo.clone());

            self.tui_tx.send(TuiEvent::WorkflowRegistered {
                name: name.clone(),
                schedule: schedule.clone(),
                mode,
                repo,
            });

            let shared = JobSharedState {
                wf_config: workflow_config.clone(),
                harness_config: self.config.clone(),
                store: self.store.clone(),
                no_workspace: self.no_workspace,
                callback: self.on_run_complete.clone(),
                tui_tx: self.tui_tx.clone(),
                disabled: self.disabled_workflows.clone(),
                active: Arc::new(Mutex::new(false)),
            };

            logger.info(&format!(
                "Scheduling workflow \"{}\" with cron: {}",
                name, schedule
            ));

            let job = create_job(&schedule, &name, &shared)?;
            let uuid = sched.add(job).await?;

            job_ids.lock().await.insert(name.clone(), uuid);
            job_state.lock().await.insert(name, shared);
        }

        sched.start().await?;

        // Listen for schedule updates and Ctrl+C concurrently.
        let mut schedule_rx = self.schedule_rx.take();
        let tui_tx = self.tui_tx.clone();

        // Spawn a background task to handle schedule updates so it
        // doesn't block the main select loop.
        if let Some(mut rx) = schedule_rx.take() {
            let sched_clone = sched.clone();
            let job_ids = job_ids.clone();
            let job_state = job_state.clone();
            let tui_tx = tui_tx.clone();

            tokio::spawn(async move {
                while let Some(update) = rx.recv().await {
                    tui_tx.send(TuiEvent::LogMessage {
                        workflow_name: update.workflow_name.clone(),
                        run_id: String::new(),
                        level: "info".into(),
                        stage: "scheduler".into(),
                        message: format!(
                            "Rescheduling to: {}",
                            update.new_schedule
                        ),
                    });

                    // Remove the old job.
                    if let Some(old_uuid) =
                        job_ids.lock().await.remove(&update.workflow_name)
                    {
                        if let Err(e) = sched_clone.remove(&old_uuid).await {
                            tui_tx.send(TuiEvent::LogMessage {
                                workflow_name: update.workflow_name.clone(),
                                run_id: String::new(),
                                level: "error".into(),
                                stage: "scheduler".into(),
                                message: format!("Failed to remove old job: {}", e),
                            });
                        }
                    }

                    // Create and add a new job with the updated schedule.
                    let shared = job_state.lock().await.get(&update.workflow_name).cloned();
                    if let Some(shared) = shared {
                        match create_job(&update.new_schedule, &update.workflow_name, &shared) {
                            Ok(new_job) => {
                                match sched_clone.add(new_job).await {
                                    Ok(new_uuid) => {
                                        job_ids.lock().await.insert(
                                            update.workflow_name.clone(),
                                            new_uuid,
                                        );
                                        tui_tx.send(TuiEvent::LogMessage {
                                            workflow_name: update.workflow_name.clone(),
                                            run_id: String::new(),
                                            level: "info".into(),
                                            stage: "scheduler".into(),
                                            message: "Rescheduled successfully".into(),
                                        });
                                    }
                                    Err(e) => {
                                        tui_tx.send(TuiEvent::LogMessage {
                                            workflow_name: update.workflow_name.clone(),
                                            run_id: String::new(),
                                            level: "error".into(),
                                            stage: "scheduler".into(),
                                            message: format!(
                                                "Failed to add rescheduled job: {}",
                                                e
                                            ),
                                        });
                                    }
                                }
                            }
                            Err(e) => {
                                tui_tx.send(TuiEvent::LogMessage {
                                    workflow_name: update.workflow_name.clone(),
                                    run_id: String::new(),
                                    level: "error".into(),
                                    stage: "scheduler".into(),
                                    message: format!(
                                        "Failed to create job with schedule \"{}\": {}",
                                        update.new_schedule, e
                                    ),
                                });
                            }
                        }
                    }
                }
            });
        }

        // Block until Ctrl+C.
        tokio::signal::ctrl_c().await?;

        logger.info("Shutting down cron scheduler…");
        self.tui_tx.send(TuiEvent::Shutdown);
        sched.shutdown().await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Job creation helper
// ---------------------------------------------------------------------------

/// Shared state cloned into each job closure.  Stored per-workflow so that
/// jobs can be removed and re-created with a new schedule without losing
/// the closure's captured state.
#[derive(Clone)]
struct JobSharedState {
    wf_config: crate::types::config::WorkflowConfig,
    harness_config: HarnessConfig,
    store: Arc<RunStore>,
    no_workspace: bool,
    callback: Option<RunCompleteCallback>,
    tui_tx: TuiEventSender,
    disabled: Arc<Mutex<HashSet<String>>>,
    /// Per-workflow overlap guard: `true` while a run is active.
    active: Arc<Mutex<bool>>,
}

/// Create a cron `Job` from a schedule expression and shared state.
fn create_job(
    schedule: &str,
    name: &str,
    shared: &JobSharedState,
) -> Result<Job> {
    let name = name.to_string();
    let shared = shared.clone();

    let job = Job::new_async(schedule, move |_, _| {
        let name = name.clone();
        let s = shared.clone();

        Box::pin(async move {
            s.tui_tx.send(TuiEvent::TickFired {
                workflow_name: name.clone(),
            });

            // Disabled guard.
            {
                let set = s.disabled.lock().await;
                if set.contains(&name) {
                    s.tui_tx.send(TuiEvent::TickSkipped {
                        workflow_name: name.clone(),
                        reason: "workflow disabled".to_string(),
                    });
                    return;
                }
            }

            // Lightweight Info pane refresh — runs on every tick even when
            // a previous run is still active, so the Info pane stays current.
            {
                let harness_root =
                    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                let refresh_opts = ExecutorOptions {
                    provider: Arc::new(crate::providers::provider::NoopProvider),
                    registry: Arc::new(ToolRegistry::new()),
                    store: s.store.clone(),
                    no_workspace: true,
                    target_repo: s.harness_config.target_repo.clone(),
                    git_method: s.harness_config.git_method.clone(),
                    harness_root: Some(harness_root),
                    prompts: s.harness_config.prompts.clone(),
                    system_prompt: None,
                    tui_tx: s.tui_tx.clone(),
                };
                let refresher = WorkflowExecutor::new(s.wf_config.clone(), refresh_opts);
                refresher.refresh_info_only().await;
            }

            // Overlap guard.
            {
                let mut lock = s.active.lock().await;
                if *lock {
                    s.tui_tx.send(TuiEvent::TickSkipped {
                        workflow_name: name.clone(),
                        reason: "previous run still active".to_string(),
                    });
                    return;
                }
                *lock = true;
            }

            let provider = match resolve_provider(&s.harness_config) {
                Ok(p) => p,
                Err(e) => {
                    s.tui_tx.send(TuiEvent::TickSkipped {
                        workflow_name: name.clone(),
                        reason: format!("provider error: {}", e),
                    });
                    *s.active.lock().await = false;
                    return;
                }
            };

            let harness_root =
                std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let system_prompt =
                resolve_system_prompt(&s.harness_config, &harness_root);

            let opts = ExecutorOptions {
                provider,
                registry: Arc::new(ToolRegistry::new()),
                store: s.store.clone(),
                no_workspace: s.no_workspace,
                target_repo: s.harness_config.target_repo.clone(),
                git_method: s.harness_config.git_method.clone(),
                harness_root: Some(harness_root),
                prompts: s.harness_config.prompts.clone(),
                system_prompt,
                tui_tx: s.tui_tx.clone(),
            };

            let executor = WorkflowExecutor::new(s.wf_config.clone(), opts);
            match executor.run().await {
                Ok(result) => {
                    s.tui_tx.send(TuiEvent::RunCompleted {
                        workflow_name: result.workflow_name.clone(),
                        run_id: result.run_id.clone(),
                        success: result.success,
                        skipped: result.skipped,
                        error: result.error.clone(),
                        pr_url: result.pr_url.clone(),
                    });
                    if let Some(cb) = &s.callback {
                        cb(result);
                    }
                }
                Err(e) => {
                    s.tui_tx.send(TuiEvent::RunCompleted {
                        workflow_name: name.clone(),
                        run_id: String::new(),
                        success: false,
                        skipped: false,
                        error: Some(e.to_string()),
                        pr_url: None,
                    });
                }
            }

            *s.active.lock().await = false;
        })
    })?;

    Ok(job)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::config::HarnessConfig;

    #[test]
    fn test_cron_runner_new_creates_instance() {
        let config = HarnessConfig {
            provider: "anthropic".into(),
            model: "claude-opus-4-6".into(),
            ..Default::default()
        };
        let runner = CronRunner::new(config, true);
        assert!(runner.on_run_complete.is_none());
        assert!(runner.no_workspace);
    }

    #[test]
    fn test_cron_runner_on_run_complete_sets_callback() {
        let config = HarnessConfig::default();
        let mut runner = CronRunner::new(config, false);
        runner.on_run_complete(Arc::new(|_result| {}));
        assert!(runner.on_run_complete.is_some());
    }

    #[test]
    fn test_cron_runner_no_workflows_is_fine() {
        let config = HarnessConfig {
            provider: "openai".into(),
            model: "gpt-4o".into(),
            ..Default::default()
        };
        let runner = CronRunner::new(config, true);
        assert!(runner.config.workflows.is_empty());
    }

    #[test]
    fn test_schedule_update_struct() {
        let update = ScheduleUpdate {
            workflow_name: "test".into(),
            new_schedule: "0 */5 * * * *".into(),
        };
        assert_eq!(update.workflow_name, "test");
        assert_eq!(update.new_schedule, "0 */5 * * * *");
    }
}
