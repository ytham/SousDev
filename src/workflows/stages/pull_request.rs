use anyhow::Result;
use async_trait::async_trait;
use std::io::Write;
use std::path::Path;
use std::process::Stdio;
use tempfile::NamedTempFile;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use regex::Regex;

use crate::workflows::stage::{Stage, StageContext};

/// Commits all workspace changes and opens (or finds an existing) pull
/// request on GitHub.
///
/// Steps:
/// 1. `git add -A`
/// 2. `git status --porcelain` — commit only when there are staged changes.
/// 3. `git rev-list --count <base>..HEAD` — skip PR creation when 0 commits.
/// 4. `git push -u origin <branch>`
/// 5. Check for an existing open PR on this branch via `gh pr list`.
/// 6. Build title / body from `ctx.pr_title` / `ctx.pr_generated_body`.
/// 7. Write body to a temp file; call `gh pr create --body-file`.
/// 8. Parse the PR URL from stdout; store in `ctx.pr_url`.
pub struct PullRequestStage;

/// Commit message fallback when no better source is available.
const DEFAULT_COMMIT_MSG: &str = "chore: apply sousdev agent changes";

/// Build the best available commit message from context.
///
/// Priority:
/// 1. Explicit title from `pull_request` config
/// 2. LLM-generated PR title (`ctx.pr_title`, set by PrDescriptionStage)
/// 3. First line of the parsed task (e.g. "Fix issue #42: <title>")
/// 4. Hardcoded fallback
fn derive_commit_message(ctx: &StageContext) -> String {
    // 1. Explicit config override.
    if let Some(title) = ctx
        .config
        .pull_request
        .as_ref()
        .and_then(|pr| pr.title.as_deref())
    {
        return title.to_string();
    }

    // 2. LLM-generated PR title (set by PrDescriptionStage which runs before us).
    if let Some(ref title) = ctx.pr_title {
        if !title.is_empty() {
            return title.clone();
        }
    }

    // 3. First line of the parsed task — typically "Fix issue #N: <title>".
    let first_line = ctx.parsed_task.task.lines().next().unwrap_or("");
    if !first_line.is_empty() && first_line.len() > 5 {
        // Truncate to a reasonable commit message length.
        let truncated = if first_line.len() > 100 {
            format!("{}...", &first_line[..97])
        } else {
            first_line.to_string()
        };
        return truncated;
    }

    // 4. Hardcoded fallback.
    DEFAULT_COMMIT_MSG.to_string()
}

#[async_trait]
impl Stage for PullRequestStage {
    fn name(&self) -> &str {
        "pull-request"
    }

    async fn run(&self, ctx: &mut StageContext) -> Result<()> {
        if ctx.is_aborted() {
            return Ok(());
        }

        // Try the automated push+PR flow first.
        match try_automated_pr(ctx).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                ctx.logger.info(&format!(
                    "Automated PR creation failed: {} — delegating to agent",
                    e
                ));
                // Fall through to agent-assisted recovery.
            }
        }

        // Agent-assisted recovery: invoke the Claude CLI with the error
        // context and let it fix the issue (commit, push, create PR).
        agent_assisted_pr(ctx).await
    }
}

