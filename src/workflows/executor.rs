use anyhow::Result;
use chrono::Utc;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{atomic::AtomicBool, Arc};
use uuid::Uuid;

use crate::workflows::github_issues::{fetch_github_issues, repo_to_gh_identifier, FetchIssuesOptions};
use crate::workflows::linear_issues::{fetch_linear_issues, FetchLinearIssuesOptions};
use crate::workflows::github_prs::{
    detect_github_login, fetch_github_prs, fetch_inline_review_comments, fetch_pr_comments,
    FetchPRsOptions, GitHubPR, InlineReviewComment, PRComment,
};
use crate::workflows::workflow::{make_skipped_result, ParsedTask};

/// Truncate a string for info panel display.
fn truncate_title(s: &str, max: usize) -> String {
    if s.len() > max {
        format!("{}…", &s[..max.saturating_sub(1)])
    } else {
        s.to_string()
    }
}
use crate::workflows::stage::{ResolvedPrompts, Stage, StageContext, UnaddressedComments};
use crate::workflows::stages::agent_loop::AgentLoopStage;
use crate::workflows::stages::pr_checkout::PrCheckoutStage;
use crate::workflows::stages::pr_comment_responder::PrCommentResponderStage;
use crate::workflows::stages::pr_description::PrDescriptionStage;
use crate::workflows::stages::pr_review_poster::PrReviewPosterStage;
use crate::workflows::stages::pull_request::PullRequestStage;
use crate::workflows::stages::review_feedback_loop::ReviewFeedbackLoopStage;
use crate::workflows::stores::FailureCooldownStore;
use crate::workflows::stores::{
    HandledIssueRecord, HandledIssueStore, WorkflowResult, PrResponseRecord, PrResponseStore,
    PrReviewRecord, PrReviewStore, RunStore,
};
use crate::workflows::workflow_log::WorkflowLog;
use crate::workflows::workspace::WorkspaceManager;
use crate::providers::provider::LLMProvider;
use crate::tools::registry::ToolRegistry;
use crate::types::config::{WorkflowConfig, PromptConfig};
use crate::tui::events::{ItemStatus, ItemSummary, WorkflowMode, TuiEvent, TuiEventSender};
use crate::utils::logger::Logger;
use crate::utils::prompt_loader::PromptLoader;

// ---------------------------------------------------------------------------
// ExecutorOptions
// ---------------------------------------------------------------------------

/// Options supplied to a [`WorkflowExecutor`] at construction time.
pub struct ExecutorOptions {
    /// LLM provider instance shared across all stages.
    pub provider: Arc<dyn LLMProvider>,
    /// Tool registry shared across all stages.
    pub registry: Arc<ToolRegistry>,
    /// Append-only store for workflow run records.
    pub store: Arc<RunStore>,
    /// Skip workspace setup (useful for tests and dry-runs).
    pub no_workspace: bool,
    /// Target repository in `owner/repo` form or a full GitHub URL.
    pub target_repo: Option<String>,
    /// Git transport method: `"ssh"` or `"https"`.
    pub git_method: Option<String>,
    /// Absolute path of the harness installation root.
    pub harness_root: Option<PathBuf>,
    /// Harness-level prompt overrides (lowest precedence).
    pub prompts: Option<PromptConfig>,
    /// Resolved system prompt text (already template-substituted).
    pub system_prompt: Option<String>,
    /// TUI event sender for real-time UI updates (no-op in headless mode).
    pub tui_tx: TuiEventSender,
}

// ---------------------------------------------------------------------------
// WorkflowExecutor
// ---------------------------------------------------------------------------

/// Orchestrates all workflow modes: standard, GitHub Issues, GitHub PRs, and
/// GitHub PR Responses.
pub struct WorkflowExecutor {
    config: Arc<WorkflowConfig>,
    opts: ExecutorOptions,
    issue_store: HandledIssueStore,
    pr_review_store: PrReviewStore,
    pr_response_store: PrResponseStore,
    failure_store: FailureCooldownStore,
    /// Cached GitHub login for the authenticated user (lazily populated).
    reviewer_login: tokio::sync::Mutex<Option<String>>,
}

/// Source-agnostic issue representation used by the executor.
///
/// Both GitHub and Linear issues are mapped to this before the executor
/// processes them through the stage workflow.
#[derive(Debug, Clone)]
struct IssueItem {
    /// Unique numeric identifier (GitHub issue number or Linear number).
    number: u64,
    /// Human-readable identifier (e.g. `"#42"` or `"ENG-42"`).
    display_id: String,
    /// Issue title.
    title: String,
    /// Issue body/description.
    body: String,
    /// URL to the issue.
    url: String,
    /// Repository or project identifier for the handled-issue record.
    repo: String,
}

impl IssueItem {
    fn from_github(issue: &crate::workflows::github_issues::GitHubIssue) -> Self {
        Self {
            number: issue.number,
            display_id: format!("#{}", issue.number),
            title: issue.title.clone(),
            body: issue.body_str().to_string(),
            url: issue.url.clone(),
            repo: issue.repo.clone(),
        }
    }

    fn from_linear(issue: &crate::workflows::linear_issues::LinearIssue, repo: &str) -> Self {
        Self {
            number: issue.number,
            display_id: issue.identifier.clone(),
            title: issue.title.clone(),
            body: issue.description_str().to_string(),
            url: issue.url.clone(),
            repo: repo.to_string(),
        }
    }
}

/// Resolve the system prompt from config.
///
/// Loads the template (inline string or `.md` file), then substitutes
/// `{{blocked_commands}}` with the formatted blocked commands list.
/// Returns `None` if no system prompt is configured and the default
/// `prompts/system.md` does not exist.
pub fn resolve_system_prompt(config: &crate::types::config::HarnessConfig, harness_root: &std::path::Path) -> Option<String> {
    // Load the template: explicit config value, or default file.
    let template = if let Some(ref sp) = config.system_prompt {
        // Check if it's a file path.
        if !sp.contains('\n') && (sp.ends_with(".md") || sp.ends_with(".txt") || sp.ends_with(".prompt")) {
            let path = if std::path::Path::new(sp).is_absolute() {
                PathBuf::from(sp)
            } else {
                harness_root.join(sp)
            };
            std::fs::read_to_string(path).ok()
        } else {
            // Inline template string.
            Some(sp.clone())
        }
    } else {
        // Try the default prompts/system.md
        let default_path = harness_root.join("prompts").join("system.md");
        if default_path.exists() {
            std::fs::read_to_string(default_path).ok()
        } else {
            None
        }
    };

    let template = template?;

    // Build the blocked commands section.
    let blocked_section = if config.blocked_commands.is_empty() {
        String::new()
    } else {
        let mut section = String::from("\nBlocked commands — you must NEVER run these:\n");
        for cmd in &config.blocked_commands {
            section.push_str(&format!("- `{}`\n", cmd));
        }
        section
    };

    let rendered = template.replace("{{blocked_commands}}", &blocked_section);
    Some(rendered.trim().to_string())
}

impl WorkflowExecutor {
    /// Create a new executor for a single workflow.
    pub fn new(config: WorkflowConfig, opts: ExecutorOptions) -> Self {
        let store_dir = opts
            .harness_root
            .clone()
            .unwrap_or_else(|| PathBuf::from("."));
        Self {
            config: Arc::new(config),
            issue_store: HandledIssueStore::new(&store_dir),
            pr_review_store: PrReviewStore::new(&store_dir),
            pr_response_store: PrResponseStore::new(&store_dir),
            failure_store: FailureCooldownStore::new(&store_dir),
            reviewer_login: tokio::sync::Mutex::new(None),
            opts,
        }
    }

