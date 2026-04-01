use anyhow::Result;
use async_trait::async_trait;
use crate::workflows::stage::{Stage, StageContext};

/// Sets `ctx.branch` to the PR head branch so subsequent stages (e.g.
/// `PullRequestStage`) know which branch they are operating on.
///
/// This stage is used by the `githubPRs` (review) and `githubPRResponses`
/// (comment-response) workflows.  The actual git checkout is performed by
/// [`WorkspaceManager::setup_for_pr_review`]; this stage only propagates the
/// branch name through the context.
pub struct PrCheckoutStage;

#[async_trait]
impl Stage for PrCheckoutStage {
    fn name(&self) -> &str {
        "pr-checkout"
    }

    async fn run(&self, ctx: &mut StageContext) -> Result<()> {
        // Prefer the PR being reviewed; fall back to the one being responded to.
        let pr = ctx
            .reviewing_pr
            .as_ref()
            .or(ctx.responding_pr.as_ref())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "PrCheckoutStage: neither reviewing_pr nor responding_pr is set"
                )
            })?;

        ctx.logger.info(&format!(
            "PrCheckoutStage: PR #{} — branch: {}",
            pr.number, pr.head_ref_name
        ));
        ctx.branch = pr.head_ref_name.clone();
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflows::github_prs::{GitHubPR, PRAuthor};
    use crate::workflows::stage::{ResolvedPrompts, StageContext};
    use crate::tools::registry::ToolRegistry;
    use crate::types::config::WorkflowConfig;
    use crate::utils::logger::Logger;
    use crate::workflows::workflow::ParsedTask;
    use std::sync::Arc;

    fn make_pr(number: u64, head_ref: &str) -> GitHubPR {
        GitHubPR {
            number,
            title: "Test PR".into(),
            body: None,
            url: format!("https://github.com/owner/repo/pull/{}", number),
            head_ref_name: head_ref.to_string(),
            head_ref_oid: "abc123".into(),
            base_ref_name: "main".into(),
            author: PRAuthor { login: "user".into() },
            labels: vec![],
            review_decision: String::new(),
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            repo: "owner/repo".into(),
            requested_reviewers: vec![],
            requested_teams: vec![],
            assignees: vec![],
            additions: 0,
            deletions: 0,
        }
    }

    fn make_ctx() -> StageContext {
        StageContext {
            config: Arc::new(WorkflowConfig::default()),
            provider: Arc::new(crate::providers::anthropic::AnthropicProvider::new("test")),
            registry: Arc::new(ToolRegistry::new()),
            workspace_dir: std::path::PathBuf::from("/tmp"),
            branch: String::new(),
            parsed_task: ParsedTask::new(""),
            harness_root: std::path::PathBuf::from("/tmp"),
            prompts: ResolvedPrompts::default(),
            system_prompt: None,
            target_repo: None,
            logger: Logger::new("test"),
            run_id: "test-run".into(),
            retry_count: 0,
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
            workflow_log: None,
            aborted: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    #[tokio::test]
    async fn test_sets_branch_from_reviewing_pr() {
        let mut ctx = make_ctx();
        ctx.reviewing_pr = Some(make_pr(42, "feature/fix-auth"));
        PrCheckoutStage.run(&mut ctx).await.unwrap();
        assert_eq!(ctx.branch, "feature/fix-auth");
    }

    #[tokio::test]
    async fn test_sets_branch_from_responding_pr() {
        let mut ctx = make_ctx();
        ctx.responding_pr = Some(make_pr(7, "feature/add-metrics"));
        PrCheckoutStage.run(&mut ctx).await.unwrap();
        assert_eq!(ctx.branch, "feature/add-metrics");
    }

    #[tokio::test]
    async fn test_reviewing_pr_takes_priority() {
        let mut ctx = make_ctx();
        ctx.reviewing_pr = Some(make_pr(1, "branch-a"));
        ctx.responding_pr = Some(make_pr(2, "branch-b"));
        PrCheckoutStage.run(&mut ctx).await.unwrap();
        assert_eq!(ctx.branch, "branch-a");
    }

    #[tokio::test]
    async fn test_errors_when_no_pr_set() {
        let mut ctx = make_ctx();
        let result = PrCheckoutStage.run(&mut ctx).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("reviewing_pr nor responding_pr"));
    }
}
