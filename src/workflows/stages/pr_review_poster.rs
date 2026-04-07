use anyhow::Result;
use async_trait::async_trait;
use regex::Regex;
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

/// A parsed inline comment from agent review output.
#[derive(Debug, Clone)]
pub struct ParsedInlineComment {
    /// File path relative to the repo root.
    pub path: String,
    /// Line number in the new version of the file.
    pub line: u64,
    /// Comment body text.
    pub body: String,
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

        // NOTE: We previously had logic here to detect and dismiss formal
        // reviews posted by the agent.  This was removed because:
        // 1. --disallowedTools now prevents the agent from calling `gh pr review`
        // 2. The dismiss logic could not distinguish agent-posted approvals from
        //    manual human approvals, causing legitimate approvals to be dismissed

        // Use the PR's HEAD SHA from the GitHub API (full SHA, not abbreviated).
        // This is required for posting inline comments — abbreviated SHAs fail.
        let head_sha = pr.head_ref_oid.clone();

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

        let show_branding = ctx
            .config
            .pull_request
            .as_ref()
            .and_then(|pr| pr.show_branding)
            .unwrap_or(true);

        if let Some(ref text) = summary_text {
            let header = if show_branding {
                "## 🧑‍🍳 PR Review"
            } else {
                "## PR Review"
            };
            body_parts.push(format!("{}\n\n{}", header, text));
        }