    /// Select and run the appropriate workflow mode.
    pub async fn run(&self) -> Result<WorkflowResult> {
        if self.config.github_pr_responses.is_some() {
            return self.run_pr_response_mode().await;
        }
        if self.config.github_prs.is_some() {
            return self.run_prs_mode().await;
        }
        if self.config.github_issues.is_some() || self.config.linear_issues.is_some() {
            return self.run_issues_mode().await;
        }
        self.run_standard_mode().await
    }

    /// Run a single stage with TUI event emission around it.
    async fn run_stage(
        &self,
        stage: &dyn Stage,
        ctx: &mut StageContext,
    ) -> Result<()> {
        let stage_name = stage.name().to_string();
        self.opts.tui_tx.send(TuiEvent::StageStarted {
            workflow_name: self.config.name.clone(),
            run_id: ctx.run_id.clone(),
            stage_name: stage_name.clone(),
        });
        match stage.run(ctx).await {
            Ok(()) => {
                self.opts.tui_tx.send(TuiEvent::StageCompleted {
                    workflow_name: self.config.name.clone(),
                    run_id: ctx.run_id.clone(),
                    stage_name,
                });
                Ok(())
            }
            Err(e) => {
                self.opts.tui_tx.send(TuiEvent::StageFailed {
                    workflow_name: self.config.name.clone(),
                    run_id: ctx.run_id.clone(),
                    stage_name,
                    error: e.to_string(),
                });
                Err(e)
            }
        }
    }

    // ── Shared helpers ────────────────────────────────────────────────────────

    /// Return the GitHub login of the authenticated user, caching after the
    /// first successful lookup.
    async fn get_reviewer_login(&self) -> Result<String> {
        let mut lock = self.reviewer_login.lock().await;
        if let Some(ref login) = *lock {
            return Ok(login.clone());
        }
        let login = detect_github_login().await?;
        *lock = Some(login.clone());
        Ok(login)
    }

    /// Build [`ResolvedPrompts`] by merging workflow-level overrides on top of
    /// harness-level defaults, falling back to conventional file paths under
    /// `harness_root/prompts/`.
    fn build_resolved_prompts(&self) -> ResolvedPrompts {
        let harness_root = self
            .opts
            .harness_root
            .clone()
            .unwrap_or_else(|| PathBuf::from("."));
        let root = harness_root.to_string_lossy();

        let default_prompt =
            |name: &str| -> String { format!("{}/prompts/{}", root, name) };

        let hp = self.opts.prompts.as_ref();
        let pp = self.config.prompts.as_ref();

        ResolvedPrompts {
            code_review: pp
                .and_then(|p| p.code_review.clone())
                .or_else(|| hp.and_then(|p| p.code_review.clone()))
                .unwrap_or_else(|| default_prompt("code-review.md")),
            review_feedback: pp
                .and_then(|p| p.review_feedback.clone())
                .or_else(|| hp.and_then(|p| p.review_feedback.clone()))
                .unwrap_or_else(|| default_prompt("review-feedback.md")),
            pr_description: pp
                .and_then(|p| p.pr_description.clone())
                .or_else(|| hp.and_then(|p| p.pr_description.clone()))
                .unwrap_or_else(|| default_prompt("pr-description.md")),
            pr_review: hp
                .and_then(|p| p.pr_review.clone())
                .unwrap_or_else(|| default_prompt("pr-review.md")),
            pr_comment_response: hp
                .and_then(|p| p.pr_comment_response.clone())
                .unwrap_or_else(|| default_prompt("pr-comment-response.md")),
            react_system: hp
                .and_then(|p| p.react_system.clone())
                .unwrap_or_else(|| default_prompt("react-system.md")),
            reflexion_system: hp
                .and_then(|p| p.reflexion_system.clone())
                .unwrap_or_else(|| default_prompt("reflexion-system.md")),
            reflexion_reflect: hp
                .and_then(|p| p.reflexion_reflect.clone())
                .unwrap_or_else(|| default_prompt("reflexion-reflect.md")),
        }
    }

    /// Construct a base [`StageContext`] without workspace or PR-specific fields.
    fn make_base_ctx(
        &self,
        run_id: &str,
        parsed_task: ParsedTask,
        retry_count: usize,
        workflow_log: Option<WorkflowLog>,
    ) -> StageContext {
        let harness_root = self
            .opts
            .harness_root
            .clone()
            .unwrap_or_else(|| PathBuf::from("."));

        StageContext {
            config: self.config.clone(),
            provider: self.opts.provider.clone(),
            registry: self.opts.registry.clone(),
            workspace_dir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            branch: String::new(),
            parsed_task,
            harness_root,
            prompts: self.build_resolved_prompts(),
            system_prompt: self.opts.system_prompt.clone(),
            target_repo: self.opts.target_repo.clone(),
            logger: Logger::new(&self.config.name),
            run_id: run_id.to_string(),
            retry_count,
            review_rounds: 0,
            agent_result: None,
            review_result: None,
            review_feedback: None,
            issue_url: None,
            issue_display_id: None,
            pr_url: None,
            pr_title: None,
            pr_generated_body: None,
            reviewing_pr: None,
            pr_review_result: None,
            responding_pr: None,
            unaddressed_comments: None,
            pr_response_result: None,
            reviewer_login: None,
            workflow_log,
            aborted: Arc::new(AtomicBool::new(false)),
        }
    }

    // ── Issues mode (GitHub or Linear) ──────────────────────────────────────