/// Attempt the fully automated push + PR creation flow.
async fn try_automated_pr(ctx: &mut StageContext) -> Result<()> {
    let dir = &ctx.workspace_dir;
    let branch = &ctx.branch;
    let base_branch = ctx
        .config
        .workspace
        .as_ref()
        .and_then(|w| w.base_branch.as_deref())
        .unwrap_or("main");

    // ── 0. Ensure we're on the correct branch ──────────────────────────
    // The Claude CLI agent may have switched branches during its run.
    let current_branch = exec_git(&["branch", "--show-current"], dir)
        .await
        .unwrap_or_default();
    if current_branch.trim() != branch {
        ctx.logger.info(&format!(
            "Current branch is '{}', expected '{}' — switching",
            current_branch.trim(),
            branch
        ));
        // Try checkout; if the branch doesn't exist locally, create it.
        if exec_git(&["checkout", branch], dir).await.is_err() {
            exec_git(&["checkout", "-b", branch], dir).await?;
        }
    }

    // ── 1. Stage all changes ─────────────────────────────────────────────
    exec_git(&["add", "-A"], dir).await?;

    // ── 2. Commit if dirty ───────────────────────────────────────────────
    let status = exec_git(&["status", "--porcelain"], dir).await?;
    if !status.trim().is_empty() {
        let commit_msg = derive_commit_message(ctx);
        exec_git(&["commit", "-m", &commit_msg], dir).await?;
        ctx.logger.info(&format!("Committed workspace changes: {}", commit_msg));
    }

    // ── 3. Check whether there are new commits beyond base ───────────────
    let range = format!("{}..HEAD", base_branch);
    let commit_count_str = exec_git(&["rev-list", "--count", &range], dir)
        .await
        .unwrap_or_else(|_| "0".to_string());
    let commit_count: u64 = commit_count_str.trim().parse().unwrap_or(0);
    if commit_count == 0 {
        let diff = exec_git(
            &["diff", "--stat", &format!("origin/{}", base_branch), "HEAD"],
            dir,
        )
        .await
        .unwrap_or_default();
        if diff.trim().is_empty() {
            ctx.logger
                .info("No new commits or changes beyond base branch — skipping PR creation.");
            return Ok(());
        }
        ctx.logger.info("rev-list shows 0 commits but diff exists — proceeding with PR");
    }

    // ── 4. Rebase on remote branch if it exists ─────────────────────────
    let remote_ref = format!("origin/{}", branch);
    if exec_git(&["fetch", "origin", branch], dir).await.is_ok() {
        ctx.logger.info("Remote branch exists — rebasing on top of CI changes");
        match exec_git(&["rebase", &remote_ref], dir).await {
            Ok(_) => {}
            Err(_) => {
                ctx.logger.info("Rebase conflict — aborting rebase, will force-push");
                let _ = exec_git(&["rebase", "--abort"], dir).await;
            }
        }
    }

    // ── 5. Push ──────────────────────────────────────────────────────────
    ctx.logger.info(&format!("Pushing branch: {}", branch));
    let push_result = exec_git(&["push", "-u", "origin", branch], dir).await;
    match push_result {
        Ok(output) => {
            ctx.logger
                .info(&format!("Push succeeded: {}", output.trim()));
        }
        Err(e) => {
            ctx.logger
                .info(&format!("Push failed ({}), force-pushing", e));
            exec_git(&["push", "--force", "-u", "origin", branch], dir).await?;
        }
    }

    // Verify the branch was actually pushed.
    let ls_remote = exec_git(&["ls-remote", "--heads", "origin", branch], dir)
        .await
        .unwrap_or_default();
    if ls_remote.trim().is_empty() {
        return Err(anyhow::anyhow!(
            "Branch {} was not pushed to the remote — ls-remote returned empty",
            branch
        ));
    }

    // ── 6. Check for existing open PR on this branch ─────────────────────
    let existing = check_existing_pr(branch, ctx).await;
    if let Some(url) = existing {
        ctx.logger
            .info(&format!("Existing PR found: {} — skipping creation.", url));
        ctx.pr_url = Some(url);
        return Ok(());
    }

    // ── 7. Build title / body ────────────────────────────────────────────
    let title = ctx.pr_title.clone().unwrap_or_else(|| {
        format!(
            "sousdev: {}",
            ctx.parsed_task.task.lines().next().unwrap_or("automated fix")
        )
    });
    let mut body = ctx.pr_generated_body.clone().unwrap_or_else(|| {
        let task_summary: String = ctx.parsed_task.task.lines().take(10).collect::<Vec<_>>().join("\n");
        let agent_summary = ctx
            .agent_result
            .as_ref()
            .map(|r| {
                let answer: String = r.answer.chars().take(500).collect();
                format!("\n\n## Agent Summary\n\n{}", answer)
            })
            .unwrap_or_default();
        format!(
            "## Summary\n\n## Task\n\n{}{}\n",
            task_summary,
            agent_summary
        )
    });

    // Prepend issue link at the top if this is an issue-autofix run.
    if let Some(ref issue_url) = ctx.issue_url {
        let issue_line = format!("Closes {}\n\n", issue_url);
        body.insert_str(0, &issue_line);
    }

    // Append branding at the bottom (configurable, default true).
    let show_branding = ctx
        .config
        .pull_request
        .as_ref()
        .and_then(|pr| pr.show_branding)
        .unwrap_or(true);
    if show_branding {
        body.push_str("\n\n---\n\n🧑‍🍳 Automated by [SousDev](https://github.com/ytham/SousDev)\n");
    }

    // ── 8. Create PR ─────────────────────────────────────────────────────
    let pr_url = create_pr(ctx, &title, &body).await?;

    ctx.logger.info(&format!("PR created: {}", pr_url));
    ctx.pr_url = Some(pr_url);
    Ok(())
}

