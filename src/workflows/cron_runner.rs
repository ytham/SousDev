use anyhow::Result;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_cron_scheduler::{Job, JobScheduler};

use crate::workflows::executor::{resolve_system_prompt, ExecutorOptions, WorkflowExecutor};
use crate::workflows::stores::{WorkflowResult, RunStore};
use crate::providers::resolve_provider;
use crate::tools::registry::ToolRegistry;
use crate::tui::events::{WorkflowMode, TuiEvent, TuiEventSender};
use crate::types::config::HarnessConfig;
use crate::utils::logger::Logger;

/// Callback invoked after each workflow run completes (success or failure).
pub type RunCompleteCallback = Arc<dyn Fn(WorkflowResult) + Send + Sync>;

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
        }
    }

    /// Attach a TUI event sender so the cron runner emits events for the TUI.
    pub fn with_tui_sender(mut self, tx: TuiEventSender) -> Self {
        self.tui_tx = tx;
        self
    }

    /// Attach a shared set of disabled workflow names.
    ///
    /// The TUI updates this set when the user toggles workflows.  The cron
    /// runner checks it on each tick and skips disabled workflows.
    pub fn with_disabled_workflows(mut self, set: Arc<Mutex<HashSet<String>>>) -> Self {
        self.disabled_workflows = set;
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
    pub async fn start(self) -> Result<()> {
        let mut sched = JobScheduler::new().await?;
        let logger = Logger::new("cron-runner");

        for workflow_config in &self.config.workflows {
            let name = workflow_config.name.clone();
            let schedule = workflow_config.schedule.clone();
            let wf = workflow_config.clone();
            let config_clone = self.config.clone();
            let store = self.store.clone();
            let no_workspace = self.no_workspace;
            let callback = self.on_run_complete.clone();
            let tui_tx = self.tui_tx.clone();
            // Per-workflow mutex: `true` while a run is active.
            let active: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));

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
                .or_else(|| config_clone.target_repo.clone());

            tui_tx.send(TuiEvent::WorkflowRegistered {
                name: name.clone(),
                schedule: schedule.clone(),
                mode,
                repo,
            });

            logger.info(&format!(
                "Scheduling workflow \"{}\" with cron: {}",
                name, schedule
            ));

            let disabled = self.disabled_workflows.clone();

            let job = Job::new_async(schedule.as_str(), move |_, _| {
                let name = name.clone();
                let wf = wf.clone();
                let config_clone = config_clone.clone();
                let store = store.clone();
                let callback = callback.clone();
                let active = active.clone();
                let tui_tx = tui_tx.clone();
                let disabled = disabled.clone();

                Box::pin(async move {
                    tui_tx.send(TuiEvent::TickFired {
                        workflow_name: name.clone(),
                    });

                    // Disabled guard — skip this tick if the workflow is disabled.
                    {
                        let set = disabled.lock().await;
                        if set.contains(&name) {
                            tui_tx.send(TuiEvent::TickSkipped {
                                workflow_name: name.clone(),
                                reason: "workflow disabled".to_string(),
                            });
                            return;
                        }
                    }

                    // Overlap guard — skip this tick if a previous run is still running.
                    {
                        let mut lock = active.lock().await;
                        if *lock {
                            tracing::warn!(
                                "⚠ [{}] tick skipped — previous run still active",
                                name
                            );
                            tui_tx.send(TuiEvent::TickSkipped {
                                workflow_name: name.clone(),
                                reason: "previous run still active".to_string(),
                            });
                            return;
                        }
                        *lock = true;
                    }

                    let provider = match resolve_provider(&config_clone) {
                        Ok(p) => p,
                        Err(e) => {
                            tracing::error!("[{}] Failed to resolve provider: {}", name, e);
                            tui_tx.send(TuiEvent::TickSkipped {
                                workflow_name: name.clone(),
                                reason: format!("provider error: {}", e),
                            });
                            *active.lock().await = false;
                            return;
                        }
                    };

                    let harness_root = std::env::current_dir()
                        .unwrap_or_else(|_| PathBuf::from("."));
                    let system_prompt = resolve_system_prompt(&config_clone, &harness_root);

                    let opts = ExecutorOptions {
                        provider,
                        registry: Arc::new(ToolRegistry::new()),
                        store: store.clone(),
                        no_workspace,
                        target_repo: config_clone.target_repo.clone(),
                        git_method: config_clone.git_method.clone(),
                        harness_root: Some(harness_root),
                        prompts: config_clone.prompts.clone(),
                        system_prompt,
                        tui_tx: tui_tx.clone(),
                    };

                    let executor = WorkflowExecutor::new(wf, opts);
                    match executor.run().await {
                        Ok(result) => {
                            tui_tx.send(TuiEvent::RunCompleted {
                                workflow_name: result.workflow_name.clone(),
                                run_id: result.run_id.clone(),
                                success: result.success,
                                skipped: result.skipped,
                                error: result.error.clone(),
                                pr_url: result.pr_url.clone(),
                            });
                            if let Some(cb) = &callback {
                                cb(result);
                            }
                        }
                        Err(e) => {
                            tracing::error!("[{}] Run threw: {}", name, e);
                            tui_tx.send(TuiEvent::RunCompleted {
                                workflow_name: name.clone(),
                                run_id: String::new(),
                                success: false,
                                skipped: false,
                                error: Some(e.to_string()),
                                pr_url: None,
                            });
                        }
                    }

                    *active.lock().await = false;
                })
            })?;

            sched.add(job).await?;
        }

        sched.start().await?;
        // Block until the process receives Ctrl+C.
        tokio::signal::ctrl_c().await?;
        logger.info("Shutting down cron scheduler…");
        self.tui_tx.send(TuiEvent::Shutdown);
        sched.shutdown().await?;
        Ok(())
    }
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
}