    async fn run_issues_mode(&self) -> Result<WorkflowResult> {
        let run_id = Uuid::new_v4().to_string();
        let started_at = Utc::now().to_rfc3339();
        let logger = Logger::new(&self.config.name);

        // Fetch issues from the configured source (GitHub or Linear).
        let items: Vec<IssueItem> = if let Some(ref linear_cfg) = self.config.linear_issues {
            let repo = self.opts.target_repo.clone().unwrap_or_default();

            logger.info("Fetching Linear issues");

            let issues = match fetch_linear_issues(&FetchLinearIssuesOptions {
                team: linear_cfg.team.clone(),
                states: linear_cfg.states.clone(),
                labels: linear_cfg.labels.clone(),
                assignee: linear_cfg.assignee.clone(),
                limit: linear_cfg.limit,
            })
            .await
            {
                Ok(i) => i,
                Err(e) => {
                    let error = e.to_string();
                    logger.error(&format!("Failed to fetch Linear issues: {}", error));
                    let result =
                        make_skipped_result(&self.config.name, &run_id, &started_at, Some(&error));
                    self.opts.store.append(&result).await?;
                    return Ok(result);
                }
            };

            issues.iter().map(|i| IssueItem::from_linear(i, &repo)).collect()
        } else if let Some(ref issues_cfg) = self.config.github_issues {
            let gh_repo = issues_cfg
                .repo
                .clone()
                .or_else(|| repo_to_gh_identifier(self.opts.target_repo.as_deref()));

            logger.info("Fetching GitHub issues");

            let issues = match fetch_github_issues(&FetchIssuesOptions {
                repo: gh_repo,
                assignees: issues_cfg.assignees.clone(),
                labels: issues_cfg.labels.clone(),
                limit: issues_cfg.limit,
            })
            .await
            {
                Ok(i) => i,
                Err(e) => {
                    let error = e.to_string();
                    logger.error(&format!("Failed to fetch GitHub issues: {}", error));
                    let result =
                        make_skipped_result(&self.config.name, &run_id, &started_at, Some(&error));
                    self.opts.store.append(&result).await?;
                    return Ok(result);
                }
            };

            issues.iter().map(IssueItem::from_github).collect()
        } else {
            // Neither source configured — should not happen (caught by mode routing).
            let result = make_skipped_result(
                &self.config.name,
                &run_id,
                &started_at,
                Some("No issue source configured (github_issues or linear_issues)"),
            );
            self.opts.store.append(&result).await?;
            return Ok(result);
        };

        logger.info(&format!("Fetched {} issue(s)", items.len()));

        let mut unhandled = Vec::new();
        for item in &items {
            if self
                .issue_store
                .is_handled(&self.config.name, item.number)
                .await?
            {
                continue;
            }
            // Skip items in failure cooldown (prevents infinite retries).
            let item_key = item.number.to_string();
            if self
                .failure_store
                .is_in_cooldown(&self.config.name, &item_key)
                .await?
            {
                logger.info(&format!(
                    "Skipping issue {} — in failure cooldown",
                    item.display_id
                ));
                continue;
            }
            unhandled.push(item.clone());
        }

        // Emit item summaries for the TUI info panel.
        {
            let mut summaries: Vec<ItemSummary> = Vec::new();
            for item in &items {
                let item_key = item.number.to_string();
                let is_handled = self
                    .issue_store
                    .is_handled(&self.config.name, item.number)
                    .await
                    .unwrap_or(false);
                let in_cooldown = self
                    .failure_store
                    .is_in_cooldown(&self.config.name, &item_key)
                    .await
                    .unwrap_or(false);
                let status = if is_handled {
                    ItemStatus::Success
                } else if in_cooldown {
                    ItemStatus::Cooldown
                } else {
                    ItemStatus::None
                };
                summaries.push(ItemSummary {
                    id: item.display_id.clone(),
                    title: truncate_title(&item.title, 50),
                    url: item.url.clone(),
                    status,
                });
            }
            self.opts.tui_tx.send(TuiEvent::ItemsSummary {
                workflow_name: self.config.name.clone(),
                items: summaries,
            });
        }

        if unhandled.is_empty() {
            logger.info("No new issues to process — skipping");
            let result = make_skipped_result(&self.config.name, &run_id, &started_at, None);
            self.opts.store.append(&result).await?;
            return Ok(result);
        }

        logger.info(&format!(
            "Processing {} issue(s) sequentially",
            unhandled.len()
        ));

        let mut last_result: Option<WorkflowResult> = None;
        for item in &unhandled {
            let result = match self.run_single_issue(item).await {
                Ok(r) => r,
                Err(e) => {
                    let completed_at = Utc::now().to_rfc3339();
                    WorkflowResult {
                        workflow_name: self.config.name.clone(),
                        run_id: Uuid::new_v4().to_string(),
                        started_at: Utc::now().to_rfc3339(),
                        completed_at,
                        success: false,
                        skipped: false,
                        error: Some(e.to_string()),
                        issue_number: Some(item.number),
                        retry_count: 0,
                        review_rounds: 0,
                        trajectory: vec![],
                        ..Default::default()
                    }
                }
            };

            self.opts.store.append(&result).await?;

            let item_key = item.number.to_string();
            if result.success && !result.skipped {
                // Clear any prior failure cooldown on success.
                let _ = self
                    .failure_store
                    .clear_failure(&self.config.name, &item_key)
                    .await;
                if let Some(ref pr_url) = result.pr_url {
                    self.issue_store
                        .mark_handled_with_number(
                            &self.config.name,
                            item.number,
                            HandledIssueRecord {
                                pr_number: None,
                                issue_url: item.url.clone(),
                                issue_title: item.title.clone(),
                                issue_repo: item.repo.clone(),
                                pr_url: Some(pr_url.clone()),
                                pr_open: true,
                                handled_at: Utc::now().to_rfc3339(),
                                updated_at: Utc::now().to_rfc3339(),
                            },
                        )
                        .await?;
                }
            } else {
                // Record failure — item will be skipped until cooldown expires.
                let _ = self
                    .failure_store
                    .record_failure(&self.config.name, &item_key)
                    .await;
                logger.info(&format!(
                    "Issue {} entered failure cooldown",
                    item.display_id
                ));
            }

            last_result = Some(result);
        }

        Ok(last_result.unwrap_or_else(|| {
            make_skipped_result(&self.config.name, &run_id, &started_at, None)
        }))
    }

    async fn run_single_issue(
        &self,
        issue: &IssueItem,
    ) -> Result<WorkflowResult> {
        let run_id = Uuid::new_v4().to_string();
        let started_at = Utc::now().to_rfc3339();
        let logger = Logger::new(format!("{} {}", self.config.name, issue.display_id));

        let parsed_task = ParsedTask::new(format!(
            "Fix issue {}: {}\n\n{}",
            issue.display_id,
            issue.title,
            issue.body
        ));

        let log_label = format!("issue-{}", issue.number);
        let wf_log = WorkflowLog::with_tui_sender_and_label(
            &self.opts.harness_root.clone().unwrap_or_else(|| PathBuf::from(".")),
            &self.config.name,
            &run_id,
            Some(&log_label),
            self.opts.tui_tx.clone(),
        ).await.ok();
        let mut ctx = self.make_base_ctx(&run_id, parsed_task, 0, wf_log.clone());
        ctx.issue_url = Some(issue.url.clone());
        ctx.issue_display_id = Some(issue.display_id.clone());
        let mut workspace_cleanup: Option<(WorkspaceManager, crate::workflows::workspace::WorkspaceInfo)> = None;

        self.opts.tui_tx.send(TuiEvent::RunStarted {
            workflow_name: self.config.name.clone(),
            run_id: run_id.clone(),
            mode: WorkflowMode::Issues,
            item_label: Some(format!("issue {}", issue.display_id)),
        });

        let result = async {
            if !self.opts.no_workspace {
                let ws = WorkspaceManager::new(
                    self.config.workspace.clone().unwrap_or_default(),
                    logger.clone(),
                    self.opts.target_repo.clone(),
                    self.opts.git_method.as_deref().unwrap_or("https"),
                );
                let info = ws.setup(&run_id, Some(issue.number)).await?;
                ctx.workspace_dir = info.dir.clone();
                ctx.branch = info.branch.clone();
                workspace_cleanup = Some((ws, info));
            }

            self.run_stage(&AgentLoopStage, &mut ctx).await?;
            self.run_stage(&ReviewFeedbackLoopStage, &mut ctx).await?;
            self.run_stage(&PrDescriptionStage, &mut ctx).await?;
            self.run_stage(&PullRequestStage, &mut ctx).await?;
            Ok::<_, anyhow::Error>(())
        }
        .await;

        let completed_at = Utc::now().to_rfc3339();

        match result {
            Ok(_) => {
                // Tear down workspace only on success.
                if let Some((ws, info)) = workspace_cleanup {
                    ws.teardown(&info).await.ok();
                }
                if let Some(ref log) = wf_log {
                    let _ = log.complete("success").await;
                }
                let trajectory = ctx.agent_result.as_ref()
                    .map(|r| r.trajectory.clone())
                    .unwrap_or_default();
                Ok(WorkflowResult {
                    workflow_name: self.config.name.clone(),
                    run_id,
                    started_at,
                    completed_at,
                    success: true,
                    skipped: false,
                    pr_url: ctx.pr_url,
                    pr_title: ctx.pr_title,
                    agent_result: ctx.agent_result,
                    review_result: ctx.review_result,
                    issue_number: Some(issue.number),
                    retry_count: 0,
                    review_rounds: ctx.review_rounds,
                    trajectory,
                    ..Default::default()
                })
            }
            Err(e) => {
                if let Some(ref log) = wf_log {
                    let _ = log.error("executor", &e.to_string()).await;
                    let _ = log.complete("failed").await;
                }
                Ok(WorkflowResult {
                    workflow_name: self.config.name.clone(),
                    run_id,
                    started_at,
                    completed_at,
                    success: false,
                    skipped: false,
                    error: Some(e.to_string()),
                    issue_number: Some(issue.number),
                    retry_count: 0,
                    review_rounds: 0,
                    trajectory: vec![],
                    ..Default::default()
                })
            }
        }
    }

