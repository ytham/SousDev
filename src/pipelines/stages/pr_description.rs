use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;
use tokio::process::Command;
use crate::pipelines::stage::{Stage, StageContext};
use crate::providers::provider::{Message, CompleteOptions};
use crate::utils::prompt_loader::PromptLoader;

/// Maximum bytes of `git diff HEAD` passed to the LLM (avoids overflowing the
/// context window on large diffs).
const MAX_DIFF_BYTES: usize = 40_000;

/// Generates a pull-request title and body by feeding the current diff to the
/// LLM with the `pr_description` prompt.
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
        vars.insert(
            "branch".to_string(),
            ctx.branch.clone(),
        );
        let prompt = loader.load(&ctx.prompts.pr_description, &vars).await?;

        ctx.logger
            .info("PrDescriptionStage: generating PR title and body");

        let messages = vec![Message::user(&prompt)];
        let opts = CompleteOptions {
            max_tokens: Some(1024),
            ..Default::default()
        };
        let completion = ctx.provider.complete(&messages, Some(&opts)).await?;
        let text = completion.content;

        let (title, body) = parse_title_body(&text);

        // Config overrides take precedence over LLM-generated values.
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
            // Inline content after "BODY: " on the same line.
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
}
