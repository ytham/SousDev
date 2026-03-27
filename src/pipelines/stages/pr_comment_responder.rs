use anyhow::Result;
use async_trait::async_trait;
use tokio::process::Command;
use crate::pipelines::github_prs::{post_summary_comment, reply_to_inline_comment};
use crate::pipelines::stage::{Stage, StageContext};
use crate::pipelines::stores::PrResponseResult;

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

/// Build the reply body for a single inline review comment.
///
/// Looks for a matching mention of the file path in the agent output;
/// otherwise returns a generic acknowledgement.
fn build_reply_body(agent_output: &str, path: &str, line: Option<u64>) -> String {
    // Try to find a relevant excerpt from the agent output that mentions the
    // file being commented on.
    let relevant: Vec<&str> = agent_output
        .lines()
        .filter(|l| l.contains(path))
        .take(3)
        .collect();

    if !relevant.is_empty() {
        return format!(
            "Addressed in this update ({}:{}):\n\n{}",
            path,
            line.unwrap_or(0),
            relevant.join("\n")
        );
    }

    format!(
        "I've addressed the comment on `{}`{}. Please review the latest commit.",
        path,
        line.map(|l| format!(":{}", l)).unwrap_or_default()
    )
}

/// Build the summary comment body posted after all inline replies.
fn build_summary_body(
    agent_output: &str,
    head_sha: &str,
    inline_count: usize,
    timeline_count: usize,
) -> String {
    // If the agent output is short, include it verbatim.
    let agent_summary = if !agent_output.is_empty() {
        let truncated = if agent_output.len() > 2000 {
            format!("{}\n… (truncated)", &agent_output[..2000])
        } else {
            agent_output.to_string()
        };
        format!("\n\n**Agent summary:**\n{}", truncated)
    } else {
        String::new()
    };

    let sha_ref = if head_sha.is_empty() {
        String::new()
    } else {
        let short = &head_sha[..head_sha.len().min(7)];
        format!(" (commit `{}`)", short)
    };

    format!(
        "## Review comment response{}\n\n\
         I've addressed {} inline comment{} and {} timeline comment{}.{}\n\n\
         Please re-review at your convenience.",
        sha_ref,
        inline_count,
        if inline_count == 1 { "" } else { "s" },
        timeline_count,
        if timeline_count == 1 { "" } else { "s" },
        agent_summary
    )
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
        assert!(body.contains("src/auth.rs"));
        assert!(body.contains("null check"));
    }

    #[test]
    fn test_build_reply_body_generic() {
        let output = "Nothing relevant here.";
        let body = build_reply_body(output, "src/utils.rs", Some(5));
        assert!(body.contains("src/utils.rs"));
        assert!(body.contains("5"));
    }

    #[test]
    fn test_build_reply_body_no_line() {
        let body = build_reply_body("", "src/x.rs", None);
        assert!(body.contains("src/x.rs"));
    }

    #[test]
    fn test_build_summary_body_counts() {
        let body = build_summary_body("done", "abc1234", 3, 1);
        assert!(body.contains("3 inline comments"));
        assert!(body.contains("1 timeline comment"));
        assert!(body.contains("abc1234"));
    }

    #[test]
    fn test_build_summary_body_singular() {
        let body = build_summary_body("", "sha", 1, 1);
        assert!(body.contains("1 inline comment"));
        assert!(body.contains("1 timeline comment"));
    }

    #[test]
    fn test_build_summary_body_no_sha() {
        let body = build_summary_body("", "", 0, 0);
        assert!(!body.contains("commit"));
    }

    #[test]
    fn test_build_summary_truncates_long_output() {
        let long_output = "x".repeat(3000);
        let body = build_summary_body(&long_output, "sha", 0, 0);
        assert!(body.contains("truncated"));
    }

    #[test]
    fn test_pr_comment_responder_stage_name() {
        assert_eq!(PrCommentResponderStage.name(), "pr-comment-responder");
    }

    // ── Additional tests ─────────────────────────────────────────────────────

    #[test]
    fn test_build_summary_comment_no_inline() {
        // 0 inline, 3 timeline
        let body = build_summary_body("agent did stuff", "abc1234", 0, 3);
        assert!(body.contains("0 inline comments"));
        assert!(body.contains("3 timeline comments"));
        assert!(body.contains("agent did stuff"));
    }

    #[test]
    fn test_build_summary_comment_no_timeline() {
        // 2 inline, 0 timeline
        let body = build_summary_body("fixed it", "def5678", 2, 0);
        assert!(body.contains("2 inline comments"));
        assert!(body.contains("0 timeline comments"));
        assert!(body.contains("fixed it"));
    }

    #[test]
    fn test_build_summary_comment_empty_answer() {
        let body = build_summary_body("", "sha123", 1, 1);
        // With an empty agent answer, there should be no "Agent summary" section.
        assert!(!body.contains("Agent summary"));
        assert!(body.contains("1 inline comment"));
        assert!(body.contains("1 timeline comment"));
    }
}
