use anyhow::Result;
use async_trait::async_trait;
use regex::Regex;
use tokio::process::Command;
use crate::workflows::github_prs::post_summary_comment;
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

        // Check if the agent posted a formal review directly (via `gh pr review`).
        // This is explicitly prohibited in the prompt, but the agent may ignore
        // the instruction.  If it happened, dismiss the formal review and skip
        // posting our own comment to avoid duplicates (the dismissed review's
        // body is still visible on the PR timeline).
        let agent_review_id = find_agent_formal_review(&pr.repo, pr.number).await;
        if let Some(review_id) = agent_review_id {
            ctx.logger.info(&format!(
                "PrReviewPosterStage: agent posted a formal review (id={}) — dismissing and skipping harness comment",
                review_id
            ));
            dismiss_review(&pr.repo, pr.number, review_id).await;
            ctx.pr_review_result = Some(PrReviewResult {
                inline_comment_count: 0,
                summary_posted: true,
                head_sha: resolve_head_sha(&ctx.workspace_dir).await,
                errors: vec![],
            });
            return Ok(());
        }

        // Resolve the HEAD SHA of the PR branch.
        let head_sha = resolve_head_sha(&ctx.workspace_dir).await;

        // Parse inline comments and summary from agent output.
        let inline_comments = parse_inline_comments(agent_output);
        let summary = parse_summary(agent_output);

        ctx.logger.info(&format!(
            "PrReviewPosterStage: found {} inline observation(s) for PR #{}",
            inline_comments.len(),
            pr.number
        ));

        let mut errors: Vec<String> = Vec::new();
        let inline_count = inline_comments.len();

        // Build a single combined comment with summary + inline observations.
        // Posted as a timeline comment (NOT a formal review) so it doesn't
        // count as an approval or request changes.
        let mut body_parts: Vec<String> = Vec::new();

        // Summary section.
        let summary_text = summary.or_else(|| {
            if agent_output.is_empty() {
                return None;
            }
            let clean_lines: Vec<&str> = agent_output
                .lines()
                .filter(|l| {
                    let t = l.trim();
                    // Keep blank lines — they are critical for Markdown
                    // spacing (paragraph breaks, horizontal rules, etc.).
                    if t.is_empty() {
                        return true;
                    }
                    !t.starts_with('{')
                        && !t.starts_with('[')
                        && !t.starts_with("INLINE_COMMENT")
                        && !t.starts_with("END_INLINE_COMMENT")
                })
                .collect();
            // Strip agent meta-commentary about permissions or posting.
            // The agent may prepend lines like "I don't have permission to
            // submit reviews directly" when --disallowedTools blocks it.
            // Find the first line that looks like actual review content
            // (starts with #, *, -, **, or a review heading pattern) and
            // drop everything before it.
            let clean_lines = strip_agent_preamble(&clean_lines);
            if clean_lines.is_empty() {
                None
            } else {
                let text = clean_lines.join("\n");
                // Fix Markdown spacing: ensure blank lines around horizontal
                // rules, headings, and code blocks so GitHub renders correctly.
                let text = fix_markdown_spacing(&text);
                // Collapse runs of 3+ blank lines to 2.
                let text = collapse_excess_blank_lines(&text);
                // GitHub comment limit is 65,536 chars.  Use 60,000 to leave
                // room for the header and inline sections appended later.
                Some(crate::utils::truncate::safe_truncate(&text, 60000))
            }
        });

        if let Some(ref text) = summary_text {
            body_parts.push(format!("## 🧑‍🍳 PR Review\n\n{}", text));
        }

        // Inline observations section (formatted as markdown, not formal review comments).
        if !inline_comments.is_empty() {
            let mut inline_section = String::from("\n### Inline observations\n");
            for comment in &inline_comments {
                inline_section.push_str(&format!(
                    "\n**`{}:{}`**\n{}\n",
                    comment.path, comment.line, comment.body
                ));
            }
            body_parts.push(inline_section);
        }

        let summary_posted = if !body_parts.is_empty() {
            let full_body = body_parts.join("\n");
            ctx.logger.info("PrReviewPosterStage: posting review comment");
            match post_summary_comment(&pr.repo, pr.number, &full_body).await {
                Ok(()) => true,
                Err(e) => {
                    let msg = format!("Failed to post review comment — {}", e);
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

/// Strip agent meta-commentary preamble from the review output.
///
/// When `--disallowedTools` blocks `gh pr review`, the agent often prepends
/// lines like "I don't have permission to submit reviews directly. Here's my
/// review:" before the actual review content.  This function finds the first
/// line that looks like real review content and drops everything before it.
/// Fix Markdown spacing issues that cause incorrect rendering on GitHub.
///
/// - Ensures `---` / `***` / `___` horizontal rules have blank lines before
///   and after (otherwise GitHub may interpret them as setext heading underlines).
/// - Ensures `#` headings have a blank line before them (except at the start).
/// - Ensures code fences (```) have blank lines around them.
fn fix_markdown_spacing(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut result: Vec<String> = Vec::with_capacity(lines.len() + 20);

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        let prev_blank = i == 0 || lines[i - 1].trim().is_empty();

        // Horizontal rules: ---, ***, ___  (possibly with spaces between)
        let is_hr = !trimmed.is_empty()
            && (trimmed.starts_with("---") || trimmed.starts_with("***") || trimmed.starts_with("___"))
            && trimmed.chars().all(|c| c == '-' || c == '*' || c == '_' || c == ' ');

        // Headings: # ...
        let is_heading = trimmed.starts_with('#');

        // Code fences: ``` or ~~~
        let is_fence = trimmed.starts_with("```") || trimmed.starts_with("~~~");

        // Insert blank line before if needed.
        if (is_hr || is_heading || is_fence) && !prev_blank && i > 0 {
            result.push(String::new());
        }

        result.push(line.to_string());

        // Insert blank line after horizontal rules and code fences if needed.
        if (is_hr || is_fence) && i + 1 < lines.len() && !lines[i + 1].trim().is_empty() {
            result.push(String::new());
        }
    }

    result.join("\n")
}

/// Collapse runs of 3+ consecutive blank lines to exactly 2.
fn collapse_excess_blank_lines(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut consecutive_blanks = 0u32;

    for line in text.lines() {
        if line.trim().is_empty() {
            consecutive_blanks += 1;
            if consecutive_blanks <= 2 {
                result.push('\n');
            }
        } else {
            consecutive_blanks = 0;
            result.push_str(line);
            result.push('\n');
        }
    }

    result.trim_end_matches('\n').to_string()
}

fn strip_agent_preamble<'a>(lines: &[&'a str]) -> Vec<&'a str> {
    // Patterns that indicate the start of actual review content.
    let is_review_content = |line: &str| -> bool {
        let t = line.trim();
        t.starts_with('#')          // Markdown heading
            || t.starts_with("**")  // Bold text (common in structured reviews)
            || t.starts_with("| ")  // Table row
            || t.starts_with("- ")  // List item
            || t.starts_with("* ")  // List item
            || t.starts_with("1.")  // Numbered list
            || t.starts_with("---") // Horizontal rule / separator
            || t.starts_with("```") // Code block
    };

    if let Some(start) = lines.iter().position(|l| is_review_content(l)) {
        lines[start..].to_vec()
    } else {
        // No clear review content found — return everything as-is.
        lines.to_vec()
    }
}