    // ── GitHub PRs (review) mode ───────────────────────────────────────────────

    async fn run_prs_mode(&self) -> Result<WorkflowResult> {
        let prs_cfg = self.config.github_prs.as_ref().unwrap();
        let run_id = Uuid::new_v4().to_string();
        let started_at = Utc::now().to_rfc3339();
        let logger = Logger::new(&self.config.name);

        let gh_repo = prs_cfg
            .repo
            .clone()
            .or_else(|| repo_to_gh_identifier(self.opts.target_repo.as_deref()));

        logger.info("Fetching PRs with review requested");

        let prs = match fetch_github_prs(&FetchPRsOptions {
            repo: gh_repo,
            search: prs_cfg.search.clone(),
            limit: prs_cfg.limit,
            raw_search: false, // Use reviewer dual-search.
        })
        .await
        {
            Ok(p) => p,
            Err(e) => {
                let error = e.to_string();
                logger.error(&format!("Failed to fetch GitHub PRs: {}", error));
                let result =
                    make_skipped_result(&self.config.name, &run_id, &started_at, Some(&error));
                self.opts.store.append(&result).await?;
                return Ok(result);
            }
        };

        let reviewer_login = match self.get_reviewer_login().await {
            Ok(l) => l,
            Err(e) => {
                let error = e.to_string();
                logger.error(&format!("Failed to detect GitHub login: {}", error));
                let result =
                    make_skipped_result(&self.config.name, &run_id, &started_at, Some(&error));
                self.opts.store.append(&result).await?;
                return Ok(result);
            }
        };

        logger.info(&format!("Fetched {} PR(s) from GitHub search", prs.len()));

        let prs_config = self.config.github_prs.as_ref();
        let assignee_filter: Option<Vec<String>> = prs_config
            .and_then(|c| c.assignees.clone())
            .map(|list| {
                list.into_iter()
                    .map(|a| {
                        if a == "@me" {
                            reviewer_login.clone()
                        } else {
                            a
                        }
                    })
                    .collect()
            });

        // Filter PRs based on configured criteria.
        //
        // A PR is included if ANY of these are true:
        //   1. The user is individually listed as a requested reviewer
        //   2. The user is in the assignees list (when assignee filter is configured)
        //
        // PRs that only match via team-level review requests (e.g. "eng" team
        // auto-added to all PRs) are excluded unless the user is also assigned
        // or individually requested.
        let prs: Vec<GitHubPR> = prs
            .into_iter()
            .filter(|pr| {
                // Check 1: individually requested as reviewer.
                let individually_requested = pr
                    .requested_reviewers
                    .iter()
                    .any(|r| r == &reviewer_login);

                if individually_requested {
                    return true;
                }

                // Check 2: assigned to the user (when assignee filter is configured).
                if let Some(ref allowed) = assignee_filter {
                    if pr.assignees.iter().any(|a| allowed.contains(a)) {
                        return true;
                    }
                }

                // Neither individually requested nor assigned — skip.
                // This filters out PRs that only match via team membership.
                false
            })
            .collect();

        logger.info(&format!(
            "{} PR(s) after filtering (reviewer: {}{})",
            prs.len(),
            reviewer_login,
            if assignee_filter.is_some() {
                ", with assignee filter"
            } else {
                ""
            }
        ));

        let mut to_review: Vec<&GitHubPR> = Vec::new();
        for pr in &prs {
            // Skip PRs in failure cooldown.
            let pr_key = format!("pr-{}", pr.number);
            if self
                .failure_store
                .is_in_cooldown(&self.config.name, &pr_key)
                .await?
            {
                logger.info(&format!(
                    "Skipping PR #{} — in failure cooldown",
                    pr.number
                ));
                continue;
            }

            let record = self
                .pr_review_store
                .get_record(&self.config.name, pr.number)
                .await?;
            match record {
                None => {
                    to_review.push(pr);
                }
                Some(ref rec) if rec.head_sha != pr.head_ref_oid => {
                    logger.info(&format!(
                        "PR #{} has new commits — re-reviewing",
                        pr.number
                    ));
                    to_review.push(pr);
                }
                Some(ref rec) => {
                    let new_comments =
                        fetch_pr_comments(&pr.repo, pr.number, Some(rec.last_comment_id))
                            .await
                            .unwrap_or_default();
                    let trigger = format!("@{}", reviewer_login);
                    if new_comments.iter().any(|c| c.body.trim() == trigger) {
                        logger.info(&format!(
                            "PR #{} has a \"{}\" comment — re-reviewing",
                            pr.number, trigger
                        ));
                        to_review.push(pr);
                    } else {
                        logger.info(&format!(
                            "PR #{} already reviewed — skipping",
                            pr.number
                        ));
                    }
                }
            }
        }

        // Emit item summaries for the TUI info panel.
        {
            let mut summaries: Vec<ItemSummary> = Vec::new();
            for pr in &prs {
                let pr_key = format!("pr-{}", pr.number);
                let in_cooldown = self
                    .failure_store
                    .is_in_cooldown(&self.config.name, &pr_key)
                    .await
                    .unwrap_or(false);
                let is_reviewed = self
                    .pr_review_store
                    .get_record(&self.config.name, pr.number)
                    .await
                    .ok()
                    .flatten()
                    .is_some();
                let status = if in_cooldown {
                    ItemStatus::Cooldown
                } else if is_reviewed {
                    ItemStatus::Reviewed
                } else {
                    ItemStatus::None
                };
                summaries.push(ItemSummary {
                    id: format!("PR #{}", pr.number),
                    title: truncate_title(&pr.title, 50),
                    url: pr.url.clone(),
                    status,
                });
            }
            self.opts.tui_tx.send(TuiEvent::ItemsSummary {
                workflow_name: self.config.name.clone(),
                items: summaries,
            });
        }

        if to_review.is_empty() {
            logger.info("No new PRs to review — skipping");
            let result = make_skipped_result(&self.config.name, &run_id, &started_at, None);
            self.opts.store.append(&result).await?;
            return Ok(result);
        }

        let mut last_result: Option<WorkflowResult> = None;
        for pr in to_review {
            logger.info(&format!("Reviewing PR #{}: {}", pr.number, pr.title));
            let result = self
                .run_single_pr_review(pr, &reviewer_login)
                .await?;
            self.opts.store.append(&result).await?;

            let pr_key = format!("pr-{}", pr.number);
            if result.success && !result.skipped {
                let _ = self
                    .failure_store
                    .clear_failure(&self.config.name, &pr_key)
                    .await;
                let all_comments =
                    fetch_pr_comments(&pr.repo, pr.number, None)
                        .await
                        .unwrap_or_default();
                let last_comment_id = all_comments.iter().map(|c| c.id).max().unwrap_or(0);
                self.pr_review_store
                    .mark_reviewed(
                        &self.config.name,
                        PrReviewRecord {
                            pr_number: pr.number,
                            pr_url: pr.url.clone(),
                            pr_title: pr.title.clone(),
                            pr_repo: pr.repo.clone(),
                            head_sha: pr.head_ref_oid.clone(),
                            last_comment_id,
                            reviewed_at: Utc::now().to_rfc3339(),
                        },
                    )
                    .await?;
            } else {
                let _ = self
                    .failure_store
                    .record_failure(&self.config.name, &pr_key)
                    .await;
                logger.info(&format!(
                    "PR #{} entered failure cooldown",
                    pr.number
                ));
            }
            last_result = Some(result);
        }

        Ok(last_result.unwrap_or_else(|| {
            make_skipped_result(&self.config.name, &run_id, &started_at, None)
        }))
    }

