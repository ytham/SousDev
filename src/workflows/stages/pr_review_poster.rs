use anyhow::Result;
use async_trait::async_trait;
use regex::Regex;
use tokio::process::Command;
use crate::workflows::github_prs::{post_inline_comment, post_summary_comment};
use crate::workflows::stage::{Stage, StageContext};
use crate::workflows::stores::PrReviewResult;

// ---------------------------------------------------------------------------
// Block markers used by the agent in its review output
// ---------------------------------------------------------------------------

/// End marker for an inline comment block.
///
/// Expected format:
/// ```text
/// INLINE_COMMENT path/to/file.rs:42
/// <comment body>
/// END_INLINE_COMMENT
/// ```
const INLINE_COMMENT_END: &str = "END_INLINE_COMMENT";

/// Opening / closing markers for the summary block.
const SUMMARY_START: &str = "SUMMARY";
const SUMMARY_END: &str = "END_SUMMARY";

// ---------------------------------------------------------------------------
// Parsed inline comment
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct ParsedInlineComment {
    path: String,
    line: u64,
    body: String,
}

// ---------------------------------------------------------------------------
// Stage implementation
// ---------------------------------------------------------------------------

/// Posts the agent's review output to GitHub as inline diff comments and a
/// top-level summary comment.
///
/// The agent is expected to produce output in this structure:
/// ```text
/// INLINE_COMMENT src/auth.rs:42
/// The error is not being propagated here.
/// END_INLINE_COMMENT
///
/// SUMMARY
/// Overall the PR looks good but there are a few issues to address.
/// END_SUMMARY
/// ```
///
/// Every `INLINE_COMMENT` block results in one call to
/// [`post_inline_comment`].  The `SUMMARY` block is posted via
/// [`post_summary_comment`].  Errors on individual comments are collected but
/// do not abort the stage.
pub struct PrReviewPosterStage;

#[async_trait]
impl Stage for PrReviewPosterStage {
    fn name(&self) -> &str {
        "pr-review-poster"
    }