/// Check if the authenticated user posted a formal review (APPROVED or
/// CHANGES_REQUESTED) on this PR within the last 15 minutes.  Returns the
/// review ID if found, so the caller can dismiss it.
async fn find_agent_formal_review(repo: &str, pr_number: u64) -> Option<u64> {
    let login = crate::workflows::github_prs::detect_github_login()
        .await
        .unwrap_or_default();
    if login.is_empty() {
        return None;
    }

    // Fetch ALL reviews (not just those with a body — we need to find
    // APPROVED reviews which may have empty bodies).
    let endpoint = format!("/repos/{}/pulls/{}/reviews?per_page=100", repo, pr_number);
    let output = Command::new("gh")
        .arg("api")
        .arg("--paginate")
        .arg(&endpoint)
        .output()
        .await
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let reviews: Vec<serde_json::Value> = serde_json::from_str(&stdout).ok()?;

    let cutoff = chrono::Utc::now() - chrono::Duration::minutes(15);

    for review in reviews.iter().rev() {
        let review_login = review["user"]["login"].as_str().unwrap_or("");
        let state = review["state"].as_str().unwrap_or("");
        let submitted_at = review["submitted_at"].as_str().unwrap_or("");

        if review_login != login {
            continue;
        }

        // Only care about formal approvals or changes_requested — not COMMENTED.
        if state != "APPROVED" && state != "CHANGES_REQUESTED" {
            continue;
        }

        // Only recent reviews (within the current run window).
        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(submitted_at) {
            if dt > cutoff {
                return review["id"].as_u64();
            }
        }
    }

    None
}