    async fn run_single_pr_review(
        &self,
        pr: &GitHubPR,
        _reviewer_login: &str,
    ) -> Result<WorkflowResult> {
        // Runtime enforcement: must be claude-loop
        if self.config.agent_loop.technique != "claude-loop" {
            return Ok(WorkflowResult {
                workflow_name: self.config.name.clone(),
                run_id: Uuid::new_v4().to_string(),
                started_at: Utc::now().to_rfc3339(),
                completed_at: Utc::now().to_rfc3339(),
                success: false,
                skipped: false,
                error: Some(format!(
                    "githubPRs workflows require agentLoop.technique: \"claude-loop\".\n\n\
                     The PR review agent must be able to run shell commands (git diff, file reads) \
                     to inspect the PR changes in the workspace. Harness-native techniques (\"{}\") \
                     receive only text and cannot access the workspace filesystem.\n\n\
                     Fix: set agent_loop.technique to \"claude-loop\" in your githubPRs workflow config.",
                    self.config.agent_loop.technique
                )),
                pr_number: Some(pr.number),
                retry_count: 0,
                review_rounds: 0,
                trajectory: vec![],
                ..Default::default()
            });
        }

        let run_id = Uuid::new_v4().to_string();
        let started_at = Utc::now().to_rfc3339();
        let logger = Logger::new(format!("{}#{}", self.config.name, pr.number));

        let parsed_task =
            ParsedTask::new(format!("Review PR #{}: {}", pr.number, pr.title));
        let log_label = format!("pr-{}", pr.number);
        let wf_log = WorkflowLog::with_tui_sender_and_label(
            &self.opts.harness_root.clone().unwrap_or_else(|| PathBuf::from(".")),
            &self.config.name,
            &run_id,
            Some(&log_label),
            self.opts.tui_tx.clone(),
        ).await.ok();
        let mut ctx = self.make_base_ctx(&run_id, parsed_task, 0, wf_log.clone());
        ctx.reviewing_pr = Some(pr.clone());

        self.opts.tui_tx.send(TuiEvent::RunStarted {
            workflow_name: self.config.name.clone(),
            run_id: run_id.clone(),
            mode: WorkflowMode::PrReview,
            item_label: Some(format!("PR #{}", pr.number)),
        });

        let result = async {
            if !self.opts.no_workspace {
                let ws = WorkspaceManager::new(
                    self.config.workspace.clone().unwrap_or_default(),
                    logger.clone(),
                    self.opts.target_repo.clone(),
                    self.opts.git_method.as_deref().unwrap_or("https"),
                );
                let info = ws.setup_for_pr_review(pr, &run_id).await?;
                ctx.workspace_dir = info.dir;
                ctx.branch = info.branch;
            }
            self.run_stage(&PrCheckoutStage, &mut ctx).await?;
            self.run_stage(&AgentLoopStage, &mut ctx).await?;
            self.run_stage(&PrReviewPosterStage, &mut ctx).await?;
            Ok::<_, anyhow::Error>(())
        }
        .await;

        let completed_at = Utc::now().to_rfc3339();

        match result {
            Ok(_) => {
                if let Some(ref log) = wf_log {
                    let _ = log.complete("success").await;
                }
                let trajectory = ctx.agent_result.as_ref()
                    .map(|r| r.trajectory.clone())
                    .unwrap_or_default();
                Ok(WorkflowResult {
                    workflow_name: self.config.name.clone(),
                    run_id,
                    started_at,
                    completed_at,
                    success: true,
                    skipped: false,
                    pr_number: Some(pr.number),
                    pr_review_result: ctx.pr_review_result,
                    agent_result: ctx.agent_result,
                    retry_count: 0,
                    review_rounds: 0,
                    trajectory,
                    ..Default::default()
                })
            }
            Err(e) => {
                if let Some(ref log) = wf_log {
                    let _ = log.error("executor", &e.to_string()).await;
                    let _ = log.complete("failed").await;
                }
                Ok(WorkflowResult {
                    workflow_name: self.config.name.clone(),
                    run_id,
                    started_at,
                    completed_at,
                    success: false,
                    skipped: false,
                    error: Some(e.to_string()),
                    pr_number: Some(pr.number),
                    retry_count: 0,
                    review_rounds: 0,
                    trajectory: vec![],
                    ..Default::default()
                })
            }
        }
    }

    // ── GitHub PR Response mode ───────────────────────────────────────────────