        // Inline observations: include a summary in the timeline comment AND
        // post each as an actual inline PR comment on the specific line.
        if !inline_comments.is_empty() {
            let mut inline_section = String::from("\n### Inline observations\n");
            for comment in &inline_comments {
                inline_section.push_str(&format!(
                    "\n**`{}:{}`**\n{}\n",
                    comment.path, comment.line, comment.body
                ));
            }
            body_parts.push(inline_section);

            // Post each inline comment as an actual PR review comment on the diff.
            for comment in &inline_comments {
                let inline_body = if show_branding {
                    format!("🧑‍🍳 {}", comment.body)
                } else {
                    comment.body.clone()
                };
                match crate::workflows::github_prs::post_inline_comment(
                    &pr.repo,
                    pr.number,
                    &head_sha,
                    &comment.path,
                    comment.line,
                    &inline_body,
                )
                .await
                {
                    Ok(()) => {
                        ctx.logger.info(&format!(
                            "Posted inline comment on {}:{}",
                            comment.path, comment.line
                        ));
                    }
                    Err(e) => {
                        // Non-fatal: the comment is still in the timeline summary.
                        // Common failure: line number doesn't exist in the diff.
                        ctx.logger.warn(&format!(
                            "Could not post inline comment on {}:{} — {}",
                            comment.path, comment.line, e
                        ));
                    }
                }
            }
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

        // Parse the verdict and score from the review text.
        let verdict = parse_verdict(agent_output);
        let score = parse_score(agent_output);

        ctx.pr_review_result = Some(PrReviewResult {
            inline_comment_count: inline_count,
            summary_posted,
            head_sha,
            errors,
            verdict,
            score,
        });
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Strip leading emoji characters and whitespace from a line.
///
/// Handles branding prefixes like `🧑‍🍳 `, `📊 `, and other common emoji.
fn strip_leading_emojis(s: &str) -> &str {
    let mut start = 0;
    let bytes = s.as_bytes();
    while start < s.len() {
        // Skip whitespace.
        if bytes[start] == b' ' || bytes[start] == b'\t' {
            start += 1;
            continue;
        }
        // ASCII characters (letters, digits, punctuation) — stop stripping.
        if bytes[start].is_ascii_alphanumeric() || bytes[start] == b'#' || bytes[start] == b'*' || bytes[start] == b'-' || bytes[start] == b'|' {
            break;
        }
        // Non-ASCII = likely emoji. Skip the full UTF-8 character.
        let ch = s[start..].chars().next().unwrap();
        start += ch.len_utf8();
    }
    &s[start..]
}

/// Parse the score from review text. Public alias for use by the executor.
pub fn parse_score_from_text(text: &str) -> Option<u32> {
    parse_score(text)
}

/// Parse inline comments from review text. Public alias for use by the executor.
///
/// Tries structured `INLINE_COMMENT` markers first; falls back to parsing
/// markdown bold `**path:line**` format if no structured markers found.
pub fn parse_inline_comments_from_text(text: &str) -> Vec<ParsedInlineComment> {
    let structured = parse_inline_comments(text);
    if !structured.is_empty() {
        return structured;
    }
    // Fallback: parse markdown bold format.
    parse_markdown_inline_comments(text)
}

/// Parse the score (0-100) from the review text.
///
/// Looks for a line matching `Score: <N>` (case-insensitive).
/// Returns `None` if no score line is found or the value is out of range.
fn parse_score(text: &str) -> Option<u32> {
    for line in text.lines().rev() {
        let trimmed = line.trim().to_lowercase();
        let stripped = strip_leading_emojis(&trimmed);
        if let Some(rest) = stripped.strip_prefix("score:")
            .or_else(|| stripped.strip_prefix("avg score:")) {
            let rest = rest.trim();
            // Parse the number, ignoring anything after it (like "/100").
            let num_str: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(n) = num_str.parse::<u32>() {
                if n <= 100 {
                    return Some(n);
                }
            }
        }
    }
    None
}

/// Strip agent meta-commentary preamble from the review output.
///
/// When `--disallowedTools` blocks `gh pr review`, the agent often prepends
/// lines like "I don't have permission to submit reviews directly. Here's my
/// review:" before the actual review content.  This function finds the first
/// line that looks like real review content and drops everything before it.
/// Parse the verdict from the review text.
///
/// Looks for a line matching `Verdict: Approved` or `Verdict: Not Approved`
/// (case-insensitive).  Returns `"approved"`, `"not_approved"`, or `"unknown"`.
/// Parse the verdict from review text. Public alias for use by the executor.
pub fn parse_verdict_from_text(text: &str) -> String {
    parse_verdict(text)
}

fn parse_verdict(text: &str) -> String {
    for line in text.lines().rev() {
        let trimmed = line.trim().to_lowercase();
        // Strip leading emoji/branding prefixes before checking for "verdict:".
        let stripped = strip_leading_emojis(&trimmed);
        if let Some(rest_raw) = stripped.strip_prefix("verdict:") {
            // Strip emoji prefixes and whitespace.
            let rest = rest_raw
                .trim()
                .trim_start_matches('✅')
                .trim_start_matches('❌')
                .trim_start_matches('✅')
                .trim_start_matches('🔴')
                .trim();
            if rest.starts_with("approved") || rest == "lgtm" || rest == "looks good" {
                return "approved".to_string();
            }
            if rest.starts_with("not approved") || rest.starts_with("not_approved")
                || rest.starts_with("rejected") || rest.starts_with("changes requested")
            {
                return "not_approved".to_string();
            }
            // Unrecognized verdict text — return as-is normalized.
            return if rest.contains("approv") {
                "approved".to_string()
            } else {
                "not_approved".to_string()
            };
        }
    }
    "unknown".to_string()
}

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



/// Parse inline comments from markdown formats commonly used by models:
///
/// - `**path:line**` or `**\`path:line\`**` (bold, with/without backticks)
/// - `- \`path:line\`` (list item with backticks)
/// - `- **\`path:line\`**` (list item with bold + backticks)
/// - `**\`path:line\`** — description` (inline description)
/// - `- \`path:line\` — description` (list with inline description)
///
/// This is a fallback parser for when models don't use the structured
/// `INLINE_COMMENT` markers but instead use markdown formatting.
fn parse_markdown_inline_comments(text: &str) -> Vec<ParsedInlineComment> {
    let mut result = Vec::new();
    // Match file:line in various markdown wrappers.
    // Group 1: file path (with extension), Group 2: line number, Group 3: trailing text on same line
    let re = Regex::new(
        r"(?m)^[-*\s]*\*{0,2}`?([^`*\s:]+\.\w+):(\d+)`?\*{0,2}[:\s—–\-]*(.*?)$"
    ).unwrap();

    for caps in re.captures_iter(text) {
        let path = caps[1].trim().to_string();
        let line: u64 = caps[2].parse().unwrap_or(0);
        let mut body = caps[3].trim().to_string();

        if line == 0 || path.is_empty() {
            continue;
        }

        // If the body is empty, try to get text from the next non-empty lines
        // (the comment may be on the following line).
        if body.is_empty() {
            // Look for the body after this match in the original text.
            let match_end = caps.get(0).unwrap().end();
            let remaining = &text[match_end..];
            let mut body_lines = Vec::new();
            for next_line in remaining.lines() {
                let trimmed = next_line.trim();
                if trimmed.is_empty() {
                    break;
                }
                // Stop if we hit another bold path:line or a heading.
                if trimmed.starts_with("**") || trimmed.starts_with('#') || trimmed.starts_with('|') {
                    break;
                }
                body_lines.push(trimmed);
            }
            body = body_lines.join(" ");
        }

        if !body.is_empty() {
            result.push(ParsedInlineComment { path, line, body });
        }
    }

    result
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
                let stripped = strip_heading_prefix(body_line.trim());
                if stripped.eq_ignore_ascii_case(INLINE_COMMENT_END) {
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
/// Strip leading markdown heading characters (`#`, spaces) from a line.
fn strip_heading_prefix(line: &str) -> &str {
    line.trim().trim_start_matches('#').trim()
}

fn parse_summary(text: &str) -> Option<String> {
    let mut in_summary = false;
    let mut lines: Vec<&str> = Vec::new();

    for line in text.lines() {
        let trimmed = line.trim();
        let stripped = strip_heading_prefix(trimmed);
        if !in_summary && stripped.eq_ignore_ascii_case(SUMMARY_START) {
            in_summary = true;
            continue;
        }
        if in_summary && stripped.eq_ignore_ascii_case(SUMMARY_END) {
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
    fn test_parse_summary_with_heading_prefix() {
        // Agent may output ### SUMMARY instead of bare SUMMARY.
        let text = "### INLINE COMMENTS\nSome text.\n### SUMMARY\nReview body.\nVerdict: Approved\n### END_SUMMARY";
        let summary = parse_summary(text);
        assert!(summary.is_some());
        let s = summary.unwrap();
        assert!(s.contains("Review body."));
        assert!(s.contains("Verdict: Approved"));
    }

    #[test]
    fn test_parse_summary_with_hash_prefix() {
        let text = "## SUMMARY\nContent here.\n## END_SUMMARY";
        let summary = parse_summary(text).unwrap();
        assert!(summary.contains("Content here."));
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

    // ── parse_verdict tests ───────────────────────────────────────────────

    #[test]
    fn test_parse_verdict_approved() {
        assert_eq!(parse_verdict("Some review.\n\nVerdict: Approved"), "approved");
        assert_eq!(parse_verdict("Verdict: approved"), "approved");
        assert_eq!(parse_verdict("VERDICT: APPROVED"), "approved");
    }

    #[test]
    fn test_parse_verdict_not_approved() {
        assert_eq!(
            parse_verdict("Issues found.\n\nVerdict: Not Approved"),
            "not_approved"
        );
        assert_eq!(parse_verdict("Verdict: not approved"), "not_approved");
        assert_eq!(parse_verdict("Verdict: Not_Approved"), "not_approved");
        assert_eq!(parse_verdict("Verdict: Changes Requested"), "not_approved");
    }

    #[test]
    fn test_parse_verdict_lgtm() {
        assert_eq!(parse_verdict("Verdict: LGTM"), "approved");
    }

    #[test]
    fn test_parse_verdict_unknown() {
        assert_eq!(parse_verdict("No verdict in this review"), "unknown");
        assert_eq!(parse_verdict(""), "unknown");
    }

    #[test]
    fn test_parse_verdict_in_summary_block() {
        let text = "SUMMARY\nLooks good overall.\n\nVerdict: Approved\nEND_SUMMARY";
        assert_eq!(parse_verdict(text), "approved");
    }

    #[test]
    fn test_parse_verdict_with_emoji() {
        assert_eq!(parse_verdict("Verdict: ✅ Approved"), "approved");
        assert_eq!(parse_verdict("Verdict: ❌ Not Approved"), "not_approved");
        assert_eq!(parse_verdict("Verdict: ✅ Approved"), "approved");
        assert_eq!(parse_verdict("Verdict: 🔴 Not Approved"), "not_approved");
    }

    #[test]
    fn test_parse_verdict_with_branding_prefix() {
        assert_eq!(parse_verdict("🧑\u{200d}🍳 Verdict: ✅ Approved"), "approved");
        assert_eq!(parse_verdict("🧑\u{200d}🍳 Verdict: 🔴 Not Approved"), "not_approved");
    }

    #[test]
    fn test_parse_score_with_branding_prefix() {
        assert_eq!(parse_score("📊 Avg Score: 93.0"), Some(93));
        assert_eq!(parse_score("📊 Score: 85"), Some(85));
    }

    #[test]
    fn test_strip_leading_emojis() {
        assert_eq!(strip_leading_emojis("🧑\u{200d}🍳 Verdict: ok"), "Verdict: ok");
        assert_eq!(strip_leading_emojis("📊 Score: 85"), "Score: 85");
        assert_eq!(strip_leading_emojis("Verdict: ok"), "Verdict: ok");
        assert_eq!(strip_leading_emojis("  Verdict: ok"), "Verdict: ok");
    }

    // ── parse_score tests ─────────────────────────────────────────────────

    #[test]
    fn test_parse_score_basic() {
        assert_eq!(parse_score("Score: 87\nVerdict: Approved"), Some(87));
        assert_eq!(parse_score("Score: 100"), Some(100));
        assert_eq!(parse_score("Score: 0"), Some(0));
        assert_eq!(parse_score("score: 72"), Some(72));
    }

    #[test]
    fn test_parse_score_with_suffix() {
        assert_eq!(parse_score("Score: 85/100"), Some(85));
        assert_eq!(parse_score("Score: 90 out of 100"), Some(90));
    }

    #[test]
    fn test_parse_score_missing() {
        assert_eq!(parse_score("No score here"), None);
        assert_eq!(parse_score(""), None);
    }

    #[test]
    fn test_parse_score_out_of_range() {
        assert_eq!(parse_score("Score: 150"), None);
        assert_eq!(parse_score("Score: 999"), None);
    }

    #[test]
    fn test_parse_score_in_summary_block() {
        let text = "SUMMARY\nLooks good.\n\nScore: 88\nVerdict: Approved\nEND_SUMMARY";
        assert_eq!(parse_score(text), Some(88));
    }

    // ── parse_markdown_inline_comments tests ──────────────────────────────

    #[test]
    fn test_parse_markdown_inline_bold_backtick() {
        let text = "**`src/auth.rs:42`**\nMissing null check on user.\n\n**`src/main.rs:10`**\nUnused import.";
        let comments = parse_markdown_inline_comments(text);
        assert_eq!(comments.len(), 2);
        assert_eq!(comments[0].path, "src/auth.rs");
        assert_eq!(comments[0].line, 42);
        assert!(comments[0].body.contains("Missing null check"));
        assert_eq!(comments[1].path, "src/main.rs");
        assert_eq!(comments[1].line, 10);
    }

    #[test]
    fn test_parse_markdown_inline_bold_no_backtick() {
        let text = "**src/auth.rs:42** — Missing null check.";
        let comments = parse_markdown_inline_comments(text);
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].path, "src/auth.rs");
        assert_eq!(comments[0].line, 42);
        assert!(comments[0].body.contains("Missing null check"));
    }

    #[test]
    fn test_parse_markdown_inline_with_description_label() {
        let text = "**`rust/services/src/runner.rs:247`**\n**Efficiency concern (single-model finding):** Some description here.";
        let comments = parse_markdown_inline_comments(text);
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].path, "rust/services/src/runner.rs");
        assert_eq!(comments[0].line, 247);
    }

    #[test]
    fn test_parse_markdown_inline_list_backtick() {
        let text = "- `equity_documents_repo.rs:9` — Doc comment placed after\n- `equity_documents_repo.rs:180` — todo!() now reachable";
        let comments = parse_markdown_inline_comments(text);
        assert_eq!(comments.len(), 2);
        assert_eq!(comments[0].path, "equity_documents_repo.rs");
        assert_eq!(comments[0].line, 9);
        assert!(comments[0].body.contains("Doc comment"));
        assert_eq!(comments[1].path, "equity_documents_repo.rs");
        assert_eq!(comments[1].line, 180);
    }

    #[test]
    fn test_parse_markdown_inline_list_bold_backtick() {
        let text = "- **`src/auth.rs:42`** — Missing null check.";
        let comments = parse_markdown_inline_comments(text);
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].path, "src/auth.rs");
        assert_eq!(comments[0].line, 42);
    }

    #[test]
    fn test_parse_markdown_inline_empty() {
        let text = "No inline comments here.";
        let comments = parse_markdown_inline_comments(text);
        assert!(comments.is_empty());
    }

    #[test]
    fn test_parse_inline_comments_from_text_fallback() {
        // No INLINE_COMMENT markers, but has markdown bold format.
        let text = "SUMMARY\nLooks good.\nEND_SUMMARY\n\n**`src/main.rs:10`**\nUnused import.";
        let comments = parse_inline_comments_from_text(text);
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].path, "src/main.rs");
    }

    #[test]
    fn test_parse_inline_comments_from_text_prefers_structured() {
        // Has both INLINE_COMMENT markers and markdown bold.
        let text = "INLINE_COMMENT src/main.rs:10\nUnused import.\nEND_INLINE_COMMENT\n\n**`src/auth.rs:42`**\nAnother issue.";
        let comments = parse_inline_comments_from_text(text);
        // Should return the structured one (not the markdown fallback).
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].path, "src/main.rs");
    }
}