    async fn run(&self, ctx: &mut StageContext) -> Result<()> {
        if ctx.is_aborted() {
            return Ok(());
        }

        let pr = ctx.reviewing_pr.as_ref().ok_or_else(|| {
            anyhow::anyhow!("PrReviewPosterStage: reviewing_pr not set")
        })?;
        let pr = pr.clone();

        // Use the agent's answer, or fall back to the trajectory thinking
        // text if the answer is empty (e.g. agent timed out before producing
        // a final result event).
        let agent_output: String = ctx
            .agent_result
            .as_ref()
            .map(|r| {
                if !r.answer.is_empty() {
                    r.answer.clone()
                } else {
                    // Extract all thinking text from the trajectory.
                    r.trajectory
                        .iter()
                        .filter(|s| {
                            s.step_type == crate::types::technique::StepType::Thought
                        })
                        .map(|s| s.content.as_str())
                        .collect::<Vec<_>>()
                        .join("\n\n")
                }
            })
            .unwrap_or_default();
        let agent_output = agent_output.as_str();

        // Resolve the HEAD SHA of the PR branch.
        let head_sha = resolve_head_sha(&ctx.workspace_dir).await;

        // Parse inline comments and summary from agent output.
        let inline_comments = parse_inline_comments(agent_output);
        let summary = parse_summary(agent_output);

        ctx.logger.info(&format!(
            "PrReviewPosterStage: posting {} inline comment(s) to PR #{}",
            inline_comments.len(),
            pr.number
        ));

        let mut errors: Vec<String> = Vec::new();
        let mut inline_count = 0usize;

        // Post inline comments.
        for comment in &inline_comments {
            match post_inline_comment(
                &pr.repo,
                pr.number,
                &head_sha,
                &comment.path,
                comment.line,
                &comment.body,
            )
            .await
            {
                Ok(()) => {
                    inline_count += 1;
                    ctx.logger.debug(&format!(
                        "  Posted inline comment on {}:{}",
                        comment.path, comment.line
                    ));
                }
                Err(e) => {
                    let msg = format!(
                        "Failed to post inline comment on {}:{} — {}",
                        comment.path, comment.line, e
                    );
                    ctx.logger.error(&msg);
                    errors.push(msg);
                }
            }
        }

        // Post summary comment.  If no SUMMARY markers were found, fall back
        // to posting the cleaned agent output directly (this happens when the
        // agent times out or doesn't follow the exact marker format).
        let summary_to_post = summary.or_else(|| {
            if agent_output.is_empty() {
                return None;
            }
            // Extract clean text from the agent output, filtering out JSON/stream data.
            let clean_lines: Vec<&str> = agent_output
                .lines()
                .filter(|l| {
                    let t = l.trim();
                    !t.is_empty()
                        && !t.starts_with('{')
                        && !t.starts_with('[')
                        && !t.starts_with("INLINE_COMMENT")
                        && !t.starts_with("END_INLINE_COMMENT")
                })
                .collect();
            if clean_lines.is_empty() {
                None
            } else {
                let text = clean_lines.join("\n");
                let truncated = if text.len() > 4000 {
                    format!("{}…", &text[..4000])
                } else {
                    text
                };
                Some(format!(
                    "## 🧑‍🍳 PR Review\n\n{}\n",
                    truncated
                ))
            }
        });

        let summary_posted = if let Some(body) = &summary_to_post {
            ctx.logger.info("PrReviewPosterStage: posting summary comment");
            match post_summary_comment(&pr.repo, pr.number, body).await {
                Ok(()) => true,
                Err(e) => {
                    let msg = format!("Failed to post summary comment — {}", e);
                    ctx.logger.error(&msg);
                    errors.push(msg);
                    false
                }
            }
        } else {
            ctx.logger.info("PrReviewPosterStage: no review content to post");
            false
        };

        ctx.pr_review_result = Some(PrReviewResult {
            inline_comment_count: inline_count,
            summary_posted,
            head_sha,
            errors,
        });
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Resolve the current HEAD SHA in the workspace (short 7-char).
async fn resolve_head_sha(workspace_dir: &std::path::Path) -> String {
    Command::new("git")
        .arg("rev-parse")
        .arg("--short")
        .arg("HEAD")
        .current_dir(workspace_dir)
        .output()
        .await
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// Parse all `INLINE_COMMENT path:line … END_INLINE_COMMENT` blocks from
/// `text`.
fn parse_inline_comments(text: &str) -> Vec<ParsedInlineComment> {
    let mut result = Vec::new();
    let mut lines = text.lines().peekable();

    // Regex to match "INLINE_COMMENT path/to/file.rs:42"
    let header_re = Regex::new(
        r"(?i)INLINE_COMMENT\s+(.+?):(\d+)\s*$",
    )
    .unwrap();

    while let Some(line) = lines.next() {
        let line = line.trim();
        if let Some(caps) = header_re.captures(line) {
            let path = caps[1].trim().to_string();
            let line_num: u64 = caps[2].parse().unwrap_or(1);
            let mut body_lines: Vec<&str> = Vec::new();
            // Collect lines until END_INLINE_COMMENT
            for body_line in lines.by_ref() {
                if body_line.trim().eq_ignore_ascii_case(INLINE_COMMENT_END) {
                    break;
                }
                body_lines.push(body_line);
            }
            let body = body_lines.join("\n").trim().to_string();
            if !body.is_empty() {
                result.push(ParsedInlineComment {
                    path,
                    line: line_num,
                    body,
                });
            }
        }
    }
    result
}

/// Extract the content between `SUMMARY` … `END_SUMMARY` markers.
fn parse_summary(text: &str) -> Option<String> {
    let mut in_summary = false;
    let mut lines: Vec<&str> = Vec::new();

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.eq_ignore_ascii_case(SUMMARY_START) {
            in_summary = true;
            continue;
        }
        if trimmed.eq_ignore_ascii_case(SUMMARY_END) {
            break;
        }
        if in_summary {
            lines.push(line);
        }
    }

    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n").trim().to_string())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_inline_comments_single() {
        let text = "INLINE_COMMENT src/auth.rs:42\nThe error is not propagated.\nEND_INLINE_COMMENT";
        let comments = parse_inline_comments(text);
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].path, "src/auth.rs");
        assert_eq!(comments[0].line, 42);
        assert_eq!(comments[0].body, "The error is not propagated.");
    }

    #[test]
    fn test_parse_inline_comments_multiple() {
        let text = "\
INLINE_COMMENT src/a.rs:10
First comment.
END_INLINE_COMMENT

INLINE_COMMENT src/b.rs:99
Second comment.
END_INLINE_COMMENT";
        let comments = parse_inline_comments(text);
        assert_eq!(comments.len(), 2);
        assert_eq!(comments[0].path, "src/a.rs");
        assert_eq!(comments[0].line, 10);
        assert_eq!(comments[1].path, "src/b.rs");
        assert_eq!(comments[1].line, 99);
    }

    #[test]
    fn test_parse_inline_comments_multiline_body() {
        let text = "INLINE_COMMENT src/c.rs:5\nLine 1\nLine 2\nLine 3\nEND_INLINE_COMMENT";
        let comments = parse_inline_comments(text);
        assert_eq!(comments.len(), 1);
        assert!(comments[0].body.contains("Line 1"));
        assert!(comments[0].body.contains("Line 3"));
    }

    #[test]
    fn test_parse_inline_comments_none() {
        let text = "No inline comments here. SUMMARY\nAll good.\nEND_SUMMARY";
        let comments = parse_inline_comments(text);
        assert!(comments.is_empty());
    }

    #[test]
    fn test_parse_summary_present() {
        let text = "Some preamble.\nSUMMARY\nOverall the PR looks good.\nEND_SUMMARY\nExtra text.";
        let summary = parse_summary(text);
        assert_eq!(summary.as_deref(), Some("Overall the PR looks good."));
    }

    #[test]
    fn test_parse_summary_absent() {
        let text = "No summary markers here.";
        assert!(parse_summary(text).is_none());
    }

    #[test]
    fn test_parse_summary_multiline() {
        let text = "SUMMARY\nLine one.\nLine two.\nEND_SUMMARY";
        let summary = parse_summary(text);
        let s = summary.unwrap();
        assert!(s.contains("Line one."));
        assert!(s.contains("Line two."));
    }

    #[test]
    fn test_parse_inline_empty_body_skipped() {
        // Body has only whitespace → should not produce a comment.
        let text = "INLINE_COMMENT src/x.rs:1\n   \nEND_INLINE_COMMENT";
        let comments = parse_inline_comments(text);
        assert!(comments.is_empty());
    }
}