    async fn run_pr_response_mode(&self) -> Result<WorkflowResult> {
        let resp_cfg = self.config.github_pr_responses.as_ref().unwrap();
        let run_id = Uuid::new_v4().to_string();
        let started_at = Utc::now().to_rfc3339();
        let logger = Logger::new(&self.config.name);

        let gh_repo = resp_cfg
            .repo
            .clone()
            .or_else(|| repo_to_gh_identifier(self.opts.target_repo.as_deref()));

        let search = format!(
            "author:@me{}",
            resp_cfg
                .search
                .as_deref()
                .map(|s| format!(" {}", s))
                .unwrap_or_default()
        );

        let prs = match fetch_github_prs(&FetchPRsOptions {
            repo: gh_repo,
            search: Some(search),
            limit: resp_cfg.limit,
            raw_search: true, // Simple author search, not reviewer dual-search.
        })
        .await
        {
            Ok(p) => p,
            Err(e) => {
                let error = e.to_string();
                logger.error(&format!("Failed to fetch authored PRs: {}", error));
                let result =
                    make_skipped_result(&self.config.name, &run_id, &started_at, Some(&error));
                self.opts.store.append(&result).await?;
                return Ok(result);
            }
        };

        let reviewer_login = match self.get_reviewer_login().await {
            Ok(l) => l,
            Err(e) => {
                let error = e.to_string();
                logger.error(&format!("Failed to detect GitHub login: {}", error));
                let result =
                    make_skipped_result(&self.config.name, &run_id, &started_at, Some(&error));
                self.opts.store.append(&result).await?;
                return Ok(result);
            }
        };

        struct PRWithComments {
            pr: GitHubPR,
            inline: Vec<InlineReviewComment>,
            timeline: Vec<PRComment>,
        }

        let mut to_respond: Vec<PRWithComments> = Vec::new();
        for pr in &prs {
            let record = self
                .pr_response_store
                .get_record(&self.config.name, pr.number)
                .await?;
            let after_inline = record.as_ref().map(|r| r.last_inline_comment_id);
            let after_timeline = record.as_ref().map(|r| r.last_timeline_comment_id);

            logger.info(&format!(
                "PR #{}: checking comments (cursor: inline={:?}, timeline={:?})",
                pr.number, after_inline, after_timeline
            ));

            let inline =
                fetch_inline_review_comments(&pr.repo, pr.number, after_inline)
                    .await
                    .unwrap_or_default();
            let mut timeline =
                fetch_pr_comments(&pr.repo, pr.number, after_timeline)
                    .await
                    .unwrap_or_default();

            // Also fetch PR review bodies (submitted reviews with body text).
            // These are a separate API endpoint from timeline comments.
            let review_comments =
                crate::workflows::github_prs::fetch_pr_review_comments(
                    &pr.repo, pr.number, after_timeline,
                )
                .await
                .unwrap_or_default();
            // Merge review comments into timeline (dedup by ID).
            let existing_ids: std::collections::HashSet<u64> =
                timeline.iter().map(|c| c.id).collect();
            for rc in review_comments {
                if !existing_ids.contains(&rc.id) {
                    timeline.push(rc);
                }
            }

            logger.info(&format!(
                "PR #{}: found {} inline, {} timeline/review comments after cursor",
                pr.number, inline.len(), timeline.len()
            ));

            // Filter out bot comments — they're automated and shouldn't
            // trigger the agent (CI status, deploy previews, etc.).
            let is_bot = |login: &str| -> bool {
                login.ends_with("[bot]") || login == "github-actions"
            };

            // Inline review comments: include all humans (including yourself).
            // You may leave inline comments on your own PR to direct the agent
            // (e.g. "use a toast instead of an alert").
            let new_inline: Vec<_> = inline
                .into_iter()
                .filter(|c| !is_bot(&c.login))
                .collect();

            // Timeline comments: include all humans (including yourself).
            let new_timeline: Vec<_> = timeline
                .into_iter()
                .filter(|c| !is_bot(&c.login))
                .collect();

            if new_inline.is_empty() && new_timeline.is_empty() {
                logger.info(&format!(
                    "PR #{} has no new comments after filtering — skipping",
                    pr.number
                ));
            } else {
                logger.info(&format!(
                    "PR #{} has {} new inline, {} new timeline comment(s)",
                    pr.number,
                    new_inline.len(),
                    new_timeline.len()
                ));
                to_respond.push(PRWithComments {
                    pr: pr.clone(),
                    inline: new_inline,
                    timeline: new_timeline,
                });
            }
        }

        // Emit item summaries for the TUI info panel.
        {
            let responded_set: std::collections::HashSet<u64> = to_respond
                .iter()
                .map(|r| r.pr.number)
                .collect();
            let mut summaries: Vec<ItemSummary> = Vec::new();
            for pr in &prs {
                let has_new_comments = responded_set.contains(&pr.number);
                let status = if has_new_comments {
                    ItemStatus::NewComments
                } else {
                    ItemStatus::NoNewComments
                };
                summaries.push(ItemSummary {
                    id: format!("PR #{}", pr.number),
                    title: truncate_title(&pr.title, 50),
                    url: pr.url.clone(),
                    status,
                });
            }
            self.opts.tui_tx.send(TuiEvent::ItemsSummary {
                workflow_name: self.config.name.clone(),
                items: summaries,
            });
        }

        if to_respond.is_empty() {
            let result = make_skipped_result(&self.config.name, &run_id, &started_at, None);
            self.opts.store.append(&result).await?;
            return Ok(result);
        }

        let mut last_result: Option<WorkflowResult> = None;
        for item in to_respond {
            let result = self
                .run_single_pr_response(&item.pr, item.inline, item.timeline, &reviewer_login)
                .await?;
            self.opts.store.append(&result).await?;

            if result.success && !result.skipped {
                let all_inline =
                    fetch_inline_review_comments(&item.pr.repo, item.pr.number, None)
                        .await
                        .unwrap_or_default();
                let all_timeline =
                    fetch_pr_comments(&item.pr.repo, item.pr.number, None)
                        .await
                        .unwrap_or_default();
                let last_inline_id = all_inline.iter().map(|c| c.id).max().unwrap_or(0);
                let last_timeline_id = all_timeline.iter().map(|c| c.id).max().unwrap_or(0);

                self.pr_response_store
                    .mark_responded(
                        &self.config.name,
                        PrResponseRecord {
                            pr_number: item.pr.number,
                            pr_url: item.pr.url.clone(),
                            pr_repo: item.pr.repo.clone(),
                            head_sha: result
                                .pr_response_result
                                .as_ref()
                                .map(|r| r.new_head_sha.clone())
                                .unwrap_or_default(),
                            last_inline_comment_id: last_inline_id,
                            last_timeline_comment_id: last_timeline_id,
                            responded_at: Utc::now().to_rfc3339(),
                        },
                    )
                    .await?;
            }
            last_result = Some(result);
        }

        Ok(last_result.unwrap_or_else(|| {
            make_skipped_result(&self.config.name, &run_id, &started_at, None)
        }))
    }

