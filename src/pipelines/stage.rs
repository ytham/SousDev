use std::path::PathBuf;
use std::sync::Arc;
use async_trait::async_trait;
use anyhow::Result;
use crate::providers::provider::LLMProvider;
use crate::tools::registry::ToolRegistry;
use crate::utils::logger::Logger;
use crate::types::technique::RunResult;
use crate::pipelines::stores::{PrReviewResult, PrResponseResult};
use crate::pipelines::workflow_log::WorkflowLog;
use crate::pipelines::pipeline::ParsedTask;
use crate::pipelines::github_prs::{GitHubPR, InlineReviewComment, PRComment};
use crate::types::config::PipelineConfig;
use crate::techniques::critique_loop::CritiqueLoopResult;

/// All resolved prompt template paths/strings, threaded through the pipeline.
///
/// Each field holds either an inline template string or a file path (resolved
/// relative to `harness_root` by [`PromptLoader`]).
#[derive(Debug, Clone, Default)]
pub struct ResolvedPrompts {
    /// Prompt for the reviewer stage (claude-loop mode).
    pub code_review: String,
    /// Prompt fed back to the agent after a reviewer rejection.
    pub review_feedback: String,
    /// Prompt that generates a TITLE: / BODY: for the pull request.
    pub pr_description: String,
    /// Prompt for the PR review poster stage.
    pub pr_review: String,
    /// Prompt for the PR comment responder stage.
    pub pr_comment_response: String,
    /// System prompt for the ReAct technique.
    pub react_system: String,
    /// System prompt for the Reflexion technique.
    pub reflexion_system: String,
    /// Reflection prompt for the Reflexion technique.
    pub reflexion_reflect: String,
}

/// Unaddressed review comments for the PR response pipeline.
#[derive(Debug, Clone, Default)]
pub struct UnaddressedComments {
    /// Inline (diff-level) review comments not yet replied to.
    pub inline: Vec<InlineReviewComment>,
    /// Timeline (issue-level) comments not yet replied to.
    pub timeline: Vec<PRComment>,
}

/// The shared mutable context threaded through every stage.
///
/// Stages receive `&mut StageContext` and set output fields directly.
/// The executor reads those fields after each stage completes.
#[derive(Clone)]
pub struct StageContext {
    /// Pipeline-level configuration (immutable for the run's lifetime).
    pub config: Arc<PipelineConfig>,
    /// LLM provider used by harness-native techniques and the reviewer.
    pub provider: Arc<dyn LLMProvider>,
    /// Tool registry (used by harness-native techniques).
    pub registry: Arc<ToolRegistry>,
    /// Absolute path of the cloned workspace directory.
    pub workspace_dir: PathBuf,
    /// The feature branch created for this run (e.g. `harness/fix-issue-42`).
    pub branch: String,
    /// Parsed task produced by the parse stage.
    pub parsed_task: ParsedTask,
    /// Absolute path of the harness installation root (for prompt resolution).
    pub harness_root: PathBuf,
    /// Resolved prompt templates for all stages.
    pub prompts: ResolvedPrompts,
    /// Optional `owner/repo` string passed down from harness config.
    pub target_repo: Option<String>,
    /// Structured logger for the pipeline.
    pub logger: Logger,
    /// UUID string identifying this pipeline run.
    pub run_id: String,
    /// Number of agent retries so far (0 on first attempt).
    pub retry_count: usize,
    /// Number of reviewer → agent feedback rounds completed.
    pub review_rounds: usize,

    // ── Stage outputs ─────────────────────────────────────────────────────────
    // Fields below are `None` until set by the relevant stage.

    /// Final result from the agent-loop stage.
    pub agent_result: Option<RunResult>,
    /// Result of the last reviewer pass.
    pub review_result: Option<CritiqueLoopResult>,
    /// Critique text to feed back to the agent; `None` when approved.
    pub review_feedback: Option<String>,
    /// URL of the pull request created by the pull-request stage.
    pub pr_url: Option<String>,
    /// LLM-generated or configured PR title.
    pub pr_title: Option<String>,
    /// LLM-generated or configured PR body.
    pub pr_generated_body: Option<String>,

    // ── PR review mode (githubPRs pipeline) ───────────────────────────────────
    /// The pull request being reviewed in the current run.
    pub reviewing_pr: Option<GitHubPR>,
    /// Summary result written by the pr-review-poster stage.
    pub pr_review_result: Option<PrReviewResult>,

    // ── PR response mode (githubPRResponses pipeline) ─────────────────────────
    /// The pull request being responded to in the current run.
    pub responding_pr: Option<GitHubPR>,
    /// Comments that have not yet been addressed.
    pub unaddressed_comments: Option<UnaddressedComments>,
    /// Summary result written by the pr-comment-responder stage.
    pub pr_response_result: Option<PrResponseResult>,

    // ── Shared across all PR modes ────────────────────────────────────────────
    /// GitHub login of the authenticated reviewer bot.
    pub reviewer_login: Option<String>,

    // ── Per-run structured log ────────────────────────────────────────────────
    /// Per-run log file written to `output/logs/<pipeline>/<run_id>.json`.
    /// Stages append entries via `ctx.workflow_log.info(stage, msg)`.
    /// `None` in tests that don't need logging.
    pub workflow_log: Option<WorkflowLog>,

    // ── Abort signal ─────────────────────────────────────────────────────────
    /// Shared flag — any stage can set this to `true` to stop the pipeline.
    pub aborted: Arc<std::sync::atomic::AtomicBool>,
}

impl StageContext {
    /// Return `true` when the abort flag has been set.
    pub fn is_aborted(&self) -> bool {
        self.aborted.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Set the abort flag, causing subsequent stages to skip.
    pub fn abort(&self) {
        self.aborted.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

/// A stage in the pipeline.
///
/// Each stage receives a mutable reference to the shared [`StageContext`],
/// reads what it needs, and writes its outputs directly onto the context.
/// Stages must not throw for business-logic failures (e.g. reviewer rejected);
/// they should record the outcome in context and return `Ok(())`.
/// Only unrecoverable errors (process crash, critical misconfiguration) should
/// be propagated as `Err`.
#[async_trait]
pub trait Stage: Send + Sync {
    /// Human-readable name used in logs and run-store records.
    fn name(&self) -> &str;

    /// Execute the stage, reading from and writing to `ctx`.
    async fn run(&self, ctx: &mut StageContext) -> Result<()>;
}