/// Dismiss a formal review by ID so it no longer counts as an approval.
async fn dismiss_review(repo: &str, pr_number: u64, review_id: u64) {
    let endpoint = format!(
        "/repos/{}/pulls/{}/reviews/{}/dismissals",
        repo, pr_number, review_id
    );
    let result = Command::new("gh")
        .arg("api")
        .arg("--method")
        .arg("PUT")
        .arg(&endpoint)
        .arg("-f")
        .arg("message=Automated review dismissed — the harness posts reviews as comments, not formal approvals.")
        .output()
        .await;

    match result {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // Dismissal may fail if the repo doesn't allow it — that's OK.
            eprintln!(
                "Warning: failed to dismiss review {} on PR #{}: {}",
                review_id, pr_number, stderr.trim()
            );
        }
        Err(e) => {
            eprintln!(
                "Warning: failed to dismiss review {} on PR #{}: {}",
                review_id, pr_number, e
            );
        }
    }
}

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

    // ── strip_agent_preamble tests ────────────────────────────────────────

    #[test]
    fn test_strip_preamble_with_permission_message() {
        let lines = vec![
            "It seems I don't have permission to submit comments/reviews to GitHub directly.",
            "Here's my complete review of PR #12347:",
            "---",
            "## PR Review: refactor(rollout): simplify ExecuteDeps",
            "**Verdict: LGTM** ✅",
        ];
        let result = strip_agent_preamble(&lines);
        assert_eq!(result[0], "---");
        assert_eq!(result[1], "## PR Review: refactor(rollout): simplify ExecuteDeps");
    }

    #[test]
    fn test_strip_preamble_no_preamble() {
        let lines = vec![
            "## PR Review: clean code",
            "**Summary**: Looks good.",
            "- Minor style issue on line 42",
        ];
        let result = strip_agent_preamble(&lines);
        assert_eq!(result.len(), 3);
        assert_eq!(result[0], "## PR Review: clean code");
    }

    #[test]
    fn test_strip_preamble_all_plain_text() {
        let lines = vec![
            "This is a plain text review.",
            "Everything looks good to me.",
        ];
        let result = strip_agent_preamble(&lines);
        // No review-content markers found — return as-is.
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_strip_preamble_empty() {
        let lines: Vec<&str> = vec![];
        let result = strip_agent_preamble(&lines);
        assert!(result.is_empty());
    }

    // ── fix_markdown_spacing tests ────────────────────────────────────────

    #[test]
    fn test_fix_markdown_spacing_hr_needs_blank_lines() {
        let input = "Some text\n---\nMore text";
        let result = fix_markdown_spacing(input);
        assert!(result.contains("Some text\n\n---\n\nMore text"));
    }

    #[test]
    fn test_fix_markdown_spacing_hr_already_spaced() {
        let input = "Some text\n\n---\n\nMore text";
        let result = fix_markdown_spacing(input);
        // Should not double the blank lines.
        assert_eq!(result, "Some text\n\n---\n\nMore text");
    }

    #[test]
    fn test_fix_markdown_spacing_heading_needs_blank_line_before() {
        let input = "Some text\n### Heading\nBody";
        let result = fix_markdown_spacing(input);
        assert!(result.contains("Some text\n\n### Heading"));
    }

    #[test]
    fn test_fix_markdown_spacing_code_fence() {
        let input = "Text\n```\ncode\n```\nMore text";
        let result = fix_markdown_spacing(input);
        assert!(result.contains("Text\n\n```\n\ncode\n\n```\n\nMore text"));
    }

    #[test]
    fn test_fix_markdown_spacing_preserves_normal_text() {
        let input = "Line one\nLine two\nLine three";
        let result = fix_markdown_spacing(input);
        assert_eq!(result, input);
    }

    #[test]
    fn test_collapse_excess_blank_lines() {
        let input = "A\n\n\n\n\nB";
        let result = collapse_excess_blank_lines(input);
        assert_eq!(result, "A\n\n\nB");
    }

    #[test]
    fn test_collapse_preserves_double_blanks() {
        let input = "A\n\nB";
        let result = collapse_excess_blank_lines(input);
        assert_eq!(result, "A\n\nB");
    }
}