    async fn run_single_pr_response(
        &self,
        pr: &GitHubPR,
        inline: Vec<InlineReviewComment>,
        timeline: Vec<PRComment>,
        reviewer_login: &str,
    ) -> Result<WorkflowResult> {
        if self.config.agent_loop.technique != "claude-loop" {
            return Ok(WorkflowResult {
                workflow_name: self.config.name.clone(),
                run_id: Uuid::new_v4().to_string(),
                started_at: Utc::now().to_rfc3339(),
                completed_at: Utc::now().to_rfc3339(),
                success: false,
                skipped: false,
                error: Some("githubPRResponses workflows require agentLoop.technique: \"claude-loop\".\n\n\
                     Fix: set agent_loop.technique to \"claude-loop\".".to_string()),
                pr_number: Some(pr.number),
                retry_count: 0,
                review_rounds: 0,
                trajectory: vec![],
                ..Default::default()
            });
        }

        let run_id = Uuid::new_v4().to_string();
        let started_at = Utc::now().to_rfc3339();
        let logger = Logger::new(format!("{}#{}", self.config.name, pr.number));

        let parsed_task = ParsedTask::new(format!(
            "Address review comments on PR #{}: {}",
            pr.number, pr.title
        ));
        let log_label = format!("pr-{}-response", pr.number);
        let wf_log = WorkflowLog::with_tui_sender_and_label(
            &self.opts.harness_root.clone().unwrap_or_else(|| PathBuf::from(".")),
            &self.config.name,
            &run_id,
            Some(&log_label),
            self.opts.tui_tx.clone(),
        ).await.ok();
        let mut ctx = self.make_base_ctx(&run_id, parsed_task, 0, wf_log.clone());
        ctx.responding_pr = Some(pr.clone());
        ctx.unaddressed_comments = Some(UnaddressedComments {
            inline: inline.clone(),
            timeline: timeline.clone(),
        });
        ctx.reviewer_login = Some(reviewer_login.to_string());
        ctx.branch = pr.head_ref_name.clone();

        // Render the pr-comment-response prompt.
        let loader = PromptLoader::new(&ctx.harness_root);
        let mut vars = HashMap::new();
        vars.insert("pr_title".to_string(), pr.title.clone());
        vars.insert("pr_author".to_string(), pr.author.login.clone());
        vars.insert("pr_head_ref".to_string(), pr.head_ref_name.clone());
        vars.insert("pr_base_ref".to_string(), pr.base_ref_name.clone());
        vars.insert("pr_url".to_string(), pr.url.clone());
        vars.insert(
            "inline_comments".to_string(),
            render_inline_comments(&inline),
        );
        vars.insert(
            "timeline_comments".to_string(),
            render_timeline_comments(&timeline),
        );
        if let Ok(rendered) = loader
            .load(&ctx.prompts.pr_comment_response, &vars)
            .await
        {
            ctx.parsed_task.task = rendered;
        }

        self.opts.tui_tx.send(TuiEvent::RunStarted {
            workflow_name: self.config.name.clone(),
            run_id: run_id.clone(),
            mode: WorkflowMode::PrResponse,
            item_label: Some(format!("PR #{}", pr.number)),
        });

        let result = async {
            if !self.opts.no_workspace {
                let ws = WorkspaceManager::new(
                    self.config.workspace.clone().unwrap_or_default(),
                    logger.clone(),
                    self.opts.target_repo.clone(),
                    self.opts.git_method.as_deref().unwrap_or("https"),
                );
                let info = ws.setup_for_pr_review(pr, &run_id).await?;
                ctx.workspace_dir = info.dir;
                ctx.branch = info.branch;
            }
            self.run_stage(&AgentLoopStage, &mut ctx).await?;
            self.run_stage(&ReviewFeedbackLoopStage, &mut ctx).await?;
            self.run_stage(&PullRequestStage, &mut ctx).await?;
            self.run_stage(&PrCommentResponderStage, &mut ctx).await?;
            Ok::<_, anyhow::Error>(())
        }
        .await;

        let completed_at = Utc::now().to_rfc3339();

        match result {
            Ok(_) => {
                if let Some(ref log) = wf_log {
                    let _ = log.complete("success").await;
                }
                let trajectory = ctx.agent_result.as_ref()
                    .map(|r| r.trajectory.clone())
                    .unwrap_or_default();
                Ok(WorkflowResult {
                    workflow_name: self.config.name.clone(),
                    run_id,
                    started_at,
                    completed_at,
                    success: true,
                    skipped: false,
                    pr_url: ctx.pr_url,
                    pr_number: Some(pr.number),
                    pr_response_result: ctx.pr_response_result,
                    agent_result: ctx.agent_result,
                    review_result: ctx.review_result,
                    retry_count: 0,
                    review_rounds: ctx.review_rounds,
                    trajectory,
                    ..Default::default()
                })
            }
            Err(e) => {
                if let Some(ref log) = wf_log {
                    let _ = log.error("executor", &e.to_string()).await;
                    let _ = log.complete("failed").await;
                }
                Ok(WorkflowResult {
                    workflow_name: self.config.name.clone(),
                    run_id,
                    started_at,
                    completed_at,
                    success: false,
                    skipped: false,
                    error: Some(e.to_string()),
                    pr_number: Some(pr.number),
                    retry_count: 0,
                    review_rounds: 0,
                    trajectory: vec![],
                    ..Default::default()
                })
            }
        }
    }

    // ── Standard mode (no GitHub trigger) ────────────────────────────────────

