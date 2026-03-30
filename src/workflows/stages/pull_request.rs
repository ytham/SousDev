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

/// Commit message template used when the agent doesn't produce one.
const DEFAULT_COMMIT_MSG: &str = "chore: apply sousdev agent changes";

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
        let commit_msg = ctx
            .config
            .pull_request
            .as_ref()
            .and_then(|pr| pr.title.as_deref())
            .unwrap_or(DEFAULT_COMMIT_MSG);
        exec_git(&["commit", "-m", commit_msg], dir).await?;
        ctx.logger.info("Committed workspace changes.");
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
    let body = ctx.pr_generated_body.clone().unwrap_or_else(|| {
        format!(
            "Automated change produced by SousDev.\n\nTask:\n{}",
            ctx.parsed_task.task
        )
    });

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
3. Create a pull request using: gh pr create --title "{}" --body "Automated change by SousDev"{} --head "{}"

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
        .spawn()?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(prompt.as_bytes()).await?;
    }

    let output = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        child.wait_with_output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("Agent-assisted PR creation timed out after 120s"))??;

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
}
