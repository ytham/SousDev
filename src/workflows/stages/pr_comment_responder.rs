use anyhow::Result;
use async_trait::async_trait;
use tokio::process::Command;
use crate::workflows::github_prs::{post_summary_comment, reply_to_inline_comment};
use crate::workflows::stage::{Stage, StageContext};
use crate::workflows::stores::PrResponseResult;

/// Posts replies to unaddressed review comments and a summary comment
/// describing what was done.
///
/// Steps:
/// 1. Resolve the current HEAD SHA.
/// 2. For each inline comment in `ctx.unaddressed_comments.inline`, post a
///    reply via [`reply_to_inline_comment`].
/// 3. Post a summary comment (from agent output or a default) via
///    [`post_summary_comment`].
/// 4. Set `ctx.pr_response_result`.
pub struct PrCommentResponderStage;

#[async_trait]
impl Stage for PrCommentResponderStage {
    fn name(&self) -> &str {
        "pr-comment-responder"
    }

    async fn run(&self, ctx: &mut StageContext) -> Result<()> {
        if ctx.is_aborted() {
            return Ok(());
        }

        let pr = ctx.responding_pr.as_ref().ok_or_else(|| {
            anyhow::anyhow!("PrCommentResponderStage: responding_pr not set")
        })?;
        let pr = pr.clone();

        // Resolve HEAD SHA after the agent made its changes.
        let new_head_sha = resolve_head_sha(&ctx.workspace_dir).await;

        let unaddressed = ctx.unaddressed_comments.as_ref();
        let inline_comments = unaddressed
            .map(|u| u.inline.clone())
            .unwrap_or_default();
        let timeline_comments = unaddressed
            .map(|u| u.timeline.clone())
            .unwrap_or_default();

        ctx.logger.info(&format!(
            "PrCommentResponderStage: replying to {} inline + {} timeline comment(s) on PR #{}",
            inline_comments.len(),
            timeline_comments.len(),
            pr.number
        ));

        let mut errors: Vec<String> = Vec::new();
        let mut inline_replies: usize = 0;

        // ── Reply to inline comments ─────────────────────────────────────────
        for comment in &inline_comments {
            let reply_body = build_reply_body(
                ctx.agent_result
                    .as_ref()
                    .map(|r| r.answer.as_str())
                    .unwrap_or(""),
                &comment.path,
                comment.line,
            );
            match reply_to_inline_comment(&pr.repo, comment.id, &reply_body).await {
                Ok(()) => {
                    inline_replies += 1;
                    ctx.logger.debug(&format!(
                        "  Replied to inline comment #{} on {}:{}",
                        comment.id,
                        comment.path,
                        comment.line.unwrap_or(0)
                    ));
                }
                Err(e) => {
                    let msg = format!(
                        "Failed to reply to inline comment #{}: {}",
                        comment.id, e
                    );
                    ctx.logger.error(&msg);
                    errors.push(msg);
                }
            }
        }

        // ── Post summary comment ─────────────────────────────────────────────
        let summary_body = build_summary_body(
            ctx.agent_result
                .as_ref()
                .map(|r| r.answer.as_str())
                .unwrap_or(""),
            &new_head_sha,
            inline_comments.len(),
            timeline_comments.len(),
        );

        let summary_posted =
            match post_summary_comment(&pr.repo, pr.number, &summary_body).await {
                Ok(()) => {
                    ctx.logger
                        .info("PrCommentResponderStage: summary comment posted");
                    true
                }
                Err(e) => {
                    let msg = format!("Failed to post summary comment: {}", e);
                    ctx.logger.error(&msg);
                    errors.push(msg);
                    false
                }
            };

        // ── Update PR description if changes are significant ──────────────
        // Check if the diff is large enough to warrant a description update.
        let diff_stat = get_diff_stat(&ctx.workspace_dir, &pr.base_ref_name).await;
        let files_changed = diff_stat.lines().count();

        if files_changed >= 2 {
            ctx.logger.info(&format!(
                "PrCommentResponderStage: {} files changed — updating PR description",
                files_changed
            ));
            match update_pr_description(&pr.repo, pr.number, ctx).await {
                Ok(()) => {
                    ctx.logger.info("PrCommentResponderStage: PR description updated");
                }
                Err(e) => {
                    let msg = format!("Failed to update PR description: {}", e);
                    ctx.logger.info(&msg);
                    // Non-fatal — the PR is still valid without an updated description.
                }
            }
        }

        ctx.pr_response_result = Some(PrResponseResult {
            inline_replies_posted: inline_replies,
            summary_posted,
            new_head_sha,
            errors,
        });
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Resolve the full (40-char) HEAD SHA in the workspace directory.
async fn resolve_head_sha(workspace_dir: &std::path::Path) -> String {
    Command::new("git")
        .arg("rev-parse")
        .arg("HEAD")
        .current_dir(workspace_dir)
        .output()
        .await
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// Get `git diff --stat` against the base branch.
async fn get_diff_stat(workspace_dir: &std::path::Path, base_branch: &str) -> String {
    let range = format!("origin/{}...HEAD", base_branch);
    Command::new("git")
        .args(["diff", "--stat", &range])
        .current_dir(workspace_dir)
        .output()
        .await
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// Update the PR description using the Claude CLI to generate a holistic
/// summary of all changes in the PR.
async fn update_pr_description(
    repo: &str,
    pr_number: u64,
    ctx: &StageContext,
) -> Result<()> {
    use std::process::Stdio;
    use tokio::io::AsyncWriteExt;

    let base_branch = ctx
        .config
        .workspace
        .as_ref()
        .and_then(|w| w.base_branch.as_deref())
        .unwrap_or("main");

    // Get the full diff for the PR.
    let diff = Command::new("git")
        .args(["diff", &format!("origin/{}...HEAD", base_branch)])
        .current_dir(&ctx.workspace_dir)
        .output()
        .await
        .ok()
        .map(|o| {
            let raw = String::from_utf8_lossy(&o.stdout);
            if raw.len() > 30000 {
                format!("{}\n… (diff truncated)", &raw[..30000])
            } else {
                raw.to_string()
            }
        })
        .unwrap_or_default();

    let prompt = format!(
        "Write an updated pull request description for this PR.\n\n\
         The PR has been modified since it was originally opened. \
         Write a holistic description that covers ALL current changes \
         (not just the latest commit).\n\n\
         ## Current diff\n\n```\n{}\n```\n\n\
         Output ONLY the new PR body in markdown. Do not include a title line. \
         Keep it concise but comprehensive. Include:\n\
         - A summary paragraph\n\
         - A bullet list of key changes\n\
         - Any notes for reviewers",
        diff
    );

    // Call Claude CLI.
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
        .current_dir(&ctx.workspace_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(prompt.as_bytes()).await?;
    }

    let output = tokio::time::timeout(
        std::time::Duration::from_secs(90),
        child.wait_with_output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("Claude CLI timed out generating PR description"))??;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow::anyhow!("Claude CLI failed: {}", &stderr[..stderr.len().min(200)]));
    }

    let new_body = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if new_body.is_empty() {
        return Err(anyhow::anyhow!("Claude CLI returned empty description"));
    }

    // Update the PR description via gh CLI.
    let mut tmp = tempfile::NamedTempFile::new()?;
    std::io::Write::write_all(&mut tmp, new_body.as_bytes())?;
    let tmp_path = tmp.path().to_owned();

    let gh_output = Command::new("gh")
        .arg("pr")
        .arg("edit")
        .arg(pr_number.to_string())
        .arg("--repo")
        .arg(repo)
        .arg("--body-file")
        .arg(&tmp_path)
        .output()
        .await?;

    if !gh_output.status.success() {
        let stderr = String::from_utf8_lossy(&gh_output.stderr);
        return Err(anyhow::anyhow!("gh pr edit failed: {}", stderr));
    }

    Ok(())
}

/// Build the reply body for a single inline review comment.
///
/// Looks for a matching mention of the file path in the agent output;
/// otherwise returns a generic acknowledgement.
fn build_reply_body(agent_output: &str, path: &str, _line: Option<u64>) -> String {
    let clean = extract_clean_summary(agent_output);

    // Try to find a relevant excerpt that mentions the file.
    let relevant: Vec<&str> = clean
        .lines()
        .filter(|l| l.contains(path) || l.contains(&path.replace('/', "")))
        .take(3)
        .collect();

    if !relevant.is_empty() {
        return format!(
            "Addressed in the latest commit:\n\n{}",
            relevant.join("\n")
        );
    }

    format!(
        "Addressed the comment on `{}`. Please review the latest commit.",
        path,
    )
}

/// Build the summary comment body posted after all inline replies.
///
/// Produces a clean markdown response. The agent output is parsed for
/// bullet-point summaries; raw JSON or stream data is filtered out.
fn build_summary_body(
    agent_output: &str,
    head_sha: &str,
    inline_count: usize,
    timeline_count: usize,
) -> String {
    let sha_ref = if head_sha.is_empty() {
        String::new()
    } else {
        let short = &head_sha[..head_sha.len().min(7)];
        format!(" (`{}`)", short)
    };

    let total = inline_count + timeline_count;
    let header = format!(
        "## 🧑‍🍳 Review comments addressed{}\n\nAddressed {} comment{}.\n",
        sha_ref,
        total,
        if total == 1 { "" } else { "s" },
    );

    // Extract clean summary lines from the agent output.
    // The agent is prompted to output "one bullet per comment addressed".
    // Filter out JSON blobs, empty lines, and non-summary content.
    let summary = extract_clean_summary(agent_output);

    if summary.is_empty() {
        format!(
            "{}Please re-review at your convenience.",
            header
        )
    } else {
        format!(
            "{}\n### Changes\n\n{}\n\n\
             Please re-review at your convenience.",
            header, summary
        )
    }
}

/// Extract clean summary lines from the agent's raw output.
///
/// Filters out JSON blobs, stream-json events, empty lines, and other
/// non-human-readable content.  Keeps markdown-formatted text, bullet
/// points, and plain English sentences.
fn extract_clean_summary(agent_output: &str) -> String {
    if agent_output.is_empty() {
        return String::new();
    }

    let mut clean_lines: Vec<String> = Vec::new();

    for line in agent_output.lines() {
        let trimmed = line.trim();
        // Skip empty lines.
        if trimmed.is_empty() {
            if !clean_lines.is_empty() {
                clean_lines.push(String::new());
            }
            continue;
        }
        // Skip JSON blobs and stream-json events.
        if trimmed.starts_with('{') || trimmed.starts_with('[') {
            continue;
        }
        // Skip lines that look like raw data.
        if trimmed.starts_with("\"type\"") || trimmed.contains("\"message\":{") {
            continue;
        }
        // Keep everything else (markdown, bullets, plain text).
        clean_lines.push(trimmed.to_string());
    }

    // Remove trailing empty lines.
    while clean_lines.last().map(|l| l.is_empty()).unwrap_or(false) {
        clean_lines.pop();
    }

    // Truncate to a reasonable length.
    let result = clean_lines.join("\n");
    if result.len() > 3000 {
        crate::utils::truncate::safe_truncate(&result, 3000)
    } else {
        result
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_reply_body_with_relevant_line() {
        let output = "Fixed src/auth.rs by removing the null check.\nOther stuff.";
        let body = build_reply_body(output, "src/auth.rs", Some(10));
        assert!(body.contains("null check"));
        assert!(body.contains("latest commit"));
    }

    #[test]
    fn test_build_reply_body_generic() {
        let output = "Nothing relevant here.";
        let body = build_reply_body(output, "src/utils.rs", Some(5));
        assert!(body.contains("src/utils.rs"));
    }

    #[test]
    fn test_build_reply_body_no_line() {
        let body = build_reply_body("", "src/x.rs", None);
        assert!(body.contains("src/x.rs"));
    }

    #[test]
    fn test_build_summary_body_with_content() {
        let body = build_summary_body("- Fixed the null check\n- Updated tests", "abc1234", 1, 1);
        assert!(body.contains("2 comments"));
        assert!(body.contains("abc1234"));
        assert!(body.contains("Fixed the null check"));
        assert!(body.contains("### Changes"));
    }

    #[test]
    fn test_build_summary_body_empty_agent_output() {
        let body = build_summary_body("", "sha123", 1, 1);
        assert!(body.contains("2 comments"));
        assert!(!body.contains("### Changes"));
        assert!(body.contains("re-review"));
    }

    #[test]
    fn test_build_summary_body_no_sha() {
        let body = build_summary_body("done", "", 0, 0);
        assert!(!body.contains('`'));
    }

    #[test]
    fn test_pr_comment_responder_stage_name() {
        assert_eq!(PrCommentResponderStage.name(), "pr-comment-responder");
    }

    #[test]
    fn test_extract_clean_summary_filters_json() {
        let output = r#"{"type":"result","data":"blah"}
- Fixed the bug
[1,2,3]
- Updated the tests
"type":"assistant"
Done."#;
        let summary = extract_clean_summary(output);
        assert!(summary.contains("Fixed the bug"));
        assert!(summary.contains("Updated the tests"));
        assert!(summary.contains("Done."));
        assert!(!summary.contains("result"));
        assert!(!summary.contains("[1,2,3]"));
    }

    #[test]
    fn test_extract_clean_summary_empty() {
        assert_eq!(extract_clean_summary(""), "");
    }

    #[test]
    fn test_extract_clean_summary_all_json() {
        let output = r#"{"type":"result"}
{"data":"value"}"#;
        assert_eq!(extract_clean_summary(output), "");
    }

    #[test]
    fn test_extract_clean_summary_truncates() {
        let long = "x".repeat(4000);
        let summary = extract_clean_summary(&long);
        assert!(summary.len() <= 3010); // 3000 + "…" (multi-byte)
    }
}