    async fn run_standard_mode(&self) -> Result<WorkflowResult> {
        let run_id = Uuid::new_v4().to_string();
        let started_at = Utc::now().to_rfc3339();
        let logger = Logger::new(&self.config.name);

        let parsed_task = ParsedTask::new("standard-workflow-task");
        let wf_log = WorkflowLog::with_tui_sender(
            &self.opts.harness_root.clone().unwrap_or_else(|| PathBuf::from(".")),
            &self.config.name,
            &run_id,
            self.opts.tui_tx.clone(),
        ).await.ok();
        let mut ctx = self.make_base_ctx(&run_id, parsed_task, 0, wf_log.clone());

        self.opts.tui_tx.send(TuiEvent::RunStarted {
            workflow_name: self.config.name.clone(),
            run_id: run_id.clone(),
            mode: WorkflowMode::Standard,
            item_label: None,
        });

        let result = async {
            if !self.opts.no_workspace {
                let ws = WorkspaceManager::new(
                    self.config.workspace.clone().unwrap_or_default(),
                    logger.clone(),
                    self.opts.target_repo.clone(),
                    self.opts.git_method.as_deref().unwrap_or("https"),
                );
                let info = ws.setup(&run_id, None).await?;
                ctx.workspace_dir = info.dir.clone();
                ctx.branch = info.branch.clone();
            }
            self.run_stage(&AgentLoopStage, &mut ctx).await?;
            self.run_stage(&ReviewFeedbackLoopStage, &mut ctx).await?;
            self.run_stage(&PrDescriptionStage, &mut ctx).await?;
            self.run_stage(&PullRequestStage, &mut ctx).await?;
            Ok::<_, anyhow::Error>(())
        }
        .await;

        let completed_at = Utc::now().to_rfc3339();

        match result {
            Ok(_) => {
                if let Some(ref log) = wf_log {
                    let _ = log.complete("success").await;
                }
                let trajectory = ctx.agent_result.as_ref()
                    .map(|r| r.trajectory.clone())
                    .unwrap_or_default();
                Ok(WorkflowResult {
                    workflow_name: self.config.name.clone(),
                    run_id,
                    started_at,
                    completed_at,
                    success: true,
                    skipped: false,
                    pr_url: ctx.pr_url,
                    pr_title: ctx.pr_title,
                    agent_result: ctx.agent_result,
                    review_result: ctx.review_result,
                    retry_count: 0,
                    review_rounds: ctx.review_rounds,
                    trajectory,
                    ..Default::default()
                })
            }
            Err(e) => {
                if let Some(ref log) = wf_log {
                    let _ = log.error("executor", &e.to_string()).await;
                    let _ = log.complete("failed").await;
                }
                Ok(WorkflowResult {
                    workflow_name: self.config.name.clone(),
                    run_id,
                    started_at,
                    completed_at,
                    success: false,
                    skipped: false,
                    error: Some(e.to_string()),
                    retry_count: 0,
                    review_rounds: 0,
                    trajectory: vec![],
                    ..Default::default()
                })
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Rendering helpers
// ---------------------------------------------------------------------------

/// Render a list of inline review comments into a Markdown-style summary.
fn render_inline_comments(comments: &[InlineReviewComment]) -> String {
    if comments.is_empty() {
        return "(none)".to_string();
    }
    comments
        .iter()
        .map(|c| {
            let hunk_lines: Vec<&str> = c
                .diff_hunk
                .as_deref()
                .unwrap_or("")
                .lines()
                .filter(|l| !l.starts_with("@@"))
                .collect();
            let hunk_excerpt = hunk_lines
                .iter()
                .rev()
                .take(3)
                .rev()
                .cloned()
                .collect::<Vec<_>>()
                .join("\n");
            let hunk_part = if hunk_excerpt.is_empty() {
                String::new()
            } else {
                format!("> ```diff\n{}\n```\n", hunk_excerpt)
            };
            format!(
                "**`{}` line {}** — @{}:\n{}> {}",
                c.path,
                c.line.unwrap_or(0),
                c.login,
                hunk_part,
                c.body
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Render a list of timeline (issue-level) comments into plain-text.
fn render_timeline_comments(comments: &[PRComment]) -> String {
    if comments.is_empty() {
        return "(none)".to_string();
    }
    comments
        .iter()
        .map(|c| format!("@{}: {}", c.login, c.body))
        .collect::<Vec<_>>()
        .join("\n\n")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflows::github_prs::{InlineReviewComment, PRComment};

    #[test]
    fn test_render_inline_comments_empty() {
        assert_eq!(render_inline_comments(&[]), "(none)");
    }

    #[test]
    fn test_render_timeline_comments_empty() {
        assert_eq!(render_timeline_comments(&[]), "(none)");
    }

    #[test]
    fn test_render_inline_comments_one() {
        let comment = InlineReviewComment {
            id: 1,
            login: "alice".into(),
            body: "Fix this".into(),
            path: "src/foo.rs".into(),
            line: Some(42),
            diff_hunk: None,
            created_at: "".into(),
            in_reply_to_id: None,
        };
        let result = render_inline_comments(&[comment]);
        assert!(result.contains("src/foo.rs"));
        assert!(result.contains("42"));
        assert!(result.contains("alice"));
        assert!(result.contains("Fix this"));
    }

    #[test]
    fn test_render_inline_comments_with_hunk() {
        let comment = InlineReviewComment {
            id: 2,
            login: "bob".into(),
            body: "See hunk".into(),
            path: "lib.rs".into(),
            line: Some(10),
            diff_hunk: Some("@@ -1,3 +1,4 @@\n context\n-old\n+new".into()),
            created_at: "".into(),
            in_reply_to_id: None,
        };
        let result = render_inline_comments(&[comment]);
        assert!(result.contains("```diff"));
        assert!(result.contains("context"));
    }

    #[test]
    fn test_render_timeline_comments_one() {
        let comment = PRComment {
            id: 1,
            login: "bob".into(),
            body: "LGTM".into(),
            created_at: "".into(),
        };
        let result = render_timeline_comments(&[comment]);
        assert_eq!(result, "@bob: LGTM");
    }

    #[test]
    fn test_render_timeline_comments_multiple() {
        let comments = vec![
            PRComment {
                id: 1,
                login: "alice".into(),
                body: "Nice".into(),
                created_at: "".into(),
            },
            PRComment {
                id: 2,
                login: "bob".into(),
                body: "LGTM".into(),
                created_at: "".into(),
            },
        ];
        let result = render_timeline_comments(&comments);
        assert!(result.contains("@alice: Nice"));
        assert!(result.contains("@bob: LGTM"));
    }

    // ── resolve_system_prompt ──────────────────────────────────────────────

    #[test]
    fn test_resolve_system_prompt_default_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let prompts_dir = dir.path().join("prompts");
        std::fs::create_dir_all(&prompts_dir).unwrap();
        std::fs::write(
            prompts_dir.join("system.md"),
            "You are an agent.\n{{blocked_commands}}\n",
        )
        .unwrap();

        let config = crate::types::config::HarnessConfig {
            blocked_commands: vec!["rm -rf /".into(), "docker system prune".into()],
            ..Default::default()
        };

        let result = resolve_system_prompt(&config, dir.path());
        assert!(result.is_some());
        let text = result.unwrap();
        assert!(text.contains("You are an agent."));
        assert!(text.contains("rm -rf /"));
        assert!(text.contains("docker system prune"));
        assert!(text.contains("NEVER"));
    }

    #[test]
    fn test_resolve_system_prompt_inline() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = crate::types::config::HarnessConfig {
            system_prompt: Some("Custom system prompt.\n{{blocked_commands}}".into()),
            blocked_commands: vec!["shutdown -h now".into()],
            ..Default::default()
        };

        let result = resolve_system_prompt(&config, dir.path());
        assert!(result.is_some());
        let text = result.unwrap();
        assert!(text.contains("Custom system prompt."));
        assert!(text.contains("shutdown -h now"));
    }

    #[test]
    fn test_resolve_system_prompt_no_blocked_commands() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = crate::types::config::HarnessConfig {
            system_prompt: Some("Just a prompt.\n{{blocked_commands}}".into()),
            ..Default::default()
        };

        let result = resolve_system_prompt(&config, dir.path());
        let text = result.unwrap();
        assert!(text.contains("Just a prompt."));
        // No blocked commands section rendered.
        assert!(!text.contains("NEVER"));
    }

    #[test]
    fn test_resolve_system_prompt_no_config_no_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = crate::types::config::HarnessConfig::default();
        let result = resolve_system_prompt(&config, dir.path());
        assert!(result.is_none());
    }

    #[test]
    fn test_resolve_system_prompt_file_path_in_config() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("my-system.md"), "Custom file content.").unwrap();

        let config = crate::types::config::HarnessConfig {
            system_prompt: Some("my-system.md".into()),
            ..Default::default()
        };

        let result = resolve_system_prompt(&config, dir.path());
        assert!(result.is_some());
        assert!(result.unwrap().contains("Custom file content."));
    }

    #[test]
    fn test_render_inline_comments_no_line() {
        let comment = InlineReviewComment {
            id: 3,
            login: "carol".into(),
            body: "Typo".into(),
            path: "main.rs".into(),
            line: None,
            diff_hunk: None,
            created_at: "".into(),
            in_reply_to_id: None,
        };
        let result = render_inline_comments(&[comment]);
        assert!(result.contains("main.rs"));
        assert!(result.contains("line 0"));
    }

    // ── truncate_title ────────────────────────────────────────────────────

    #[test]
    fn test_truncate_title_short() {
        assert_eq!(truncate_title("hello", 10), "hello");
    }

    #[test]
    fn test_truncate_title_long() {
        assert_eq!(truncate_title("a very long title indeed", 10), "a very lo…");
    }

    #[test]
    fn test_truncate_title_exact() {
        assert_eq!(truncate_title("exactly10!", 10), "exactly10!");
    }

    // ── IssueItem conversions ─────────────────────────────────────────────

    #[test]
    fn test_issue_item_from_github() {
        let gh_issue = crate::workflows::github_issues::GitHubIssue {
            number: 42,
            title: "Fix the bug".into(),
            body: Some("It's broken".into()),
            url: "https://github.com/o/r/issues/42".into(),
            repo: "o/r".into(),
            labels: vec![],
            assignees: vec![],
            created_at: "".into(),
            updated_at: "".into(),
            state: "OPEN".into(),
        };
        let item = IssueItem::from_github(&gh_issue);
        assert_eq!(item.number, 42);
        assert_eq!(item.display_id, "#42");
        assert_eq!(item.title, "Fix the bug");
        assert_eq!(item.body, "It's broken");
        assert_eq!(item.url, "https://github.com/o/r/issues/42");
        assert_eq!(item.repo, "o/r");
    }

    #[test]
    fn test_issue_item_from_linear() {
        let linear_issue = crate::workflows::linear_issues::LinearIssue {
            identifier: "ENG-42".into(),
            number: 42,
            title: "Fix the bug".into(),
            description: Some("It's broken".into()),
            url: "https://linear.app/t/ENG-42".into(),
            team_key: "ENG".into(),
            state: "Todo".into(),
            labels: vec![],
            assignee: None,
        };
        let item = IssueItem::from_linear(&linear_issue, "owner/repo");
        assert_eq!(item.number, 42);
        assert_eq!(item.display_id, "ENG-42");
        assert_eq!(item.title, "Fix the bug");
        assert_eq!(item.body, "It's broken");
        assert_eq!(item.repo, "owner/repo");
    }
}
