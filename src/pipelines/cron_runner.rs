use anyhow::Result;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_cron_scheduler::{Job, JobScheduler};

use crate::pipelines::executor::{ExecutorOptions, PipelineExecutor};
use crate::pipelines::stores::{PipelineResult, RunStore};
use crate::providers::resolve_provider;
use crate::tools::registry::ToolRegistry;
use crate::types::config::HarnessConfig;
use crate::utils::logger::Logger;

/// Callback invoked after each pipeline run completes (success or failure).
pub type RunCompleteCallback = Arc<dyn Fn(PipelineResult) + Send + Sync>;

/// Schedules all configured pipelines on their cron expressions, with a
/// per-pipeline overlap guard that drops a tick if the previous run is still
/// active.
pub struct CronRunner {
    config: HarnessConfig,
    no_workspace: bool,
    on_run_complete: Option<RunCompleteCallback>,
    store: Arc<RunStore>,
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
        }
    }

    /// Register a callback to be invoked after every pipeline run completes.
    pub fn on_run_complete(&mut self, callback: RunCompleteCallback) {
        self.on_run_complete = Some(callback);
    }

    /// Start the scheduler.  Blocks until `Ctrl+C` is received, then performs
    /// a clean shutdown.
    pub async fn start(self) -> Result<()> {
        let mut sched = JobScheduler::new().await?;
        let logger = Logger::new("cron-runner");

        for pipeline_config in &self.config.pipelines {
            let name = pipeline_config.name.clone();
            let schedule = pipeline_config.schedule.clone();
            let pipeline = pipeline_config.clone();
            let config_clone = self.config.clone();
            let store = self.store.clone();
            let no_workspace = self.no_workspace;
            let callback = self.on_run_complete.clone();
            // Per-pipeline mutex: `true` while a run is active.
            let active: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));

            logger.info(&format!(
                "Scheduling pipeline \"{}\" with cron: {}",
                name, schedule
            ));

            let job = Job::new_async(schedule.as_str(), move |_, _| {
                let name = name.clone();
                let pipeline = pipeline.clone();
                let config_clone = config_clone.clone();
                let store = store.clone();
                let callback = callback.clone();
                let active = active.clone();

                Box::pin(async move {
                    // Overlap guard — skip this tick if a previous run is still running.
                    {
                        let mut lock = active.lock().await;
                        if *lock {
                            tracing::warn!(
                                "⚠ [{}] tick skipped — previous run still active",
                                name
                            );
                            return;
                        }
                        *lock = true;
                    }

                    let provider = match resolve_provider(&config_clone) {
                        Ok(p) => p,
                        Err(e) => {
                            tracing::error!("[{}] Failed to resolve provider: {}", name, e);
                            *active.lock().await = false;
                            return;
                        }
                    };

                    let opts = ExecutorOptions {
                        provider,
                        registry: Arc::new(ToolRegistry::new()),
                        store: store.clone(),
                        no_workspace,
                        target_repo: config_clone.target_repo.clone(),
                        git_method: config_clone.git_method.clone(),
                        harness_root: std::env::current_dir().ok(),
                        prompts: config_clone.prompts.clone(),
                    };

                    let executor = PipelineExecutor::new(pipeline, opts);
                    match executor.run().await {
                        Ok(result) => {
                            if let Some(cb) = &callback {
                                cb(result);
                            }
                        }
                        Err(e) => {
                            tracing::error!("[{}] Run threw: {}", name, e);
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
    fn test_cron_runner_no_pipelines_is_fine() {
        let config = HarnessConfig {
            provider: "openai".into(),
            model: "gpt-4o".into(),
            ..Default::default()
        };
        let runner = CronRunner::new(config, true);
        assert!(runner.config.pipelines.is_empty());
    }
}