/// Agent-assisted PR creation: invoke the Claude CLI with the error context
/// and workspace, letting the agent fix git issues and create the PR itself.
async fn agent_assisted_pr(ctx: &mut StageContext) -> Result<()> {
    let dir = &ctx.workspace_dir;
    let branch = &ctx.branch;
    let repo_args = build_repo_args(ctx);
    let repo_flag = if repo_args.len() == 2 {
        format!(" --repo {}", repo_args[1])
    } else {
        String::new()
    };

    let title = ctx.pr_title.clone().unwrap_or_else(|| {
        format!(
            "sousdev: {}",
            ctx.parsed_task.task.lines().next().unwrap_or("automated fix")
        )
    });

    // Gather diagnostic info for the agent.
    let git_status = exec_git(&["status"], dir).await.unwrap_or_default();
    let git_log = exec_git(&["log", "--oneline", "-5"], dir)
        .await
        .unwrap_or_default();
    let git_branch = exec_git(&["branch", "-v"], dir)
        .await
        .unwrap_or_default();
    let git_remote = exec_git(&["remote", "-v"], dir)
        .await
        .unwrap_or_default();

    let prompt = format!(
        r#"You are in a git workspace at: {}

The automated PR creation process failed. Your job is to:
1. Ensure all changes are committed on branch "{}"
2. Push the branch to the remote (use --force if needed since this is a SousDev-managed branch)
3. Create a pull request using: gh pr create --title "{}" --body "Automated change by 🧑‍🍳 SousDev"{} --head "{}"

Current git state:
```
$ git status
{}

$ git log --oneline -5
{}

$ git branch -v
{}

$ git remote -v
{}
```

Do whatever is needed to get the branch pushed and the PR created. If a PR already exists for this branch, just output its URL.
Output ONLY the PR URL on the last line of your response."#,
        dir.display(),
        branch,
        title.replace('"', r#"\""#),
        repo_flag,
        branch,
        git_status,
        git_log,
        git_branch,
        git_remote,
    );

    ctx.logger
        .info("Invoking agent to fix git issues and create PR");

    let mut args = vec![
        "--print".to_string(),
        "--dangerously-skip-permissions".to_string(),
        "--output-format".to_string(),
        "text".to_string(),
    ];

    if let Some(ref sp) = ctx.system_prompt {
        args.push("--system-prompt".to_string());
        args.push(sp.clone());
    }

    args.push("-".to_string());

    let mut child = Command::new("claude")
        .args(&args)
        .current_dir(dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(prompt.as_bytes()).await?;
    }

    let output = match tokio::time::timeout(
        std::time::Duration::from_secs(120),
        child.wait_with_output(),
    )
    .await
    {
        Ok(result) => result?,
        Err(_) => {
            return Err(anyhow::anyhow!(
                "Agent-assisted PR creation timed out after 120s"
            ));
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow::anyhow!(
            "Agent-assisted PR creation failed: {}",
            stderr.chars().take(500).collect::<String>()
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    ctx.logger.info(&format!("Agent output: {}", stdout.trim()));

    // Extract PR URL from the agent's output.
    let re = Regex::new(r"https://github\.com/[^\s]+")?;
    if let Some(m) = re.find(&stdout) {
        let pr_url = m.as_str().trim_end_matches('\n').to_string();
        ctx.logger.info(&format!("PR created (via agent): {}", pr_url));
        ctx.pr_url = Some(pr_url);
        return Ok(());
    }

    // Check if the agent managed to push and a PR now exists.
    let branch = ctx.branch.clone();
    if let Some(url) = check_existing_pr(&branch, ctx).await {
        ctx.logger
            .info(&format!("PR found after agent intervention: {}", url));
        ctx.pr_url = Some(url);
        return Ok(());
    }

    Err(anyhow::anyhow!(
        "Agent-assisted PR creation did not produce a PR URL"
    ))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Run a `git` sub-command in `cwd` and return trimmed stdout.
async fn exec_git(args: &[&str], cwd: &Path) -> Result<String> {
    let output = Command::new("git")
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

/// Return the URL of an existing open PR for `branch`, if one exists.
async fn check_existing_pr(branch: &str, ctx: &StageContext) -> Option<String> {
    let repo_args = build_repo_args(ctx);
    let mut cmd = Command::new("gh");
    cmd.arg("pr")
        .arg("list")
        .arg("--head")
        .arg(branch)
        .arg("--state")
        .arg("open")
        .arg("--json")
        .arg("url")
        .arg("--limit")
        .arg("1")
        .current_dir(&ctx.workspace_dir);
    for a in &repo_args {
        cmd.arg(a);
    }
    let output = cmd.output().await.ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let arr: serde_json::Value = serde_json::from_str(&stdout).ok()?;
    arr.as_array()?
        .first()?
        .get("url")?
        .as_str()
        .map(|s| s.to_string())
}

/// Call `gh pr create` using a body temp file; return the PR URL.
async fn create_pr(ctx: &StageContext, title: &str, body: &str) -> Result<String> {
    // Write body to a NamedTempFile so we can pass --body-file.
    let mut tmp = NamedTempFile::new()?;
    tmp.write_all(body.as_bytes())?;
    let tmp_path = tmp.path().to_owned();

    let draft = ctx
        .config
        .pull_request
        .as_ref()
        .and_then(|pr| pr.draft)
        .unwrap_or(false);

    let base_branch = ctx
        .config
        .workspace
        .as_ref()
        .and_then(|w| w.base_branch.as_deref())
        .unwrap_or("main");

    let repo_args = build_repo_args(ctx);

    let mut cmd = Command::new("gh");
    cmd.arg("pr")
        .arg("create")
        .arg("--title")
        .arg(title)
        .arg("--body-file")
        .arg(&tmp_path)
        .arg("--head")
        .arg(&ctx.branch)
        .arg("--base")
        .arg(base_branch)
        .current_dir(&ctx.workspace_dir);

    if draft {
        cmd.arg("--draft");
    }

    // Apply label flags.
    if let Some(labels) = ctx
        .config
        .pull_request
        .as_ref()
        .and_then(|pr| pr.labels.as_deref())
    {
        for label in labels {
            cmd.arg("--label").arg(label);
        }
    }

    for a in &repo_args {
        cmd.arg(a);
    }

    let output = cmd.output().await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow::anyhow!("gh pr create failed: {}", stderr.trim()));
    }

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();

    // Extract the URL from stdout using a regex.
    let re = Regex::new(r"https://github\.com/[^\s]+")?;
    if let Some(m) = re.find(&stdout) {
        return Ok(m.as_str().trim_end_matches('\n').to_string());
    }

    // Fallback: return the trimmed raw output.
    Ok(stdout.trim().to_string())
}

/// Build `--repo <owner/repo>` args if a target repo is configured.
fn build_repo_args(ctx: &StageContext) -> Vec<String> {
    let repo = ctx
        .config
        .github_issues
        .as_ref()
        .and_then(|gi| gi.repo.as_deref())
        .or(ctx.target_repo.as_deref());
    match repo {
        Some(r) => {
            // Normalise to owner/repo.
            let normalised = crate::workflows::github_issues::repo_to_gh_identifier(Some(r))
                .unwrap_or_else(|| r.to_string());
            vec!["--repo".to_string(), normalised]
        }
        None => vec![],
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pull_request_stage_name() {
        assert_eq!(PullRequestStage.name(), "pull-request");
    }

    #[test]
    fn test_pr_url_regex() {
        let re = Regex::new(r"https://github\.com/[^\s]+").unwrap();
        let stdout = "Creating pull request...\nhttps://github.com/owner/repo/pull/42\n";
        let m = re.find(stdout).unwrap();
        assert_eq!(m.as_str(), "https://github.com/owner/repo/pull/42");
    }

    #[test]
    fn test_pr_url_regex_not_found() {
        let re = Regex::new(r"https://github\.com/[^\s]+").unwrap();
        assert!(re.find("no url here").is_none());
    }

    // ── derive_commit_message tests ───────────────────────────────────────

    /// Stub LLM provider for tests (never called).
    struct StubProvider;

    #[async_trait]
    impl crate::providers::provider::LLMProvider for StubProvider {
        fn name(&self) -> &str { "stub" }
        fn model(&self) -> &str { "stub" }
        async fn complete(
            &self,
            _messages: &[crate::providers::provider::Message],
            _options: Option<&crate::providers::provider::CompleteOptions>,
        ) -> anyhow::Result<crate::providers::provider::CompletionResult> {
            unimplemented!("stub")
        }
    }

    fn make_ctx(task: &str) -> StageContext {
        use crate::workflows::workflow::ParsedTask;
        use crate::workflows::stage::ResolvedPrompts;
        use crate::utils::logger::Logger;
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;

        StageContext {
            config: Arc::new(crate::types::config::WorkflowConfig::default()),
            provider: Arc::new(StubProvider),
            registry: Arc::new(crate::tools::registry::ToolRegistry::new()),
            workspace_dir: std::path::PathBuf::from("/tmp"),
            branch: "main".to_string(),
            parsed_task: ParsedTask::new(task),
            harness_root: std::path::PathBuf::from("/tmp"),
            prompts: ResolvedPrompts::default(),
            system_prompt: None,
            target_repo: None,
            logger: Logger::new("test"),
            run_id: "test".to_string(),
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
            extra_agent_flags: None,
            reviewing_pr: None,
            pr_review_result: None,
            responding_pr: None,
            unaddressed_comments: None,
            pr_response_result: None,
            reviewer_login: None,
            workflow_log: None,
            aborted: Arc::new(AtomicBool::new(false)),
        }
    }

    #[test]
    fn test_derive_commit_message_from_pr_title() {
        let mut ctx = make_ctx("Fix issue #42: Some bug");
        ctx.pr_title = Some("fix(auth): resolve token expiry race condition".to_string());
        assert_eq!(
            derive_commit_message(&ctx),
            "fix(auth): resolve token expiry race condition"
        );
    }

    #[test]
    fn test_derive_commit_message_from_task_first_line() {
        let ctx = make_ctx("Fix issue #42: Rename equities routes to instruments");
        assert_eq!(
            derive_commit_message(&ctx),
            "Fix issue #42: Rename equities routes to instruments"
        );
    }

    #[test]
    fn test_derive_commit_message_truncates_long_task() {
        let long_task = "Fix issue #99: ".to_string() + &"a".repeat(200);
        let ctx = make_ctx(&long_task);
        let msg = derive_commit_message(&ctx);
        assert!(msg.len() <= 100);
        assert!(msg.ends_with("..."));
    }

    #[test]
    fn test_derive_commit_message_fallback() {
        let ctx = make_ctx("");
        assert_eq!(derive_commit_message(&ctx), DEFAULT_COMMIT_MSG);
    }

    #[test]
    fn test_derive_commit_message_config_takes_priority() {
        let mut cfg = crate::types::config::WorkflowConfig::default();
        cfg.pull_request = Some(crate::types::config::PullRequestConfig {
            title: Some("custom: configured title".to_string()),
            ..Default::default()
        });
        let mut ctx = make_ctx("Fix issue #42: Some bug");
        ctx.config = std::sync::Arc::new(cfg);
        ctx.pr_title = Some("feat: generated title".to_string());
        assert_eq!(derive_commit_message(&ctx), "custom: configured title");
    }

    #[test]
    fn test_derive_commit_message_pr_title_over_task() {
        let mut ctx = make_ctx("Fix issue #42: Some bug\n\nLong body text here");
        ctx.pr_title = Some("feat(equity): add v2 instrument routes".to_string());
        assert_eq!(
            derive_commit_message(&ctx),
            "feat(equity): add v2 instrument routes"
        );
    }
}
