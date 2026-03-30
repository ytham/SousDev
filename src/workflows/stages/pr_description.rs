use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::utils::prompt_loader::PromptLoader;
use crate::workflows::stage::{Stage, StageContext};

/// Maximum bytes of `git diff HEAD` passed to the prompt (avoids overflowing
/// the context window on large diffs).
const MAX_DIFF_BYTES: usize = 40_000;

/// Generates a pull-request title and body by feeding the current diff to
/// the Claude CLI with the `pr_description` prompt.
///
/// Uses the same Claude CLI as the agent loop so no separate API key is
/// needed.  Falls back to a template-based title/body if the CLI call fails.
///
/// Parses the response for:
/// ```text
/// TITLE: <single-line title>
/// BODY:
/// <multi-line body>
/// ```
/// and stores the results in `ctx.pr_title` and `ctx.pr_generated_body`.
pub struct PrDescriptionStage;

#[async_trait]
impl Stage for PrDescriptionStage {
    fn name(&self) -> &str {
        "pr-description"
    }

    async fn run(&self, ctx: &mut StageContext) -> Result<()> {
        if ctx.is_aborted() {
            return Ok(());
        }

        // Collect the diff (best-effort).
        let diff = get_diff(&ctx.workspace_dir).await;

        let loader = PromptLoader::new(&ctx.harness_root);
        let mut vars = HashMap::new();
        vars.insert("task".to_string(), ctx.parsed_task.task.clone());
        vars.insert("diff".to_string(), diff.clone());
        vars.insert("branch".to_string(), ctx.branch.clone());
        vars.insert(
            "issue_url".to_string(),
            ctx.issue_url.clone().unwrap_or_else(|| "(no issue)".into()),
        );
        if let Some(answer) = ctx.agent_result.as_ref().map(|r| r.answer.clone()) {
            vars.insert("agent_answer".to_string(), answer);
        }
        let prompt = loader.load(&ctx.prompts.pr_description, &vars).await?;

        ctx.logger
            .info("PrDescriptionStage: generating PR title and body via Claude CLI");

        // Try Claude CLI first, fall back to template on failure.
        let (title, body) = match generate_via_claude_cli(&prompt, ctx).await {
            Ok(text) => {
                let (t, b) = parse_title_body(&text);
                // If the LLM didn't follow the TITLE:/BODY: format,
                // use the raw output as the body.
                let body = b.or_else(|| {
                    let trimmed = text.trim();
                    if trimmed.is_empty() {
                        None
                    } else {
                        Some(trimmed.to_string())
                    }
                });
                (t, body)
            }
            Err(e) => {
                ctx.logger.info(&format!(
                    "Claude CLI failed for PR description (falling back to template): {}",
                    e
                ));
                generate_fallback_title_body(ctx, &diff)
            }
        };

        // Config overrides take precedence over generated values.
        if ctx.pr_title.is_none() {
            if let Some(cfg_title) = ctx
                .config
                .pull_request
                .as_ref()
                .and_then(|pr| pr.title.as_deref())
            {
                ctx.pr_title = Some(cfg_title.to_string());
            } else {
                ctx.pr_title = title;
            }
        }

        if ctx.pr_generated_body.is_none() {
            if let Some(cfg_body) = ctx
                .config
                .pull_request
                .as_ref()
                .and_then(|pr| pr.body.as_deref())
            {
                ctx.pr_generated_body = Some(cfg_body.to_string());
            } else {
                ctx.pr_generated_body = body;
            }
        }

        ctx.logger.info(&format!(
            "PrDescriptionStage: title = {:?}",
            ctx.pr_title
        ));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Claude CLI invocation
// ---------------------------------------------------------------------------

/// Call the Claude CLI with the PR description prompt and return the response.
async fn generate_via_claude_cli(prompt: &str, ctx: &StageContext) -> Result<String> {
    let mut args = vec![
        "--print".to_string(),
        "--dangerously-skip-permissions".to_string(),
        "--output-format".to_string(),
        "text".to_string(),
    ];

    // Pass system prompt if available.
    if let Some(ref sp) = ctx.system_prompt {
        args.push("--system-prompt".to_string());
        args.push(sp.clone());
    }

    // Read from stdin.
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
        std::time::Duration::from_secs(120),
        child.wait_with_output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("Claude CLI timed out after 120s"))??;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow::anyhow!(
            "Claude CLI exited with code {}: {}",
            output.status.code().unwrap_or(1),
            stderr.chars().take(500).collect::<String>()
        ));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

// ---------------------------------------------------------------------------
// Fallback
// ---------------------------------------------------------------------------

/// Generate a simple title/body when the Claude CLI is unavailable.
fn generate_fallback_title_body(
    ctx: &StageContext,
    diff: &str,
) -> (Option<String>, Option<String>) {
    let title = if !ctx.branch.is_empty() {
        Some(ctx.branch.replace('-', " ").replace('/', ": "))
    } else {
        Some(ctx.parsed_task.task.chars().take(72).collect::<String>())
    };

    let diff_stat = diff
        .lines()
        .filter(|l| l.starts_with("diff --git") || l.starts_with("+++") || l.starts_with("---"))
        .take(20)
        .collect::<Vec<_>>()
        .join("\n");

    let body = Some(format!(
        "## Task\n\n{}\n\n## Changes\n\n```\n{}\n```",
        ctx.parsed_task.task.chars().take(500).collect::<String>(),
        if diff_stat.is_empty() {
            "(no diff available)".to_string()
        } else {
            diff_stat
        }
    ));

    (title, body)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Run `git diff HEAD` in `workspace_dir` and return the output, truncated to
/// [`MAX_DIFF_BYTES`].
async fn get_diff(workspace_dir: &std::path::Path) -> String {
    match Command::new("git")
        .arg("diff")
        .arg("HEAD")
        .current_dir(workspace_dir)
        .output()
        .await
    {
        Ok(output) => {
            let raw = String::from_utf8_lossy(&output.stdout);
            if raw.len() > MAX_DIFF_BYTES {
                format!("{}\n… (diff truncated)", &raw[..MAX_DIFF_BYTES])
            } else {
                raw.to_string()
            }
        }
        Err(_) => String::new(),
    }
}

/// Parse `TITLE: ...` and `BODY:\n...` from an LLM response.
///
/// Returns `(title, body)` — both are `None` when not found.
fn parse_title_body(text: &str) -> (Option<String>, Option<String>) {
    let mut title: Option<String> = None;
    let mut body: Option<String> = None;

    let mut in_body = false;
    let mut body_lines: Vec<&str> = Vec::new();

    for line in text.lines() {
        if in_body {
            body_lines.push(line);
            continue;
        }
        if let Some(rest) = line.strip_prefix("TITLE:") {
            title = Some(rest.trim().to_string());
            continue;
        }
        if line.trim_start().starts_with("BODY:") {
            in_body = true;
            let rest = line.trim_start().strip_prefix("BODY:").unwrap_or("").trim();
            if !rest.is_empty() {
                body_lines.push(rest);
            }
        }
    }

    if !body_lines.is_empty() {
        body = Some(body_lines.join("\n").trim().to_string());
    }

    (title, body)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_title_and_body() {
        let text = "TITLE: Fix null pointer in auth module\nBODY:\nThis PR fixes the null pointer exception.\n\nAll tests pass.";
        let (title, body) = parse_title_body(text);
        assert_eq!(title.as_deref(), Some("Fix null pointer in auth module"));
        let b = body.unwrap();
        assert!(b.contains("null pointer exception"));
        assert!(b.contains("All tests pass"));
    }

    #[test]
    fn test_parse_title_only() {
        let text = "TITLE: My PR\nSome preamble without BODY marker.";
        let (title, body) = parse_title_body(text);
        assert_eq!(title.as_deref(), Some("My PR"));
        assert!(body.is_none());
    }

    #[test]
    fn test_parse_body_inline() {
        let text = "TITLE: T\nBODY: inline body text";
        let (title, body) = parse_title_body(text);
        assert_eq!(title.as_deref(), Some("T"));
        assert_eq!(body.as_deref(), Some("inline body text"));
    }

    #[test]
    fn test_parse_no_title() {
        let text = "BODY:\nbody content here";
        let (title, body) = parse_title_body(text);
        assert!(title.is_none());
        assert_eq!(body.as_deref(), Some("body content here"));
    }

    #[test]
    fn test_parse_empty_input() {
        let (title, body) = parse_title_body("");
        assert!(title.is_none());
        assert!(body.is_none());
    }

    #[test]
    fn test_parse_multiline_body() {
        let text = "TITLE: T\nBODY:\nLine 1\nLine 2\nLine 3";
        let (_, body) = parse_title_body(text);
        let b = body.unwrap();
        assert!(b.contains("Line 1"));
        assert!(b.contains("Line 2"));
        assert!(b.contains("Line 3"));
    }

    #[test]
    fn test_fallback_title_from_branch() {
        use crate::workflows::workflow::ParsedTask;

        let (title, body) = generate_fallback_title_body(
            &make_stub_ctx("harness/issue-42", "Fix the bug"),
            "diff --git a/foo.rs b/foo.rs\n--- a/foo.rs\n+++ b/foo.rs\n",
        );
        assert!(title.unwrap().contains("harness"));
        assert!(body.unwrap().contains("Fix the bug"));
    }

    #[test]
    fn test_fallback_empty_diff() {
        let (_, body) = generate_fallback_title_body(
            &make_stub_ctx("main", "Do the thing"),
            "",
        );
        assert!(body.unwrap().contains("no diff available"));
    }

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

    /// Create a minimal StageContext stub for fallback tests.
    fn make_stub_ctx(branch: &str, task: &str) -> StageContext {
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
            branch: branch.to_string(),
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
}
