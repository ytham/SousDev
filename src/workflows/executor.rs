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
    fetch_review_inline_comments, FetchPRsOptions, GitHubPR, InlineReviewComment, PRComment,
};
use crate::workflows::stores::plan_state;
use crate::workflows::workflow::{make_skipped_result, ParsedTask};

/// Truncate a string for info panel display.
fn truncate_title(s: &str, max: usize) -> String {
    crate::utils::truncate::safe_truncate(s, max)
}
use crate::workflows::stage::{ResolvedPrompts, Stage, StageContext, UnaddressedComments};
use crate::workflows::multi_review::{
    self, ReviewerModel, disallowed_tools_for, format_reviews_for_consolidation,
    load_workspace_conventions,
};
use crate::workflows::stages::agent_loop::AgentLoopStage;
use crate::workflows::stages::external_agent_loop::{
    claude_adapter, codex_adapter, gemini_adapter, run_external_agent_loop,
    ExternalAgentRunOptions,
};
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
use crate::types::technique::RunResult;
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
    /// All configured models (for multi-model review routing).
    pub models: Vec<crate::types::config::ModelConfig>,
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

    /// Lightweight refresh: fetch items and emit `ItemsSummary` without
    /// processing anything.  Called on every cron tick (even when a previous
    /// run is still active) so the Info pane stays up-to-date.
    pub async fn refresh_info_only(&self) {
        if let Some(ref issues_cfg) = self.config.github_issues {
            let gh_repo = issues_cfg
                .repo
                .clone()
                .or_else(|| repo_to_gh_identifier(self.opts.target_repo.as_deref()));
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
                let is_handled = self
                    .issue_store
                    .is_handled(&self.config.name, item.number)
                    .await
                    .unwrap_or(false);
                let in_cooldown = self
                    .failure_store
                    .is_in_cooldown(&self.config.name, &item.number.to_string())
                    .await
                    .unwrap_or(false);
                let status = if in_cooldown {
                    ItemStatus::Cooldown
                } else if is_handled {
                    // Check plan-first state for richer status display.
                    let record = self
                        .issue_store
                        .get_record(&self.config.name, item.number)
                        .await
                        .ok()
                        .flatten();
                    match record.as_ref().and_then(|r| r.state.as_deref()) {
                        Some(plan_state::PLAN_POSTED) => ItemStatus::PlanPending,
                        Some(plan_state::PLAN_APPROVED) => ItemStatus::InProgress,
                        Some(plan_state::CODE_COMPLETE) => ItemStatus::Success,
                        _ => ItemStatus::Success, // Legacy (no state) = completed
                    }
                } else {
                    ItemStatus::None
                };
                summaries.push(ItemSummary {
                    id: format!("#{}", item.number),
                    title: truncate_title(&item.title, 50),
                    url: item.url.clone(),
                    status,
                    comment_count: 0,
                });
            }
            self.opts.tui_tx.send(TuiEvent::ItemsSummary {
                workflow_name: self.config.name.clone(),
                items: summaries,
            });
        } else if let Some(ref prs_cfg) = self.config.github_prs {
            let gh_repo = prs_cfg
                .repo
                .clone()
                .or_else(|| repo_to_gh_identifier(self.opts.target_repo.as_deref()));
            let prs = fetch_github_prs(&FetchPRsOptions {
                repo: gh_repo,
                search: prs_cfg.search.clone(),
                limit: prs_cfg.limit,
                raw_search: false,
            })
            .await
            .unwrap_or_default();
            // Apply the individual-reviewer/assignee/reviewed filter.
            // A PR is included if the user is a requested reviewer, an
            // assignee, OR has already reviewed it (review record exists).
            let reviewer_login = self.get_reviewer_login().await.unwrap_or_default();
            let mut summaries: Vec<ItemSummary> = Vec::new();
            for pr in &prs {
                let individually_requested = pr
                    .requested_reviewers
                    .iter()
                    .any(|r| r == &reviewer_login);
                let is_assignee = pr.assignees.iter().any(|a| a == &reviewer_login);
                let review_record = self
                    .pr_review_store
                    .get_record(&self.config.name, pr.number)
                    .await
                    .ok()
                    .flatten();
                let has_review_record = review_record.is_some();

                if !individually_requested && !is_assignee && !has_review_record {
                    continue;
                }

                let pr_key = format!("pr-{}", pr.number);
                let in_cooldown = self
                    .failure_store
                    .is_in_cooldown(&self.config.name, &pr_key)
                    .await
                    .unwrap_or(false);
                let pr_approved = pr.review_decision == "APPROVED";
                let status = if in_cooldown {
                    ItemStatus::Cooldown
                } else if pr_approved {
                    ItemStatus::Approved
                } else if let Some(ref rec) = review_record {
                    if rec.has_concerns { ItemStatus::ReviewedConcerns } else { ItemStatus::ReviewedApproved }
                } else {
                    ItemStatus::None
                };
                summaries.push(ItemSummary {
                    id: format!("PR #{}", pr.number),
                    title: truncate_title(&pr.title, 50),
                    url: pr.url.clone(),
                    status,
                    comment_count: 0,
                });
            }
            self.opts.tui_tx.send(TuiEvent::ItemsSummary {
                workflow_name: self.config.name.clone(),
                items: summaries,
            });
        } else if let Some(ref resp_cfg) = self.config.github_pr_responses {
            let gh_repo = resp_cfg
                .repo
                .clone()
                .or_else(|| repo_to_gh_identifier(self.opts.target_repo.as_deref()));
            let search = resp_cfg.search.clone().unwrap_or_else(|| "author:@me".into());
            let prs = fetch_github_prs(&FetchPRsOptions {
                repo: gh_repo,
                search: Some(search),
                limit: resp_cfg.limit,
                raw_search: true,
            })
            .await
            .unwrap_or_default();
            let summaries: Vec<ItemSummary> = prs
                .iter()
                .map(|pr| ItemSummary {
                    id: format!("PR #{}", pr.number),
                    title: truncate_title(&pr.title, 50),
                    url: pr.url.clone(),
                    status: ItemStatus::None,
                    comment_count: 0,
                })
                .collect();
            self.opts.tui_tx.send(TuiEvent::ItemsSummary {
                workflow_name: self.config.name.clone(),
                items: summaries,
            });
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
            extra_agent_flags: None,
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
                let status = if in_cooldown {
                    ItemStatus::Cooldown
                } else if is_handled {
                    // Check plan-first state for richer status display.
                    let record = self
                        .issue_store
                        .get_record(&self.config.name, item.number)
                        .await
                        .ok()
                        .flatten();
                    match record.as_ref().and_then(|r| r.state.as_deref()) {
                        Some(plan_state::PLAN_POSTED) => ItemStatus::PlanPending,
                        Some(plan_state::PLAN_APPROVED) => ItemStatus::InProgress,
                        Some(plan_state::CODE_COMPLETE) => ItemStatus::Success,
                        _ => ItemStatus::Success, // Legacy (no state) = completed
                    }
                } else {
                    ItemStatus::None
                };
                summaries.push(ItemSummary {
                    id: item.display_id.clone(),
                    title: truncate_title(&item.title, 50),
                    url: item.url.clone(),
                    status,
                    comment_count: 0,
                });
            }
            self.opts.tui_tx.send(TuiEvent::ItemsSummary {
                workflow_name: self.config.name.clone(),
                items: summaries,
            });
        }

        let plan_first = self
            .config
            .github_issues
            .as_ref()
            .and_then(|c| c.plan_first)
            .unwrap_or(true);

        // ── Phase 2 & 3: Plan polling and execution (BEFORE early return) ──
        // These must run even when there are no new unhandled issues, since
        // plan_posted issues need to be polled for approval and plan_approved
        // issues need to be executed.
        if plan_first {
            let plan_posted = self
                .issue_store
                .get_issues_in_state(&self.config.name, plan_state::PLAN_POSTED)
                .await
                .unwrap_or_default();
            if !plan_posted.is_empty() {
                self.opts.tui_tx.send(TuiEvent::LogMessage {
                    workflow_name: self.config.name.clone(),
                    level: "info".to_string(),
                    stage: "plan-approval".to_string(),
                    message: format!(
                        "Found {} issue(s) in plan_posted state",
                        plan_posted.len()
                    ),
                    run_id: String::new(),
                });
            }
            for (number, record) in plan_posted {
                logger.info(&format!("Polling plan approval for issue #{}", number));
                self.opts.tui_tx.send(TuiEvent::LogMessage {
                    workflow_name: self.config.name.clone(),
                    level: "info".to_string(),
                    stage: "plan-approval".to_string(),
                    message: format!(
                        "Polling plan approval for issue #{} (PR: {})",
                        number,
                        record.pr_url.as_deref().unwrap_or("?")
                    ),
                    run_id: String::new(),
                });
                if let Err(e) = self.poll_plan_approval(number, &record).await {
                    let error_msg = format!("Plan poll for issue #{} failed: {}", number, e);
                    logger.error(&error_msg);
                    self.opts.tui_tx.send(TuiEvent::LogMessage {
                        workflow_name: self.config.name.clone(),
                        level: "error".to_string(),
                        stage: "plan-approval".to_string(),
                        message: error_msg,
                        run_id: String::new(),
                    });
                }
            }
        }

        if plan_first {
            let plan_approved = self
                .issue_store
                .get_issues_in_state(&self.config.name, plan_state::PLAN_APPROVED)
                .await
                .unwrap_or_default();
            for (number, record) in plan_approved {
                if let Err(e) = self.run_plan_execution(number, &record).await {
                    logger.error(&format!(
                        "Plan execution for issue #{} failed: {}",
                        number, e
                    ));
                }
            }
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
            let result = if plan_first {
                self.run_plan_generation(item).await
            } else {
                self.run_single_issue(item).await
            };
            let result = match result {
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
                // In plan-first mode, run_plan_generation() already wrote the
                // handled-issue record with state="plan_posted".  Do NOT
                // overwrite it here — that would lose the plan state.
                if !plan_first {
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
                                    state: None,
                                    branch: None,
                                    approved_by: None,
                                },
                            )
                            .await?;
                    }
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

        // (Phase 2 & 3 already handled above, before the early return.)

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

    // ── Plan-first helpers ─────────────────────────────────────────────────────

    /// Load a prompt template from `prompts/{name}.md` and replace
    /// `{{key}}` placeholders with the provided values.
    fn load_and_render_prompt(&self, name: &str, vars: &[(&str, &str)]) -> String {
        let prompts_dir = self
            .opts
            .harness_root
            .as_ref()
            .map(|r| r.join("prompts"))
            .unwrap_or_else(|| PathBuf::from("prompts"));
        let path = prompts_dir.join(format!("{}.md", name));
        let template = std::fs::read_to_string(&path)
            .unwrap_or_else(|_| format!("(prompt template '{}' not found)", name));
        let mut result = template;
        for (key, value) in vars {
            result = result.replace(&format!("{{{{{}}}}}", key), value);
        }
        result
    }

    /// Plan-first mode: generate a plan file and open a plan PR.
    ///
    /// The agent reads the codebase and creates `tmp/plan-issue-{N}.md`,
    /// then we open a PR with the title `Plan(issue #{N}): {title}`.
    /// The issue is marked as `plan_posted` in the store.
    async fn run_plan_generation(&self, issue: &IssueItem) -> Result<WorkflowResult> {
        let run_id = Uuid::new_v4().to_string();
        let started_at = Utc::now().to_rfc3339();
        let logger = Logger::new(format!("{} {}", self.config.name, issue.display_id));

        // Build the plan-generation prompt from the template.
        let plan_prompt = self.load_and_render_prompt(
            "plan-generation",
            &[
                ("issue_number", &issue.number.to_string()),
                ("issue_title", &issue.title),
                ("issue_body", &issue.body),
            ],
        );

        let parsed_task = ParsedTask::new(plan_prompt);

        let log_label = format!("plan-issue-{}", issue.number);
        let wf_log = WorkflowLog::with_tui_sender_and_label(
            &self
                .opts
                .harness_root
                .clone()
                .unwrap_or_else(|| PathBuf::from(".")),
            &self.config.name,
            &run_id,
            Some(&log_label),
            self.opts.tui_tx.clone(),
        )
        .await
        .ok();
        let mut ctx = self.make_base_ctx(&run_id, parsed_task, 0, wf_log.clone());
        ctx.issue_url = Some(issue.url.clone());
        ctx.issue_display_id = Some(issue.display_id.clone());

        self.opts.tui_tx.send(TuiEvent::RunStarted {
            workflow_name: self.config.name.clone(),
            run_id: run_id.clone(),
            mode: WorkflowMode::PlanFirstIssues,
            item_label: Some(format!("plan {}", issue.display_id)),
        });

        let result = async {
            // Set up workspace (clone + branch).
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
            }

            // Run the agent to generate the plan file.
            // Emit "plan-generation" stage events (not "agent-loop") so the
            // sidebar shows the correct plan-first stage name.
            self.opts.tui_tx.send(TuiEvent::StageStarted {
                workflow_name: self.config.name.clone(),
                run_id: run_id.clone(),
                stage_name: "plan-generation".to_string(),
            });
            let agent_result = AgentLoopStage.run(&mut ctx).await;
            match &agent_result {
                Ok(()) => {
                    self.opts.tui_tx.send(TuiEvent::StageCompleted {
                        workflow_name: self.config.name.clone(),
                        run_id: run_id.clone(),
                        stage_name: "plan-generation".to_string(),
                    });
                }
                Err(e) => {
                    self.opts.tui_tx.send(TuiEvent::StageFailed {
                        workflow_name: self.config.name.clone(),
                        run_id: run_id.clone(),
                        stage_name: "plan-generation".to_string(),
                        error: e.to_string(),
                    });
                }
            }
            agent_result?;

            // Verify the plan file was created.
            let plan_path = ctx
                .workspace_dir
                .join(format!("tmp/plan-issue-{}.md", issue.number));
            if !plan_path.exists() {
                return Err(anyhow::anyhow!(
                    "Agent did not create plan file at {}",
                    plan_path.display()
                ));
            }

            // Set PR title and body for the plan PR.
            let plan_title = format!("Plan(issue #{}): {}", issue.number, issue.title);
            ctx.pr_title = Some(plan_title);

            // Render the plan PR body from the template.
            let plan_body = self.load_and_render_prompt(
                "plan-pr-body",
                &[
                    ("issue_url", &issue.url),
                    ("issue_number", &issue.number.to_string()),
                ],
            );
            ctx.pr_generated_body = Some(plan_body);

            // Push and create the PR (no stage event — this is part of
            // plan-generation, not a separate sidebar stage).
            PullRequestStage.run(&mut ctx).await?;

            Ok::<_, anyhow::Error>(())
        }
        .await;

        let completed_at = Utc::now().to_rfc3339();

        match result {
            Ok(_) => {
                if let Some(ref log) = wf_log {
                    let _ = log.complete("success").await;
                }

                // Extract PR number from URL if possible.
                let pr_number = ctx.pr_url.as_ref().and_then(|url| {
                    url.split('/').next_back().and_then(|s| s.parse::<u64>().ok())
                });

                // Mark issue as plan_posted.
                if ctx.pr_url.is_some() {
                    self.issue_store
                        .mark_handled_with_number(
                            &self.config.name,
                            issue.number,
                            HandledIssueRecord {
                                pr_number,
                                issue_url: issue.url.clone(),
                                issue_title: issue.title.clone(),
                                issue_repo: issue.repo.clone(),
                                pr_url: ctx.pr_url.clone(),
                                pr_open: true,
                                handled_at: Utc::now().to_rfc3339(),
                                updated_at: Utc::now().to_rfc3339(),
                                state: Some(plan_state::PLAN_POSTED.to_string()),
                                branch: Some(ctx.branch.clone()),
                                approved_by: None,
                            },
                        )
                        .await?;
                }

                self.opts.tui_tx.send(TuiEvent::RunCompleted {
                    workflow_name: self.config.name.clone(),
                    run_id: run_id.clone(),
                    success: true,
                    skipped: false,
                    error: None,
                    pr_url: ctx.pr_url.clone(),
                });

                Ok(WorkflowResult {
                    workflow_name: self.config.name.clone(),
                    run_id,
                    started_at,
                    completed_at,
                    success: true,
                    skipped: false,
                    pr_url: ctx.pr_url,
                    pr_title: ctx.pr_title,
                    issue_number: Some(issue.number),
                    retry_count: 0,
                    review_rounds: 0,
                    trajectory: vec![],
                    ..Default::default()
                })
            }
            Err(e) => {
                if let Some(ref log) = wf_log {
                    let _ = log.complete("error").await;
                }
                let error = e.to_string();
                logger.error(&format!("Plan generation failed: {}", error));

                self.opts.tui_tx.send(TuiEvent::RunCompleted {
                    workflow_name: self.config.name.clone(),
                    run_id: run_id.clone(),
                    success: false,
                    skipped: false,
                    error: Some(error.clone()),
                    pr_url: None,
                });

                Ok(WorkflowResult {
                    workflow_name: self.config.name.clone(),
                    run_id,
                    started_at,
                    completed_at,
                    success: false,
                    skipped: false,
                    pr_url: None,
                    pr_title: None,
                    issue_number: Some(issue.number),
                    retry_count: 0,
                    review_rounds: 0,
                    trajectory: vec![],
                    error: Some(error),
                    ..Default::default()
                })
            }
        }
    }

    /// Poll a plan PR for human approval.
    ///
    /// Checks both PR review bodies and timeline comments for "approved"
    /// (case-insensitive).  When approval is found, incorporates any inline
    /// review comments and addendum text, then transitions the issue to
    /// `plan_approved`.
    async fn poll_plan_approval(
        &self,
        issue_number: u64,
        record: &HandledIssueRecord,
    ) -> Result<()> {
        let logger = Logger::new(format!("{} #{}", self.config.name, issue_number));
        // Try pr_number field first; fall back to extracting from pr_url.
        let pr_number = record.pr_number
            .or_else(|| {
                record.pr_url.as_ref().and_then(|url| {
                    url.split('/').next_back().and_then(|s| s.parse::<u64>().ok())
                })
            })
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "No PR number for plan-posted issue #{} (pr_url: {:?})",
                    issue_number,
                    record.pr_url
                )
            })?;
        let repo = &record.issue_repo;

        logger.info(&format!(
            "Checking PR #{} in {} for approval (state: {:?}, branch: {:?})",
            pr_number, repo, record.state, record.branch
        ));

        // Check if the PR is still open.
        let pr_state =
            crate::workflows::github_prs::fetch_pr_state(repo, pr_number).await;
        if let Ok(state) = &pr_state {
            if state != "open" && state != "OPEN" {
                logger.info(&format!(
                    "Plan PR #{} is {} — treating as rejected",
                    pr_number, state
                ));
                return Ok(());
            }
        }

        // Check PR review bodies for approval.
        let reviews = crate::workflows::github_prs::fetch_pr_review_comments(
            repo, pr_number, None,
        )
        .await
        .unwrap_or_default();

        logger.info(&format!(
            "PR #{}: found {} review(s) with bodies",
            pr_number,
            reviews.len()
        ));
        for r in &reviews {
            logger.debug(&format!(
                "  review by {}: {:?}",
                r.login,
                if r.body.len() > 50 { &r.body[..50] } else { &r.body }
            ));
        }

        let mut approval_review_id: Option<u64> = None;
        let mut addendum: Option<String> = None;
        let mut approved_by: Option<String> = None;

        for review in &reviews {
            if let Some(parsed) = parse_approval(&review.body) {
                approval_review_id = Some(review.id);
                addendum = parsed.addendum;
                approved_by = Some(review.login.clone());
                logger.info(&format!(
                    "Plan PR #{}: approval found in review by {}",
                    pr_number, review.login
                ));
                break;
            }
        }

        // Fallback: check timeline comments.
        if approval_review_id.is_none() {
            let timeline = crate::workflows::github_prs::fetch_pr_comments(
                repo, pr_number, None,
            )
            .await
            .unwrap_or_default();

            for comment in &timeline {
                if is_bot(&comment.login) {
                    continue;
                }
                if let Some(parsed) = parse_approval(&comment.body) {
                    addendum = parsed.addendum;
                    approved_by = Some(comment.login.clone());
                    logger.info(&format!(
                        "Plan PR #{}: approval found in timeline comment by {}",
                        pr_number, comment.login
                    ));
                    // Sentinel: approved but no review ID.
                    approval_review_id = Some(0);
                    break;
                }
            }
        }

        if approval_review_id.is_none() {
            logger.info(&format!(
                "PR #{}: no approval found yet — will check next tick",
                pr_number
            ));
            return Ok(()); // Not yet approved — check next tick.
        }

        // Fetch inline review comments from the approval review.
        let inline_comments = if let Some(review_id) = approval_review_id {
            if review_id > 0 {
                fetch_review_inline_comments(repo, pr_number, review_id)
                    .await
                    .unwrap_or_default()
            } else {
                // Timeline-based approval — fetch ALL inline comments.
                fetch_inline_review_comments(repo, pr_number, None)
                    .await
                    .unwrap_or_default()
            }
        } else {
            vec![]
        };

        // If there are inline comments or an addendum, update the plan file.
        if !inline_comments.is_empty() || addendum.is_some() {
            let branch = record.branch.as_deref().ok_or_else(|| {
                anyhow::anyhow!("No branch for plan-posted issue #{}", issue_number)
            })?;

            // Set up workspace (re-checkout the existing branch).
            let ws = WorkspaceManager::new(
                self.config.workspace.clone().unwrap_or_default(),
                logger.clone(),
                self.opts.target_repo.clone(),
                self.opts.git_method.as_deref().unwrap_or("https"),
            );
            let info = ws.setup_existing(branch, Some(issue_number)).await?;
            let dir = &info.dir;

            let plan_path = dir.join(format!("tmp/plan-issue-{}.md", issue_number));
            if plan_path.exists() {
                let mut plan_content = tokio::fs::read_to_string(&plan_path).await?;

                // Append inline review comments as a new section.
                if !inline_comments.is_empty() {
                    plan_content.push_str("\n\n## Reviewer feedback\n\n");
                    for c in &inline_comments {
                        let line = c.line.unwrap_or(0);
                        plan_content.push_str(&format!(
                            "- **{}:{}** ({}): {}\n",
                            c.path, line, c.login, c.body
                        ));
                    }
                }

                // Append addendum from "approved; also do X".
                if let Some(ref extra) = addendum {
                    plan_content.push_str("\n\n## Additional instructions\n\n");
                    plan_content.push_str(extra);
                    plan_content.push('\n');
                }

                tokio::fs::write(&plan_path, &plan_content).await?;

                // Commit and push the updated plan.
                exec_git(&["add", "-f", "tmp/"], dir).await?;
                let status = exec_git(&["status", "--porcelain"], dir).await?;
                if !status.trim().is_empty() {
                    exec_git(
                        &[
                            "commit",
                            "-m",
                            &format!(
                                "plan: incorporate review feedback for issue #{}",
                                issue_number
                            ),
                        ],
                        dir,
                    )
                    .await?;
                    exec_git(&["push", "origin", branch], dir).await?;
                }
            }
        }

        // Transition to plan_approved.
        self.issue_store
            .update_state_with_approval(
                &self.config.name,
                issue_number,
                plan_state::PLAN_APPROVED,
                approved_by.as_deref(),
            )
            .await?;

        // Mark the "plan-approval" stage as done in the sidebar.
        self.opts.tui_tx.send(TuiEvent::StageCompleted {
            workflow_name: self.config.name.clone(),
            run_id: format!("plan-poll-{}", issue_number),
            stage_name: "plan-approval".to_string(),
        });

        logger.info(&format!(
            "Issue #{}: plan approved — ready for execution",
            issue_number
        ));

        Ok(())
    }

    /// Plan-first mode: execute the approved plan and finalize the PR.
    ///
    /// Reads the plan file, runs the agent with the plan-execution prompt,
    /// runs the internal review loop, posts the plan as a timeline comment,
    /// deletes the plan file, updates the PR title/body, and transitions
    /// to `code_complete`.
    async fn run_plan_execution(
        &self,
        issue_number: u64,
        record: &HandledIssueRecord,
    ) -> Result<()> {
        let run_id = Uuid::new_v4().to_string();
        let logger = Logger::new(format!("{} #{}", self.config.name, issue_number));
        let branch = record.branch.as_deref().ok_or_else(|| {
            anyhow::anyhow!("No branch for plan-approved issue #{}", issue_number)
        })?;
        let pr_number = record.pr_number
            .or_else(|| {
                record.pr_url.as_ref().and_then(|url| {
                    url.split('/').next_back().and_then(|s| s.parse::<u64>().ok())
                })
            })
            .ok_or_else(|| {
                anyhow::anyhow!("No PR number for plan-approved issue #{}", issue_number)
            })?;
        let repo = &record.issue_repo;

        logger.info(&format!(
            "Executing approved plan for issue #{} on branch {}",
            issue_number, branch
        ));

        self.opts.tui_tx.send(TuiEvent::RunStarted {
            workflow_name: self.config.name.clone(),
            run_id: run_id.clone(),
            mode: WorkflowMode::PlanFirstIssues,
            item_label: Some(format!("exec #{}", issue_number)),
        });

        let result = async {
            // Set up workspace (re-checkout the existing branch).
            let ws = WorkspaceManager::new(
                self.config.workspace.clone().unwrap_or_default(),
                logger.clone(),
                self.opts.target_repo.clone(),
                self.opts.git_method.as_deref().unwrap_or("https"),
            );
            let info = ws.setup_existing(branch, Some(issue_number)).await?;

            // Read the approved plan.
            let plan_path = info
                .dir
                .join(format!("tmp/plan-issue-{}.md", issue_number));
            let plan_content = tokio::fs::read_to_string(&plan_path)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to read plan file: {}", e))?;

            // Build the execution prompt.
            let test_command = "cargo test";
            let exec_prompt = self.load_and_render_prompt(
                "plan-execution",
                &[
                    ("issue_number", &issue_number.to_string()),
                    ("issue_title", &record.issue_title),
                    ("issue_body", ""), // Not stored in record — plan has the context
                    ("plan", &plan_content),
                    ("test_command", test_command),
                ],
            );

            let parsed_task = ParsedTask::new(exec_prompt);

            let log_label = format!("exec-issue-{}", issue_number);
            let wf_log = WorkflowLog::with_tui_sender_and_label(
                &self
                    .opts
                    .harness_root
                    .clone()
                    .unwrap_or_else(|| PathBuf::from(".")),
                &self.config.name,
                &run_id,
                Some(&log_label),
                self.opts.tui_tx.clone(),
            )
            .await
            .ok();
            let mut ctx = self.make_base_ctx(&run_id, parsed_task, 0, wf_log.clone());
            ctx.workspace_dir = info.dir.clone();
            ctx.branch = branch.to_string();
            ctx.issue_url = Some(record.issue_url.clone());
            ctx.issue_display_id = Some(format!("#{}", issue_number));

            // Run the agent to execute the plan.
            self.run_stage(&AgentLoopStage, &mut ctx).await?;

            // Commit any uncommitted changes left by the agent (e.g. if the
            // agent timed out before running git commit).
            let status = exec_git(&["status", "--porcelain"], &info.dir).await?;
            if !status.trim().is_empty() {
                logger.info("Committing uncommitted agent changes");
                exec_git(&["add", "-A"], &info.dir).await?;
                let fallback_msg = format!("feat: implement plan for issue #{}", issue_number);
                let commit_msg = ctx.pr_title.as_deref().unwrap_or(&fallback_msg);
                exec_git(&["commit", "-m", commit_msg], &info.dir).await?;
            }

            // Run the internal review loop.
            self.run_stage(&ReviewFeedbackLoopStage, &mut ctx).await?;

            // ── PR update stage ──────────────────────────────────────────
            self.opts.tui_tx.send(TuiEvent::StageStarted {
                workflow_name: self.config.name.clone(),
                run_id: run_id.clone(),
                stage_name: "pr-update".to_string(),
            });

            // Post the plan content as a temporary timeline comment
            // (will be deleted after the PR title/body are updated).
            let approved_by = record.approved_by.as_deref().unwrap_or("a team member");
            let plan_comment = format!(
                "## Implementation plan\n\nThe following implementation plan has been **approved** by @{}:\n\n<details>\n<summary>Click to expand</summary>\n\n{}\n\n</details>",
                approved_by, plan_content
            );
            let plan_comment_id = crate::workflows::github_prs::post_summary_comment(
                repo, pr_number, &plan_comment,
            )
            .await
            .ok();

            // Delete the plan file and commit.
            if plan_path.exists() {
                tokio::fs::remove_file(&plan_path).await?;
                exec_git(&["add", "-A"], &info.dir).await?;
                let status =
                    exec_git(&["status", "--porcelain"], &info.dir).await?;
                if !status.trim().is_empty() {
                    exec_git(
                        &[
                            "commit",
                            "-m",
                            &format!(
                                "chore: remove plan file for issue #{}",
                                issue_number
                            ),
                        ],
                        &info.dir,
                    )
                    .await?;
                }
            }

            // Push all changes.
            exec_git(&["push", "origin", branch], &info.dir).await?;

            // Generate new PR title/body.
            logger.info("Generating PR title/body via PrDescriptionStage");
            if let Err(e) = self.run_stage(&PrDescriptionStage, &mut ctx).await {
                logger.error(&format!("PrDescriptionStage failed: {}", e));
                // Continue anyway — we'll use the issue title as fallback.
            }

            // Update the existing PR title and body.
            let new_title =
                ctx.pr_title.as_deref().unwrap_or(&record.issue_title);
            logger.info(&format!("Updating PR #{} title to: {}", pr_number, new_title));

            let mut edit_args = vec![
                "pr".to_string(),
                "edit".to_string(),
                pr_number.to_string(),
                "--title".to_string(),
                new_title.to_string(),
            ];
            // Keep the body temp file alive until after gh pr edit runs.
            // NamedTempFile deletes on drop, so it must outlive the command.
            let _body_file_handle;
            if let Some(ref body) = ctx.pr_generated_body {
                let body_file = tempfile::NamedTempFile::new()?;
                std::fs::write(body_file.path(), body)?;
                edit_args.push("--body-file".to_string());
                edit_args.push(body_file.path().to_string_lossy().to_string());
                logger.info(&format!("PR body: {} chars", body.len()));
                _body_file_handle = Some(body_file); // keep alive
            } else {
                logger.info("No PR body generated — only updating title");
                _body_file_handle = None;
            }
            // Add --repo flag.
            if let Some(repo_id) =
                crate::workflows::github_issues::repo_to_gh_identifier(
                    self.opts.target_repo.as_deref(),
                )
            {
                edit_args.push("--repo".to_string());
                edit_args.push(repo_id);
            }

            logger.info(&format!("Running: gh {}", edit_args.join(" ")));
            let output = tokio::process::Command::new("gh")
                .args(&edit_args)
                .current_dir(&info.dir)
                .output()
                .await?;
            drop(_body_file_handle); // now safe to delete
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let stdout = String::from_utf8_lossy(&output.stdout);
                logger.error(&format!(
                    "Failed to update PR #{}: stderr={}, stdout={}",
                    pr_number,
                    stderr.trim(),
                    stdout.trim()
                ));
                self.opts.tui_tx.send(TuiEvent::LogMessage {
                    workflow_name: self.config.name.clone(),
                    level: "error".to_string(),
                    stage: "pr-update".to_string(),
                    message: format!(
                        "Failed to update PR #{} title/body: {}",
                        pr_number,
                        stderr.trim()
                    ),
                    run_id: run_id.clone(),
                });
            } else {
                logger.info(&format!("PR #{} title/body updated successfully", pr_number));
                // Delete the plan archive comment now that the PR has a proper
                // title and body — the plan content is no longer needed as a
                // separate comment.
                if let Some(pid) = plan_comment_id {
                    if let Err(e) = crate::workflows::github_prs::delete_comment(repo, pid).await {
                        logger.warn(&format!("Could not delete plan comment: {}", e));
                    }
                }
            }

            self.opts.tui_tx.send(TuiEvent::StageCompleted {
                workflow_name: self.config.name.clone(),
                run_id: run_id.clone(),
                stage_name: "pr-update".to_string(),
            });

            if let Some(ref log) = wf_log {
                let _ = log.complete("success").await;
            }

            Ok::<_, anyhow::Error>(())
        }
        .await;

        match result {
            Ok(_) => {
                // Transition to code_complete.
                self.issue_store
                    .update_state(
                        &self.config.name,
                        issue_number,
                        plan_state::CODE_COMPLETE,
                    )
                    .await?;

                logger.info(&format!(
                    "Issue #{}: plan executed, PR updated — code complete",
                    issue_number
                ));

                self.opts.tui_tx.send(TuiEvent::RunCompleted {
                    workflow_name: self.config.name.clone(),
                    run_id: run_id.clone(),
                    success: true,
                    skipped: false,
                    error: None,
                    pr_url: None,
                });
            }
            Err(e) => {
                let error = e.to_string();
                logger.error(&format!(
                    "Plan execution for issue #{} failed: {}",
                    issue_number, error
                ));

                // Record failure for cooldown.
                let item_key = format!("plan-exec-{}", issue_number);
                self.failure_store
                    .record_failure(&self.config.name, &item_key)
                    .await
                    .ok();

                self.opts.tui_tx.send(TuiEvent::RunCompleted {
                    workflow_name: self.config.name.clone(),
                    run_id: run_id.clone(),
                    success: false,
                    skipped: false,
                    error: Some(error),
                    pr_url: None,
                });
            }
        }

        Ok(())
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
        //   2. The user is in the assignees list
        //   3. The user has a review record for this PR (already reviewed it)
        //   4. The assignee filter from config matches
        //
        // PRs that only match via team-level review requests (e.g. "eng" team
        // auto-added to all PRs) are excluded unless the user is also assigned,
        // individually requested, or has already reviewed the PR.
        let mut filtered_prs: Vec<GitHubPR> = Vec::new();
        for pr in prs {
            // Check 1: individually requested as reviewer.
            let individually_requested = pr
                .requested_reviewers
                .iter()
                .any(|r| r == &reviewer_login);

            if individually_requested {
                filtered_prs.push(pr);
                continue;
            }

            // Check 2: assigned to the user.
            if pr.assignees.iter().any(|a| a == &reviewer_login) {
                filtered_prs.push(pr);
                continue;
            }

            // Check 3: already reviewed by the user (review record exists).
            // After submitting a review, GitHub removes the user from
            // reviewRequests, so checks 1 & 2 no longer match. This keeps
            // the PR visible while it's still open.
            let has_review_record = self
                .pr_review_store
                .get_record(&self.config.name, pr.number)
                .await
                .ok()
                .flatten()
                .is_some();
            if has_review_record {
                filtered_prs.push(pr);
                continue;
            }

            // Check 4: assignee filter from config (for additional assignees).
            if let Some(ref allowed) = assignee_filter {
                if pr.assignees.iter().any(|a| allowed.contains(a)) {
                    filtered_prs.push(pr);
                    continue;
                }
            }
        }
        let prs = filtered_prs;

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
                Some(ref rec) => {
                    let new_comments =
                        match fetch_pr_comments(&pr.repo, pr.number, Some(rec.last_comment_id)).await {
                            Ok(comments) => comments,
                            Err(e) => {
                                logger.error(&format!(
                                    "PR #{}: failed to fetch comments for re-review check: {}",
                                    pr.number, e
                                ));
                                vec![]
                            }
                        };
                    // Re-review if:
                    // 1. Any new non-bot, non-self comment exists (PR author
                    //    responding to review feedback), OR
                    // 2. Any comment contains "@sousdev focus:" (focus directive),
                    //    "@sousdev review", or "@<reviewer> review" (explicit
                    //    re-review request) — even from the reviewer themselves.
                    let review_trigger_lower = format!("@{} review", reviewer_login).to_lowercase();
                    let has_new_human_comment = new_comments.iter().any(|c| {
                        if is_bot(&c.login) {
                            return false;
                        }
                        let lower = c.body.to_lowercase();
                        let is_directive = lower.contains("@sousdev focus:")
                            || lower.contains("@sousdev review")
                            || lower.contains(&review_trigger_lower);
                        is_directive || c.login != reviewer_login
                    });
                    if has_new_human_comment {
                        logger.info(&format!(
                            "PR #{} has new human comments — re-reviewing",
                            pr.number
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
                let review_record = self
                    .pr_review_store
                    .get_record(&self.config.name, pr.number)
                    .await
                    .ok()
                    .flatten()
                    ;
                let pr_approved = pr.review_decision == "APPROVED";
                let status = if in_cooldown {
                    ItemStatus::Cooldown
                } else if pr_approved {
                    ItemStatus::Approved
                } else if let Some(ref rec) = review_record {
                    if rec.has_concerns { ItemStatus::ReviewedConcerns } else { ItemStatus::ReviewedApproved }
                } else {
                    ItemStatus::None
                };
                summaries.push(ItemSummary {
                    id: format!("PR #{}", pr.number),
                    title: truncate_title(&pr.title, 50),
                    url: pr.url.clone(),
                    status,
                    comment_count: 0,
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
                            additions: pr.additions,
                            deletions: pr.deletions,
                            has_concerns: result
                                .pr_review_result
                                .as_ref()
                                .map(|r| r.verdict != "approved")
                                .unwrap_or(true),
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
        // Check for multi-model review.  When enabled and multiple CLIs are
        // available, route to the parallel multi-model path instead.
        let multi_model_override = self.config.github_prs.as_ref()
            .and_then(|c| c.multi_model_review);
        if let Some(models) = multi_review::resolve_multi_model(multi_model_override, &self.opts.models).await {
            return self.run_multi_model_review(pr, _reviewer_login, &models).await;
        }

        // Runtime enforcement: must be a CLI-based technique
        let technique = &self.config.agent_loop.technique;
        let is_cli_technique = matches!(technique.as_str(), "claude-loop" | "codex-loop" | "gemini-loop");
        if !is_cli_technique {
            return Ok(WorkflowResult {
                workflow_name: self.config.name.clone(),
                run_id: Uuid::new_v4().to_string(),
                started_at: Utc::now().to_rfc3339(),
                completed_at: Utc::now().to_rfc3339(),
                success: false,
                skipped: false,
                error: Some(format!(
                    "githubPRs workflows require a CLI-based technique \
                     (claude-loop, codex-loop, or gemini-loop).\n\n\
                     The PR review agent must be able to run shell commands (git diff, file reads) \
                     to inspect the PR changes in the workspace. Harness-native techniques (\"{}\") \
                     receive only text and cannot access the workspace filesystem.\n\n\
                     Fix: set agent_loop.technique to a CLI-based technique in your githubPRs workflow config.",
                    technique
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

        // Load the pr-review.md prompt with variable substitution so all
        // models get structured output format instructions and review guidelines.
        let review_prompt = self.load_and_render_prompt(
            "pr-review",
            &[
                ("pr_title", &pr.title),
                ("pr_number", &pr.number.to_string()),
                ("pr_author", &pr.author.login),
                ("pr_head_ref", &pr.head_ref_name),
                ("pr_base_ref", &pr.base_ref_name),
                ("pr_body", pr.body.as_deref().unwrap_or("")),
            ],
        );
        let parsed_task = ParsedTask::new(review_prompt);
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

        // PR review is read-only — use auto permission mode instead of
        // --dangerously-skip-permissions.  The auto mode classifier blocks
        // write operations, and --disallowedTools removes dangerous patterns
        // from the model's context entirely.
        ctx.extra_agent_flags = Some(vec![
            "--permission-mode".to_string(),
            "auto".to_string(),
            "--disallowedTools".to_string(),
            "Bash(gh pr review*)".to_string(),
            "Bash(gh pr comment*)".to_string(),
            "Bash(gh pr approve*)".to_string(),
            "Bash(gh pr merge*)".to_string(),
            "Bash(gh api --method POST*)".to_string(),
            "Bash(gh api --method PUT*)".to_string(),
            "Bash(gh api -X POST*)".to_string(),
            "Bash(gh api -X PUT*)".to_string(),
        ]);

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
                match ws.setup_for_pr_review(pr, &run_id).await {
                    Ok(info) => {
                        ctx.workspace_dir = info.dir;
                        ctx.branch = info.branch;
                    }
                    Err(e) => {
                        let err_str = e.to_string();
                        if err_str.contains("couldn't find remote ref")
                            || err_str.contains("does not exist")
                        {
                            logger.info(&format!(
                                "PR #{}: branch deleted — skipping",
                                pr.number
                            ));
                            self.opts.tui_tx.send(TuiEvent::LogMessage {
                                workflow_name: self.config.name.clone(),
                                level: "warn".to_string(),
                                stage: "pr-checkout".to_string(),
                                message: format!(
                                    "PR #{}: branch '{}' no longer exists — skipping",
                                    pr.number, pr.head_ref_name
                                ),
                                run_id: run_id.clone(),
                            });
                            return Err(anyhow::anyhow!("SKIP:branch_deleted"));
                        }
                        return Err(e);
                    }
                }
            }
            self.run_stage(&PrCheckoutStage, &mut ctx).await?;

            // ── Collect focus directives and post placeholder ─────────
            let focus_directives = collect_focus_directives(
                &pr.repo, pr.number, pr.body.as_deref(), &pr.author.login,
            ).await;
            let focus_prompt_section = format_focus_for_prompt(&focus_directives);

            let show_branding = self.config.pull_request.as_ref()
                .and_then(|pr_cfg| pr_cfg.show_branding)
                .unwrap_or(true);
            let technique = &self.config.agent_loop.technique;
            let model_name = self.opts.models.first()
                .map(|mc| multi_review::model_display_name(&mc.model))
                .unwrap_or_else(|| technique.to_string());
            let placeholder_body = format_review_placeholder(
                &[model_name], &focus_directives, show_branding,
            );
            let placeholder_id = crate::workflows::github_prs::post_summary_comment(
                &pr.repo, pr.number, &placeholder_body,
            ).await.ok();

            // Append focus directives to the task text.
            if !focus_prompt_section.is_empty() {
                let new_task = format!("{}{}", ctx.parsed_task.full_text(), focus_prompt_section);
                ctx.parsed_task = ParsedTask::new(new_task);
            }

            // Prefer API-based review when an API key is available.
            // Falls back to CLI-based AgentLoopStage otherwise.
            // Find the matching model config for the technique's provider and
            // create an API provider from it (uses the correct model ID from config).
            let technique = &self.config.agent_loop.technique;
            let provider_name = match technique.as_str() {
                "claude-loop" => Some("anthropic"),
                "codex-loop" => Some("openai"),
                _ => None,
            };
            let api_model = provider_name
                .and_then(|pn| self.opts.models.iter().find(|mc| mc.provider == pn))
                .and_then(|mc| multi_review::provider_for_model_config(mc));

            if let Some(provider) = api_model {
                logger.info("Using API-based review loop");

                self.opts.tui_tx.send(TuiEvent::StageStarted {
                    workflow_name: self.config.name.clone(),
                    run_id: run_id.clone(),
                    stage_name: "agent-loop".to_string(),
                });

                let conventions = load_workspace_conventions(&ctx.workspace_dir).await;
                let api_system_prompt = if conventions.is_empty() {
                    ctx.system_prompt.clone()
                } else {
                    Some(format!(
                        "{}{}",
                        conventions,
                        ctx.system_prompt.as_deref().unwrap_or("")
                    ))
                };
                let review_registry = crate::workflows::stages::api_review_loop::review_tool_registry(&ctx.workspace_dir);
                let review_result = crate::workflows::stages::api_review_loop::run_api_review_loop(
                    provider.as_ref(),
                    &review_registry,
                    &ctx.parsed_task.full_text(),
                    api_system_prompt.as_deref(),
                    &logger,
                ).await;

                match &review_result {
                    Ok(_) => {
                        self.opts.tui_tx.send(TuiEvent::StageCompleted {
                            workflow_name: self.config.name.clone(),
                            run_id: run_id.clone(),
                            stage_name: "agent-loop".to_string(),
                        });
                    }
                    Err(e) => {
                        self.opts.tui_tx.send(TuiEvent::StageFailed {
                            workflow_name: self.config.name.clone(),
                            run_id: run_id.clone(),
                            stage_name: "agent-loop".to_string(),
                            error: e.to_string(),
                        });
                    }
                }

                let review_text = review_result?;
                ctx.agent_result = Some(RunResult::success(
                    "api-review",
                    review_text,
                    vec![],
                    0,
                    0,
                ));
            } else {
                logger.info("Using CLI-based review (no API key)");
                self.run_stage(&AgentLoopStage, &mut ctx).await?;
            }

            // Delete the placeholder before posting the full review.
            if let Some(pid) = placeholder_id {
                let _ = crate::workflows::github_prs::delete_comment(&pr.repo, pid).await;
            }

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
                let error = e.to_string();
                let is_skip = error.contains("SKIP:branch_deleted");
                if let Some(ref log) = wf_log {
                    if is_skip {
                        let _ = log.complete("skipped").await;
                    } else {
                        let _ = log.error("executor", &error).await;
                        let _ = log.complete("failed").await;
                    }
                }
                Ok(WorkflowResult {
                    workflow_name: self.config.name.clone(),
                    run_id,
                    started_at,
                    completed_at,
                    success: is_skip,
                    skipped: is_skip,
                    error: if is_skip { None } else { Some(error) },
                    pr_number: Some(pr.number),
                    retry_count: 0,
                    review_rounds: 0,
                    trajectory: vec![],
                    ..Default::default()
                })
            }
        }
    }

    // ── Multi-model PR review ──────────────────────────────────────────────

    /// Run multiple AI model CLIs in parallel to review a PR, then
    /// consolidate their outputs into a single timeline comment.
    async fn run_multi_model_review(
        &self,
        pr: &GitHubPR,
        _reviewer_login: &str,
        models: &[ReviewerModel],
    ) -> Result<WorkflowResult> {
        let run_id = Uuid::new_v4().to_string();
        let started_at = Utc::now().to_rfc3339();
        let logger = Logger::new(format!("{}#{}-multi", self.config.name, pr.number));

        logger.info(&format!(
            "Multi-model PR review for #{} with models: {}",
            pr.number,
            models.iter().map(|m| m.name()).collect::<Vec<_>>().join(", ")
        ));

        // Load the pr-review.md prompt with PR variable substitution.
        let review_prompt = self.load_and_render_prompt(
            "pr-review",
            &[
                ("pr_title", &pr.title),
                ("pr_number", &pr.number.to_string()),
                ("pr_author", &pr.author.login),
                ("pr_head_ref", &pr.head_ref_name),
                ("pr_base_ref", &pr.base_ref_name),
                ("pr_body", pr.body.as_deref().unwrap_or("")),
            ],
        );

        let parsed_task = ParsedTask::new(review_prompt.clone());
        let log_label = format!("pr-{}-multi", pr.number);
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
            item_label: Some(format!("PR #{} (multi-model)", pr.number)),
        });

        let result = async {
            // ── Workspace setup & PR checkout ────────────────────────────
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

            // ── Load workspace conventions for non-Claude models ─────────
            let conventions = load_workspace_conventions(&ctx.workspace_dir).await;
            let system_prompt = ctx.system_prompt.clone().unwrap_or_default();

            // ── Collect focus directives ─────────────────────────────────
            let focus_directives = collect_focus_directives(
                &pr.repo, pr.number, pr.body.as_deref(), &pr.author.login,
            ).await;
            if !focus_directives.is_empty() {
                logger.info(&format!(
                    "PR #{}: {} focus directive(s) collected",
                    pr.number, focus_directives.len()
                ));
            }

            // Append focus directives to the review prompt.
            let focus_prompt_section = format_focus_for_prompt(&focus_directives);
            let review_prompt = format!("{}{}", review_prompt, focus_prompt_section);

            // ── Post "review in progress" placeholder ────────────────────
            let show_branding = self.config.pull_request.as_ref()
                .and_then(|pr_cfg| pr_cfg.show_branding)
                .unwrap_or(true);
            let model_display_names: Vec<String> = models.iter().map(|m| {
                let model_id = self.opts.models.iter()
                    .find(|mc| mc.provider == match m {
                        ReviewerModel::Claude => "anthropic",
                        ReviewerModel::Codex => "openai",
                        ReviewerModel::Gemini => "gemini",
                    })
                    .map(|mc| mc.model.as_str())
                    .unwrap_or(m.name());
                multi_review::model_display_name(model_id)
            }).collect();
            let placeholder_body = format_review_placeholder(
                &model_display_names, &focus_directives, show_branding,
            );
            let placeholder_id = match crate::workflows::github_prs::post_summary_comment(
                &pr.repo, pr.number, &placeholder_body,
            ).await {
                Ok(id) => Some(id),
                Err(e) => {
                    logger.warn(&format!("Could not post review placeholder: {}", e));
                    None
                }
            };

            let ext_cfg = self.config.agent_loop.external_agent.clone().unwrap_or_default();
            let workspace_cwd = ctx.workspace_dir.to_string_lossy().to_string();

            // ── Build per-model execution plans ──────────────────────────
            // For each model, prefer the API-based review loop (faster, no CLI
            // dependency) if an API key is available.  Fall back to CLI otherwise.
            let review_registry = crate::workflows::stages::api_review_loop::review_tool_registry(&ctx.workspace_dir);

            enum ReviewMethod {
                Api(std::sync::Arc<dyn crate::providers::provider::LLMProvider>),
                Cli(
                    crate::workflows::stages::external_agent_loop::ExternalAgentAdapter,
                    ExternalAgentRunOptions,
                ),
            }

            struct ModelRun {
                model: ReviewerModel,
                display_name: String,
                method: ReviewMethod,
                system_prompt: Option<String>,
            }

            let mut runs: Vec<ModelRun> = Vec::new();
            for &model in models {
                // Look up the model ID from the config to get the display name.
                let model_id = self.opts.models.iter()
                    .find(|mc| mc.provider == match model {
                        ReviewerModel::Claude => "anthropic",
                        ReviewerModel::Codex => "openai",
                        ReviewerModel::Gemini => "gemini",
                    })
                    .map(|mc| mc.model.as_str())
                    .unwrap_or(model.name());
                let display_name = multi_review::model_display_name(model_id);
                // Claude reads CLAUDE.md natively; non-Claude models need
                // the workspace conventions prepended to the system prompt.
                let model_system_prompt = if model == ReviewerModel::Claude {
                    if system_prompt.is_empty() { None } else { Some(system_prompt.clone()) }
                } else {
                    let combined = format!("{}{}", conventions, system_prompt);
                    if combined.is_empty() { None } else { Some(combined) }
                };

                // Try API provider first, fall back to CLI.
                // Look up the model config entry to get the correct model ID.
                let model_config = self.opts.models.iter()
                    .find(|mc| mc.provider == match model {
                        ReviewerModel::Claude => "anthropic",
                        ReviewerModel::Codex => "openai",
                        ReviewerModel::Gemini => "gemini",
                    });
                let method = if let Some(provider) = model_config.and_then(multi_review::provider_for_model_config) {
                    logger.info(&format!("PR #{}: {} using API (model: {})", pr.number, model.name(), model_id));
                    ReviewMethod::Api(provider)
                } else {
                    logger.info(&format!("PR #{}: {} using CLI (no API key)", pr.number, model.name()));
                    let adapter = match model {
                        ReviewerModel::Claude => claude_adapter(ExternalAgentRunOptions::default()),
                        ReviewerModel::Codex => codex_adapter(ExternalAgentRunOptions::default()),
                        ReviewerModel::Gemini => gemini_adapter(ExternalAgentRunOptions::default()),
                    };
                    let opts = ExternalAgentRunOptions {
                        cwd: Some(workspace_cwd.clone()),
                        timeout_secs: ext_cfg.timeout_secs,
                        model: None,
                        extra_flags: Some(disallowed_tools_for(model)),
                    };
                    ReviewMethod::Cli(adapter, opts)
                };

                // Emit stage-started event.
                self.opts.tui_tx.send(TuiEvent::StageStarted {
                    workflow_name: self.config.name.clone(),
                    run_id: run_id.clone(),
                    stage_name: format!("review-{}", model.name()),
                });

                runs.push(ModelRun { model, display_name, method, system_prompt: model_system_prompt });
            }

            // ── Run all model agents concurrently ────────────────────────
            // Mark the "agent-loop" sidebar stage as in-progress for the
            // duration of all parallel model reviews.
            self.opts.tui_tx.send(TuiEvent::StageStarted {
                workflow_name: self.config.name.clone(),
                run_id: run_id.clone(),
                stage_name: "agent-loop".to_string(),
            });

            // Each future borrows `&ctx` and `&review_registry` (immutable).
            // `futures::future::join_all` runs on the same task — no `Send`
            // bound is needed.
            let review_futures: Vec<_> = runs.iter().map(|run| {
                async {
                    let result: Result<String> = match &run.method {
                        ReviewMethod::Api(provider) => {
                            crate::workflows::stages::api_review_loop::run_api_review_loop(
                                provider.as_ref(),
                                &review_registry,
                                &review_prompt,
                                run.system_prompt.as_deref(),
                                &logger,
                            ).await
                        }
                        ReviewMethod::Cli(adapter, opts) => {
                            run_external_agent_loop(
                                &review_prompt,
                                &ctx,
                                adapter,
                                opts,
                                run.system_prompt.as_deref(),
                            )
                            .await
                            .map(|r| r.answer)
                        }
                    };
                    (run.model, run.display_name.clone(), result)
                }
            }).collect();

            let model_results = futures::future::join_all(review_futures).await;

            // Emit completion events and collect results.
            let mut reviews: Vec<(String, String)> = Vec::new(); // (display_name, review_text)
            let mut failed_models: Vec<(String, String)> = Vec::new(); // (display_name, error)
            for (model, display_name, result) in &model_results {
                match result {
                    Ok(review_text) => {
                        self.opts.tui_tx.send(TuiEvent::StageCompleted {
                            workflow_name: self.config.name.clone(),
                            run_id: run_id.clone(),
                            stage_name: format!("review-{}", model.name()),
                        });
                        reviews.push((display_name.clone(), review_text.clone()));
                    }
                    Err(e) => {
                        self.opts.tui_tx.send(TuiEvent::StageFailed {
                            workflow_name: self.config.name.clone(),
                            run_id: run_id.clone(),
                            stage_name: format!("review-{}", model.name()),
                            error: e.to_string(),
                        });
                        let full_error = e.to_string();
                        logger.error(&format!("{} review failed: {}", display_name, full_error));
                        // Emit to TUI log with full error details (StageFailed
                        // only shows in the stage indicator, not the log pane).
                        self.opts.tui_tx.send(TuiEvent::LogMessage {
                            workflow_name: self.config.name.clone(),
                            level: "error".to_string(),
                            stage: format!("review-{}", model.name()),
                            message: format!("{} review failed:\n{}", display_name, full_error),
                            run_id: run_id.clone(),
                        });
                        failed_models.push((display_name.clone(), full_error));
                    }
                }
            }

            // Mark "agent-loop" as done (or failed if all models failed).
            if reviews.is_empty() {
                self.opts.tui_tx.send(TuiEvent::StageFailed {
                    workflow_name: self.config.name.clone(),
                    run_id: run_id.clone(),
                    stage_name: "agent-loop".to_string(),
                    error: "All review models failed".to_string(),
                });
                return Err(anyhow::anyhow!("All review models failed"));
            }
            self.opts.tui_tx.send(TuiEvent::StageCompleted {
                workflow_name: self.config.name.clone(),
                run_id: run_id.clone(),
                stage_name: "agent-loop".to_string(),
            });

            // ── Consolidate reviews ──────────────────────────────────────
            let mut consolidated = if reviews.len() >= 2 {
                let display_names: Vec<&str> = reviews.iter().map(|(name, _)| name.as_str()).collect();
                let example_row = format!("| {} | 85 | ✅ Approved |", display_names.first().unwrap_or(&"Model"));

                // Include info about failed models so the consolidation
                // can note them in the Summary table.
                let mut all_model_names = display_names.join(", ");
                let mut failed_note = String::new();
                if !failed_models.is_empty() {
                    let failed_names: Vec<&str> = failed_models.iter().map(|(n, _)| n.as_str()).collect();
                    all_model_names.push_str(", ");
                    all_model_names.push_str(&failed_names.join(", "));
                    failed_note.push_str("\n\nNote: The following models FAILED and could not produce a review. ");
                    failed_note.push_str("Include them in the Summary table with '🔴 Failed':\n");
                    for (name, error) in &failed_models {
                        let short = if error.len() > 80 { &error[..80] } else { error.as_str() };
                        failed_note.push_str(&format!("- {}: {}\n", name, short));
                    }
                }

                let show_branding = self.config.pull_request.as_ref()
                    .and_then(|pr_cfg| pr_cfg.show_branding)
                    .unwrap_or(true);
                let score_prefix = if show_branding { "📊 " } else { "" };
                let verdict_prefix = if show_branding { "🧑‍🍳 " } else { "" };

                let consolidation_prompt = self.load_and_render_prompt(
                    "review-consolidation",
                    &[
                        ("review_count", &reviews.len().to_string()),
                        ("pr_title", &pr.title),
                        ("pr_number", &pr.number.to_string()),
                        ("reviews", &format!("{}{}", format_reviews_for_consolidation(&reviews), failed_note)),
                        ("model_display_names", &all_model_names),
                        ("model_display_names_example", &example_row),
                        ("score_prefix", score_prefix),
                        ("verdict_prefix", verdict_prefix),
                        ("focus_directives_section", &format_focus_for_display(&focus_directives)),
                    ],
                );

                self.opts.tui_tx.send(TuiEvent::StageStarted {
                    workflow_name: self.config.name.clone(),
                    run_id: run_id.clone(),
                    stage_name: "review-consolidation".to_string(),
                });

                // Try Anthropic API first; fall back to Claude CLI.
                let consolidation_result = if std::env::var("ANTHROPIC_API_KEY").map(|k| !k.is_empty()).unwrap_or(false) {
                    let primary = self.opts.models.first().map(|m| m.model.as_str()).unwrap_or("claude-sonnet-4-20250514");
                    let provider = crate::providers::anthropic::AnthropicProvider::new(primary);
                    use crate::providers::provider::{LLMProvider as _, Message as LLMMessage, MessageRole as LLMRole};
                    match provider.complete(
                        &[LLMMessage { role: LLMRole::User, content: consolidation_prompt.clone(), content_blocks: None, tool_call_id: None }],
                        None,
                    ).await {
                        Ok(result) => Ok(result.content),
                        Err(e) => {
                            logger.error(&format!("Consolidation via API failed: {}", e));
                            Err(e)
                        }
                    }
                } else {
                    Err(anyhow::anyhow!("ANTHROPIC_API_KEY not set"))
                };

                let consolidated_text = match consolidation_result {
                    Ok(text) => text,
                    Err(_) => {
                        // Fallback: use Claude CLI for consolidation.
                        let adapter = claude_adapter(ExternalAgentRunOptions::default());
                        let opts = ExternalAgentRunOptions {
                            cwd: Some(ctx.workspace_dir.to_string_lossy().to_string()),
                            timeout_secs: Some(120),
                            model: None,
                            extra_flags: Some(disallowed_tools_for(ReviewerModel::Claude)),
                        };
                        match run_external_agent_loop(&consolidation_prompt, &ctx, &adapter, &opts, None).await {
                            Ok(result) if result.success => result.answer,
                            Ok(result) => {
                                logger.error(&format!(
                                    "Consolidation via CLI returned failure: {}",
                                    result.error.as_deref().unwrap_or("unknown")
                                ));
                                format_reviews_for_consolidation(&reviews)
                            }
                            Err(e) => {
                                logger.error(&format!("Consolidation via CLI failed: {}", e));
                                format_reviews_for_consolidation(&reviews)
                            }
                        }
                    }
                };

                self.opts.tui_tx.send(TuiEvent::StageCompleted {
                    workflow_name: self.config.name.clone(),
                    run_id: run_id.clone(),
                    stage_name: "review-consolidation".to_string(),
                });

                consolidated_text
            } else {
                // Only one model succeeded — use its review but append a
                // Summary section showing which models succeeded/failed.
                let mut text = reviews[0].1.clone();

                if !failed_models.is_empty() {
                    text.push_str("\n\n### Summary\n\n");
                    text.push_str("| Model | Verdict |\n|-------|--------|\n");
                    text.push_str(&format!("| {} | (see review above) |\n", reviews[0].0));
                    for (name, error) in &failed_models {
                        // Truncate error for the table.
                        let short_err = if error.len() > 60 {
                            format!("{}...", &error[..57])
                        } else {
                            error.clone()
                        };
                        text.push_str(&format!("| {} | 🔴 Failed: {} |\n", name, short_err));
                    }
                }

                text
            };

            // ── Post inline comments FIRST (before the timeline summary) ──
            // Extract from each individual model's raw review and deduplicate
            // by path:line. Uses both structured INLINE_COMMENT markers and
            // markdown bold **path:line** format as fallback.
            {
                let mut all_inline_comments = Vec::new();
                let mut seen_locations = std::collections::HashSet::new();
                let mut from_individual = 0usize;
                for (_name, review_text) in &reviews {
                    let comments = crate::workflows::stages::pr_review_poster::parse_inline_comments_from_text(review_text);
                    for c in comments {
                        let key = format!("{}:{}", c.path, c.line);
                        if seen_locations.insert(key) {
                            from_individual += 1;
                            all_inline_comments.push(c);
                        }
                    }
                }
                // Also try parsing the consolidated text itself.
                let mut from_consolidated = 0usize;
                {
                    let consolidated_comments = crate::workflows::stages::pr_review_poster::parse_inline_comments_from_text(&consolidated);
                    for c in consolidated_comments {
                        let key = format!("{}:{}", c.path, c.line);
                        if seen_locations.insert(key) {
                            from_consolidated += 1;
                            all_inline_comments.push(c);
                        }
                    }
                }
                logger.info(&format!(
                    "Inline comment extraction: {} from individual reviews, {} from consolidated, {} total",
                    from_individual, from_consolidated, all_inline_comments.len()
                ));

                if !all_inline_comments.is_empty() {
                    let head_sha = &pr.head_ref_oid;
                    let show_branding = self.config.pull_request.as_ref()
                        .and_then(|pr_cfg| pr_cfg.show_branding)
                        .unwrap_or(true);
                    self.opts.tui_tx.send(TuiEvent::LogMessage {
                        workflow_name: self.config.name.clone(),
                        level: "info".to_string(),
                        stage: "inline-comments".to_string(),
                        message: format!(
                            "Posting {} inline comment(s) (head_sha: {})",
                            all_inline_comments.len(),
                            &head_sha[..7.min(head_sha.len())]
                        ),
                        run_id: run_id.clone(),
                    });
                    let mut posted = 0u32;
                    let mut failed = 0u32;
                    let mut posted_keys = std::collections::HashSet::new();
                    for c in &all_inline_comments {
                        let body = if show_branding {
                            format!("🧑‍🍳 {}", c.body)
                        } else {
                            c.body.clone()
                        };
                        match crate::workflows::github_prs::post_inline_comment(
                            &pr.repo, pr.number, head_sha, &c.path, c.line, &body,
                        ).await {
                            Ok(()) => {
                                posted += 1;
                                posted_keys.insert(format!("{}:{}", c.path, c.line));
                            }
                            Err(e) => {
                                failed += 1;
                                self.opts.tui_tx.send(TuiEvent::LogMessage {
                                    workflow_name: self.config.name.clone(),
                                    level: "warn".to_string(),
                                    stage: "inline-comments".to_string(),
                                    message: format!(
                                        "Failed to post inline on {}:{} — {}",
                                        c.path, c.line, e
                                    ),
                                    run_id: run_id.clone(),
                                });
                            }
                        }
                    }

                    // Remove successfully-posted inline observations from the
                    // consolidated text so the timeline comment only shows
                    // observations that failed to post inline.
                    if !posted_keys.is_empty() {
                        consolidated = filter_posted_inline_observations(
                            &consolidated, &posted_keys,
                        );
                    }
                    self.opts.tui_tx.send(TuiEvent::LogMessage {
                        workflow_name: self.config.name.clone(),
                        level: "info".to_string(),
                        stage: "inline-comments".to_string(),
                        message: format!(
                            "Inline comments: {} posted, {} failed",
                            posted, failed
                        ),
                        run_id: run_id.clone(),
                    });
                } else {
                    self.opts.tui_tx.send(TuiEvent::LogMessage {
                        workflow_name: self.config.name.clone(),
                        level: "info".to_string(),
                        stage: "inline-comments".to_string(),
                        message: "No inline comments found in model reviews".to_string(),
                        run_id: run_id.clone(),
                    });
                }
            }

            // ── Delete the placeholder and post the full review ─────────
            if let Some(pid) = placeholder_id {
                if let Err(e) = crate::workflows::github_prs::delete_comment(&pr.repo, pid).await {
                    logger.warn(&format!("Could not delete placeholder comment: {}", e));
                }
            }

            ctx.agent_result = Some(RunResult::success(
                "multi-model-review",
                consolidated,
                vec![],
                models.len(),
                0,
            ));

            self.run_stage(&PrReviewPosterStage, &mut ctx).await?;

            // Log per-model verdicts with scores for the TUI.
            let mut all_scores: Vec<u32> = Vec::new();
            for (name, review_text) in &reviews {
                let v = crate::workflows::stages::pr_review_poster::parse_verdict_from_text(review_text);
                let s = crate::workflows::stages::pr_review_poster::parse_score_from_text(review_text);
                let emoji = if v == "approved" { "✅" } else { "🔴" };
                let score_str = s.map(|n| format!(" ({})", n)).unwrap_or_default();
                if let Some(n) = s {
                    all_scores.push(n);
                }
                self.opts.tui_tx.send(TuiEvent::LogMessage {
                    workflow_name: self.config.name.clone(),
                    level: "info".to_string(),
                    stage: "verdict".to_string(),
                    message: format!("{} {}{} {}", emoji, name, score_str, if v == "approved" { "Approved" } else { "Not Approved" }),
                    run_id: run_id.clone(),
                });
            }
            for (name, _err) in &failed_models {
                self.opts.tui_tx.send(TuiEvent::LogMessage {
                    workflow_name: self.config.name.clone(),
                    level: "error".to_string(),
                    stage: "verdict".to_string(),
                    message: format!("🔴 {} Failed", name),
                    run_id: run_id.clone(),
                });
            }
            // Log average score.
            if !all_scores.is_empty() {
                let avg = all_scores.iter().sum::<u32>() as f64 / all_scores.len() as f64;
                self.opts.tui_tx.send(TuiEvent::LogMessage {
                    workflow_name: self.config.name.clone(),
                    level: "info".to_string(),
                    stage: "verdict".to_string(),
                    message: format!("📊 Avg Score: {:.1}", avg),
                    run_id: run_id.clone(),
                });
            }
            let final_verdict = ctx.pr_review_result.as_ref()
                .map(|r| r.verdict.as_str())
                .unwrap_or("unknown");
            let final_emoji = if final_verdict == "approved" { "✅" } else { "🔴" };
            self.opts.tui_tx.send(TuiEvent::LogMessage {
                workflow_name: self.config.name.clone(),
                level: "info".to_string(),
                stage: "verdict".to_string(),
                message: format!("{} Consolidated: {}", final_emoji, if final_verdict == "approved" { "Approved" } else { "Not Approved" }),
                run_id: run_id.clone(),
            });

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

        // Skip plan PRs — they haven't been converted to code PRs yet.
        let prs: Vec<_> = prs
            .into_iter()
            .filter(|pr| !pr.title.starts_with("Plan("))
            .collect();

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
        let mut total_comment_counts: HashMap<u64, usize> = HashMap::new();
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
                match fetch_inline_review_comments(&pr.repo, pr.number, after_inline).await {
                    Ok(comments) => comments,
                    Err(e) => {
                        logger.error(&format!(
                            "PR #{}: failed to fetch inline comments: {}",
                            pr.number, e
                        ));
                        vec![]
                    }
                };
            let mut timeline =
                match fetch_pr_comments(&pr.repo, pr.number, after_timeline).await {
                    Ok(comments) => comments,
                    Err(e) => {
                        logger.error(&format!(
                            "PR #{}: failed to fetch timeline comments: {}",
                            pr.number, e
                        ));
                        vec![]
                    }
                };

            // Also fetch PR review bodies (submitted reviews with body text).
            // Review IDs are from a DIFFERENT numbering sequence than timeline
            // comment IDs, so we cannot use `after_timeline` to filter them.
            // Instead, fetch all reviews and filter by timestamp.
            let review_comments =
                match crate::workflows::github_prs::fetch_pr_review_comments(
                    &pr.repo, pr.number, None, // fetch ALL reviews
                )
                .await
                {
                    Ok(comments) => comments,
                    Err(e) => {
                        logger.error(&format!(
                            "PR #{}: failed to fetch review bodies: {}",
                            pr.number, e
                        ));
                        vec![]
                    }
                };

            // Filter reviews to only those posted after the last response.
            // Use proper datetime parsing — GitHub uses `Z` suffix while chrono
            // produces `+00:00`, making naive string comparison unreliable.
            let last_responded_at_dt = record
                .as_ref()
                .and_then(|r| chrono::DateTime::parse_from_rfc3339(&r.responded_at).ok());
            let existing_ids: std::collections::HashSet<u64> =
                timeline.iter().map(|c| c.id).collect();
            for rc in review_comments {
                // Skip reviews already in the timeline (by ID).
                if existing_ids.contains(&rc.id) {
                    continue;
                }
                // Skip reviews posted before the last response.
                if let Some(cutoff) = last_responded_at_dt {
                    if let Ok(review_dt) = chrono::DateTime::parse_from_rfc3339(&rc.created_at) {
                        if review_dt <= cutoff {
                            continue;
                        }
                    }
                    // If we can't parse the review timestamp, include it to
                    // be safe — better to re-process than to miss it.
                }
                timeline.push(rc);
            }

            logger.info(&format!(
                "PR #{}: found {} inline, {} timeline/review comments after cursor",
                pr.number, inline.len(), timeline.len()
            ));

            // Filter out bot comments — they're automated and shouldn't
            // trigger the agent (CI status, deploy previews, etc.).

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

            // Track new comment count for display in the Info pane badge.
            total_comment_counts.insert(pr.number, new_inline.len() + new_timeline.len());

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
                let count = *total_comment_counts.get(&pr.number).unwrap_or(&0);
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
                    comment_count: count,
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
// Plan-first helpers
// ---------------------------------------------------------------------------

/// Parsed approval result.
struct ParsedApproval {
    /// Extra text after "approved" (e.g., "also do X"), or `None`.
    addendum: Option<String>,
}

/// Parse a comment body to check if it contains an approval.
///
/// Returns `Some(ParsedApproval)` if the comment starts with "approved"
/// (case-insensitive), optionally followed by addendum text.
fn parse_approval(body: &str) -> Option<ParsedApproval> {
    let trimmed = body.trim();
    let lower = trimmed.to_lowercase();

    if !lower.starts_with("approved") {
        return None;
    }

    let rest = trimmed.get(8..)?.trim_start(); // skip "approved" (8 chars)
    if rest.is_empty() {
        return Some(ParsedApproval { addendum: None });
    }

    // Strip leading punctuation: ";", ",", ".", "!", "—", "-"
    let rest = rest
        .trim_start_matches(|c: char| {
            c == ';' || c == ',' || c == '.' || c == '!' || c == '—' || c == '-'
        })
        .trim_start();

    if rest.is_empty() {
        return Some(ParsedApproval { addendum: None });
    }

    Some(ParsedApproval {
        addendum: Some(rest.to_string()),
    })
}

/// Check if a login belongs to a bot account.
fn is_bot(login: &str) -> bool {
    login.ends_with("[bot]") || login == "github-actions"
}

// ---------------------------------------------------------------------------
// Focus directives
// ---------------------------------------------------------------------------

/// A focus directive requested by a PR participant.
#[derive(Debug, Clone)]
struct FocusDirective {
    text: String,
    source: String,       // "PR description" or "Comment"
    requested_by: String, // GitHub login
}

/// Collect focus directives from the PR description and timeline comments.
///
/// - **Source A**: Parses `## Review focus` section from PR body
/// - **Source B**: Scans comments for `@sousdev focus:` lines
async fn collect_focus_directives(
    repo: &str,
    pr_number: u64,
    pr_body: Option<&str>,
    pr_author: &str,
) -> Vec<FocusDirective> {
    let mut directives = Vec::new();

    // Source A: PR description "## Review focus" section.
    if let Some(body) = pr_body {
        if let Some(section) = extract_review_focus_section(body) {
            for line in section.lines() {
                let trimmed = line.trim();
                let text = trimmed
                    .strip_prefix("- ")
                    .or_else(|| trimmed.strip_prefix("* "))
                    .unwrap_or(trimmed);
                if !text.is_empty() {
                    directives.push(FocusDirective {
                        text: text.to_string(),
                        source: "PR description".into(),
                        requested_by: pr_author.to_string(),
                    });
                }
            }
        }
    }

    // Source B: PR comments with "@sousdev focus:".
    // Note: "@sousdev review" (without colon) is a re-review trigger, NOT
    // a focus directive — it has no text to extract.
    let comments = crate::workflows::github_prs::fetch_pr_comments(repo, pr_number, None)
        .await
        .unwrap_or_default();
    for comment in &comments {
        if is_bot(&comment.login) {
            continue;
        }
        for directive in parse_focus_directives_from_comment(&comment.body) {
            directives.push(FocusDirective {
                text: directive,
                source: "Comment".into(),
                requested_by: comment.login.clone(),
            });
        }
    }

    directives
}

/// Extract the `## Review focus` section from a PR description.
///
/// Returns the text between `## Review focus` and the next `##` heading
/// (or end of text).
fn extract_review_focus_section(body: &str) -> Option<String> {
    let mut in_section = false;
    let mut lines = Vec::new();
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.eq_ignore_ascii_case("## review focus")
            || trimmed.eq_ignore_ascii_case("## review focus:")
        {
            in_section = true;
            continue;
        }
        if in_section && trimmed.starts_with("## ") {
            break; // Next heading — end of section.
        }
        if in_section {
            lines.push(line);
        }
    }
    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n").trim().to_string())
    }
}

/// Parse `@sousdev focus:` directives from a comment.
///
/// Note: `@sousdev review` (without colon) is a re-review trigger, not
/// a focus directive — it's handled by the re-review check, not here.
fn parse_focus_directives_from_comment(body: &str) -> Vec<String> {
    let mut directives = Vec::new();
    let lower = body.to_lowercase();
    for line in body.lines() {
        let line_lower = line.trim().to_lowercase();
        if let Some(rest) = line_lower.strip_prefix("@sousdev focus:") {
            let text = rest.trim();
            if !text.is_empty() {
                // Use the original-case text, not the lowercased version.
                let original_rest = &line.trim()["@sousdev focus:".len()..];
                directives.push(original_rest.trim().to_string());
            }
        }
    }
    // Also handle multiline: if the whole comment body starts with the directive
    // and has more text on subsequent lines.
    if directives.is_empty() && lower.starts_with("@sousdev focus:") {
        let rest = body["@sousdev focus:".len()..].trim();
        if !rest.is_empty() {
            directives.push(rest.to_string());
        }
    }
    directives
}

/// Format focus directives for injection into the review prompt.
fn format_focus_for_prompt(directives: &[FocusDirective]) -> String {
    if directives.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "\n\n## Reviewer-requested focus areas\n\n\
         The following focus areas were requested by PR participants. Pay special\n\
         attention to these items during your review.\n\n",
    );
    for d in directives {
        out.push_str(&format!(
            "- **{}** (from @{}, {})\n",
            d.text, d.requested_by, d.source
        ));
    }
    out
}

/// Format focus directives for display in the consolidated review comment.
fn format_focus_for_display(directives: &[FocusDirective]) -> String {
    if directives.is_empty() {
        return String::new();
    }
    let mut out = String::from("### Focus directives\n\n");
    // Group by (requested_by, source).
    let mut current_key = String::new();
    for d in directives {
        let key = format!("@{} ({})", d.requested_by, d.source);
        if key != current_key {
            if !current_key.is_empty() {
                out.push('\n');
            }
            out.push_str(&format!("{}\n", key));
            current_key = key;
        }
        out.push_str(&format!("{}\n", d.text));
    }
    out
}

/// Remove successfully-posted inline observations from the consolidated
/// review text.
///
/// Replaces the `### Inline observations` section with
/// `### Inline observations that failed to post` containing only the
/// observations whose `path:line` is NOT in `posted_keys`.  If all
/// observations were posted, the section is removed entirely.
fn filter_posted_inline_observations(
    text: &str,
    posted_keys: &std::collections::HashSet<String>,
) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut result = Vec::new();
    let mut in_inline_section = false;
    let mut failed_observations: Vec<String> = Vec::new();

    let path_line_re = regex::Regex::new(
        r"`?([^\s`*:]+\.\w+):(\d+)`?"
    ).unwrap();

    for line in &lines {
        if line.contains("### Inline observation") {
            in_inline_section = true;
            continue;
        }
        if in_inline_section && line.starts_with("### ") {
            // End of inline section — emit filtered observations.
            if !failed_observations.is_empty() {
                result.push("### Inline observations that failed to post".to_string());
                result.push(String::new());
                for obs in &failed_observations {
                    result.push(obs.clone());
                }
                result.push(String::new());
            }
            in_inline_section = false;
            result.push(line.to_string());
            continue;
        }
        if in_inline_section {
            // Check if this line references a path:line that was posted.
            if let Some(caps) = path_line_re.captures(line) {
                let key = format!("{}:{}", &caps[1], &caps[2]);
                if posted_keys.contains(&key) {
                    continue; // Skip — already posted inline.
                }
            }
            if !line.trim().is_empty() {
                failed_observations.push(line.to_string());
            }
        } else {
            result.push(line.to_string());
        }
    }

    // If the inline section was the last section (no ### after it).
    if in_inline_section && !failed_observations.is_empty() {
        result.push("### Inline observations that failed to post".to_string());
        result.push(String::new());
        for obs in &failed_observations {
            result.push(obs.clone());
        }
    }

    result.join("\n")
}

/// Format the "review in progress" placeholder comment.
fn format_review_placeholder(
    model_names: &[String],
    focus_directives: &[FocusDirective],
    show_branding: bool,
) -> String {
    let mut body = String::new();
    if show_branding {
        body.push_str("🧑‍🍳 ");
    }
    body.push_str("Review in progress — the following models are reviewing this PR:\n\n");
    for name in model_names {
        body.push_str(&format!("- {}\n", name));
    }
    body.push('\n');
    if focus_directives.is_empty() {
        body.push_str("To request specific focus areas, comment with `@sousdev focus: <your request>`.\n");
    } else {
        body.push_str("Focus areas requested:\n");
        for d in focus_directives {
            body.push_str(&format!("- {} (from @{})\n", d.text, d.requested_by));
        }
    }
    body
}

/// Run a `git` sub-command in `cwd` and return trimmed stdout.
async fn exec_git(args: &[&str], cwd: &std::path::Path) -> anyhow::Result<String> {
    let output = tokio::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow::anyhow!(
            "git {} failed: {}",
            args.join(" "),
            stderr.trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
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
        assert_eq!(truncate_title("a very long title indeed", 10), "a very …");
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

    // ── parse_approval tests ──────────────────────────────────────────────

    #[test]
    fn test_parse_approval_exact() {
        let result = parse_approval("approved");
        assert!(result.is_some());
        assert!(result.unwrap().addendum.is_none());
    }

    #[test]
    fn test_parse_approval_case_insensitive() {
        assert!(parse_approval("Approved").is_some());
        assert!(parse_approval("APPROVED").is_some());
        assert!(parse_approval("ApPrOvEd").is_some());
    }

    #[test]
    fn test_parse_approval_with_whitespace() {
        let result = parse_approval("  approved  ");
        assert!(result.is_some());
        assert!(result.unwrap().addendum.is_none());
    }

    #[test]
    fn test_parse_approval_with_semicolon_addendum() {
        let result = parse_approval("approved; also handle the empty case").unwrap();
        assert_eq!(result.addendum.as_deref(), Some("also handle the empty case"));
    }

    #[test]
    fn test_parse_approval_with_comma_addendum() {
        let result = parse_approval("approved, but also fix the tests").unwrap();
        assert_eq!(result.addendum.as_deref(), Some("but also fix the tests"));
    }

    #[test]
    fn test_parse_approval_with_period_addendum() {
        let result = parse_approval("Approved. Also make sure to handle edge case X").unwrap();
        assert_eq!(
            result.addendum.as_deref(),
            Some("Also make sure to handle edge case X")
        );
    }

    #[test]
    fn test_parse_approval_just_punctuation() {
        let result = parse_approval("approved!").unwrap();
        assert!(result.addendum.is_none());
    }

    #[test]
    fn test_parse_approval_not_approved() {
        assert!(parse_approval("looks good").is_none());
        assert!(parse_approval("not approved").is_none());
        assert!(parse_approval("").is_none());
        assert!(parse_approval("I think this is approved by someone").is_none());
    }

    #[test]
    fn test_parse_approval_multiline() {
        let body = "Approved; here are some extra notes\n\nAlso check the error handling";
        let result = parse_approval(body).unwrap();
        assert!(result.addendum.is_some());
        assert!(result.addendum.unwrap().starts_with("here are some extra notes"));
    }

    // ── is_bot tests ──────────────────────────────────────────────────────

    #[test]
    fn test_is_bot_detects_bots() {
        assert!(is_bot("dependabot[bot]"));
        assert!(is_bot("renovate[bot]"));
        assert!(is_bot("github-actions"));
    }

    #[test]
    fn test_is_bot_allows_humans() {
        assert!(!is_bot("ytham"));
        assert!(!is_bot("graemecode"));
        assert!(!is_bot("tayyabmh"));
    }

    // ── Focus directive tests ─────────────────────────────────────────────

    #[test]
    fn test_extract_review_focus_section() {
        let body = "## Description\nSome text.\n\n## Review focus\n- Check migrations\n- Verify error handling\n\n## Notes\nOther stuff.";
        let section = extract_review_focus_section(body).unwrap();
        assert!(section.contains("Check migrations"));
        assert!(section.contains("Verify error handling"));
        assert!(!section.contains("Other stuff"));
    }

    #[test]
    fn test_extract_review_focus_section_missing() {
        let body = "## Description\nNo focus section here.";
        assert!(extract_review_focus_section(body).is_none());
    }

    #[test]
    fn test_extract_review_focus_section_at_end() {
        let body = "## Description\nSome text.\n\n## Review focus\n- Check migrations";
        let section = extract_review_focus_section(body).unwrap();
        assert!(section.contains("Check migrations"));
    }

    #[test]
    fn test_parse_focus_directives_from_comment() {
        let body = "@sousdev focus: Check the SQL migration for backward compatibility";
        let directives = parse_focus_directives_from_comment(body);
        assert_eq!(directives.len(), 1);
        assert_eq!(directives[0], "Check the SQL migration for backward compatibility");
    }

    #[test]
    fn test_parse_focus_directives_review_is_not_focus() {
        // "@sousdev review" is a re-review trigger, not a focus directive.
        let body = "@sousdev review";
        let directives = parse_focus_directives_from_comment(body);
        assert!(directives.is_empty());
    }

    #[test]
    fn test_parse_focus_directives_review_with_colon_is_not_focus() {
        // "@sousdev review:" with text is also not a focus directive.
        let body = "@sousdev review: please re-review this";
        let directives = parse_focus_directives_from_comment(body);
        assert!(directives.is_empty());
    }

    #[test]
    fn test_parse_focus_directives_case_insensitive() {
        let body = "@SousDev Focus: Check auth module";
        let directives = parse_focus_directives_from_comment(body);
        assert_eq!(directives.len(), 1);
        assert!(directives[0].contains("Check auth module"));
    }

    #[test]
    fn test_parse_focus_directives_no_match() {
        let body = "This is a regular comment with no directives.";
        let directives = parse_focus_directives_from_comment(body);
        assert!(directives.is_empty());
    }

    #[test]
    fn test_format_focus_for_prompt_empty() {
        assert!(format_focus_for_prompt(&[]).is_empty());
    }

    #[test]
    fn test_format_focus_for_prompt() {
        let directives = vec![
            FocusDirective {
                text: "Check migrations".into(),
                source: "PR description".into(),
                requested_by: "alice".into(),
            },
        ];
        let output = format_focus_for_prompt(&directives);
        assert!(output.contains("Reviewer-requested focus areas"));
        assert!(output.contains("Check migrations"));
        assert!(output.contains("@alice"));
    }

    #[test]
    fn test_format_focus_for_display() {
        let directives = vec![
            FocusDirective {
                text: "Check migrations".into(),
                source: "PR description".into(),
                requested_by: "alice".into(),
            },
            FocusDirective {
                text: "Verify auth".into(),
                source: "Comment".into(),
                requested_by: "bob".into(),
            },
        ];
        let output = format_focus_for_display(&directives);
        assert!(output.contains("### Focus directives"));
        assert!(output.contains("@alice (PR description)"));
        assert!(output.contains("Check migrations"));
        assert!(output.contains("@bob (Comment)"));
        assert!(output.contains("Verify auth"));
    }

    #[test]
    fn test_format_focus_for_display_empty() {
        assert!(format_focus_for_display(&[]).is_empty());
    }

    #[test]
    fn test_format_review_placeholder_with_focus() {
        let models = vec!["Claude Opus 4.6".to_string(), "GPT-5.4".to_string()];
        let directives = vec![FocusDirective {
            text: "Check auth".into(),
            source: "Comment".into(),
            requested_by: "bob".into(),
        }];
        let body = format_review_placeholder(&models, &directives, true);
        assert!(body.contains("🧑‍🍳"));
        assert!(body.contains("Claude Opus 4.6"));
        assert!(body.contains("GPT-5.4"));
        assert!(body.contains("Focus areas requested:"));
        assert!(body.contains("Check auth"));
    }

    #[test]
    fn test_format_review_placeholder_no_focus() {
        let models = vec!["Claude Opus 4.6".to_string()];
        let body = format_review_placeholder(&models, &[], true);
        assert!(body.contains("@sousdev focus:"));
    }

    #[test]
    fn test_format_review_placeholder_no_branding() {
        let models = vec!["Claude Opus 4.6".to_string()];
        let body = format_review_placeholder(&models, &[], false);
        assert!(!body.contains("🧑‍🍳"));
        assert!(body.contains("Review in progress"));
    }

    // ── filter_posted_inline_observations tests ───────────────────────────

    #[test]
    fn test_filter_posted_inline_observations() {
        let text = "### Consensus findings\nSome text.\n\n### Inline observations\n- `src/a.rs:10` — Issue A\n- `src/b.rs:20` — Issue B\n- `src/c.rs:30` — Issue C\n\n### Summary\n| Model | Verdict |";
        let mut posted = std::collections::HashSet::new();
        posted.insert("src/a.rs:10".to_string());
        posted.insert("src/c.rs:30".to_string());
        let result = filter_posted_inline_observations(text, &posted);
        assert!(!result.contains("Issue A")); // posted — removed
        assert!(result.contains("Issue B"));  // failed — kept
        assert!(!result.contains("Issue C")); // posted — removed
        assert!(result.contains("failed to post"));
        assert!(result.contains("### Summary")); // preserved
    }

    #[test]
    fn test_filter_all_posted_removes_section() {
        let text = "### Consensus findings\nText.\n\n### Inline observations\n- `src/a.rs:10` — Issue A\n\n### Summary\nTable.";
        let mut posted = std::collections::HashSet::new();
        posted.insert("src/a.rs:10".to_string());
        let result = filter_posted_inline_observations(text, &posted);
        assert!(!result.contains("Inline observation"));
        assert!(result.contains("### Summary"));
    }
}
