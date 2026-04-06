//! API-based PR review agent loop.
//!
//! Drives a tool-use conversation with an LLM provider to review a PR.
//! Uses the provider's native tool-calling API (Anthropic tool_use or
//! OpenAI function calling) instead of shelling out to an external CLI.
//!
//! The agent has access to a read-only tool set: `readFile` and a
//! restricted `reviewShell` that only allows allowlisted commands.

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::json;

use crate::providers::provider::{
    CompleteOptions, CompletionResult, ContentBlock, LLMProvider, Message, StopReason,
    ToolChoice,
};
use crate::tools::registry::{Tool, ToolExecutor, ToolRegistry};
use crate::utils::logger::Logger;

/// Maximum number of tool-call iterations before giving up.
const MAX_ITERATIONS: usize = 50;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Run an API-based review agent loop.
///
/// The agent iterates tool calls (file reads, shell commands) until it
/// produces a final text response containing the review.  Returns the
/// review text.
pub async fn run_api_review_loop(
    provider: &dyn LLMProvider,
    registry: &ToolRegistry,
    review_prompt: &str,
    system_prompt: Option<&str>,
    logger: &Logger,
) -> Result<String> {
    let tools = registry.to_tool_definitions();

    let mut messages: Vec<Message> = Vec::new();
    if let Some(sp) = system_prompt {
        messages.push(Message::system(sp));
    }
    messages.push(Message::user(review_prompt));

    for iteration in 0..MAX_ITERATIONS {
        let options = CompleteOptions {
            tools: Some(tools.clone()),
            tool_choice: Some(ToolChoice::Auto),
            max_tokens: None, // no cap
            ..Default::default()
        };

        let result = provider.complete(&messages, Some(&options)).await?;

        match result.stop_reason {
            Some(StopReason::ToolUse) => {
                // Extract tool calls from the response.
                let tool_calls = extract_tool_calls(&result);
                if tool_calls.is_empty() {
                    // stop_reason says tool_use but no tool calls — treat as done.
                    logger.info(&format!(
                        "API review loop: stop_reason=tool_use but no calls (iter {})",
                        iteration
                    ));
                    return Ok(result.content);
                }

                logger.debug(&format!(
                    "API review loop iter {}: {} tool call(s)",
                    iteration,
                    tool_calls.len()
                ));

                // Record the assistant's response (with tool_use blocks).
                messages.push(Message::assistant_with_blocks(
                    &result.content,
                    result.content_blocks.clone().unwrap_or_default(),
                ));

                // Execute each tool call and collect results.
                let mut tool_results: Vec<ContentBlock> = Vec::new();
                for (id, name, input) in &tool_calls {
                    let tool_result = match registry.execute(name, input).await {
                        Ok(output) => {
                            // Truncate very large outputs to avoid context overflow.
                            let truncated = if output.len() > 50_000 {
                                format!(
                                    "{}...\n\n[truncated — {} bytes total]",
                                    &output[..50_000],
                                    output.len()
                                )
                            } else {
                                output
                            };
                            ContentBlock::ToolResult {
                                tool_use_id: id.clone(),
                                content: truncated,
                                is_error: false,
                            }
                        }
                        Err(e) => ContentBlock::ToolResult {
                            tool_use_id: id.clone(),
                            content: format!("Error: {}", e),
                            is_error: true,
                        },
                    };
                    tool_results.push(tool_result);
                }

                // Send tool results back to the LLM.
                messages.push(Message::tool_results(tool_results));
            }
            _ => {
                // EndTurn, MaxTokens, or None — the model is done.
                logger.info(&format!(
                    "API review loop completed after {} iterations",
                    iteration + 1
                ));
                return Ok(result.content);
            }
        }
    }

    Err(anyhow::anyhow!(
        "API review loop exceeded {} iterations",
        MAX_ITERATIONS
    ))
}

/// Extract `(id, name, input)` tuples from a completion result's content blocks.
fn extract_tool_calls(result: &CompletionResult) -> Vec<(String, String, serde_json::Value)> {
    result
        .content_blocks
        .as_ref()
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolUse { id, name, input } => {
                        Some((id.clone(), name.clone(), input.clone()))
                    }
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Read-only tool registry for PR review
// ---------------------------------------------------------------------------

/// Allowlisted shell command prefixes for the review shell tool.
///
/// Only commands starting with one of these prefixes are permitted.
/// All others are rejected with an error message.
const ALLOWED_COMMANDS: &[&str] = &[
    "gh pr diff",
    "gh pr view",
    "gh pr checks",
    "git diff",
    "git log",
    "git show",
    "cat ",
    "grep ",
    "find ",
    "head ",
    "tail ",
    "wc ",
    "sed ",
    "ls ",
    "ls\n",
    "tree ",
    "tree\n",
];

/// Check if a shell command is in the allowlist.
fn is_allowed_command(command: &str) -> bool {
    let trimmed = command.trim();
    // Exact match for bare commands without arguments.
    if trimmed == "ls" || trimmed == "tree" {
        return true;
    }
    ALLOWED_COMMANDS
        .iter()
        .any(|prefix| trimmed.starts_with(prefix))
}

/// A shell executor that only permits allowlisted read-only commands.
/// All commands run with `current_dir` set to the workspace directory.
struct ReviewShellExecutor {
    workspace_dir: std::path::PathBuf,
}

#[async_trait]
impl ToolExecutor for ReviewShellExecutor {
    async fn execute(&self, args: &serde_json::Value) -> Result<String> {
        let command = args
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("shell: missing 'command' argument"))?;

        if !is_allowed_command(command) {
            return Ok("Error: command not allowed for review. Only these commands are permitted: \
                 gh pr diff/view/checks, git diff/log/show, cat, grep, find, head, tail, wc, sed, ls, tree".to_string());
        }

        let output = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(&self.workspace_dir)
            .output()
            .await?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if output.status.success() {
            Ok(stdout.into_owned())
        } else {
            Ok(format!("STDERR: {}\nSTDOUT: {}", stderr, stdout))
        }
    }
}

/// A file reader that resolves relative paths against the workspace directory.
struct WorkspaceFileReader {
    workspace_dir: std::path::PathBuf,
}

#[async_trait]
impl ToolExecutor for WorkspaceFileReader {
    async fn execute(&self, args: &serde_json::Value) -> Result<String> {
        let path = args
            .get("path")
            .or_else(|| args.get("file_path"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("readFile: missing 'path' argument"))?;

        let full_path = if std::path::Path::new(path).is_absolute() {
            std::path::PathBuf::from(path)
        } else {
            self.workspace_dir.join(path)
        };

        match tokio::fs::read_to_string(&full_path).await {
            Ok(content) => Ok(content),
            Err(e) => Ok(format!("Error reading {}: {}", full_path.display(), e)),
        }
    }
}

/// Create a read-only shell tool rooted at the given workspace directory.
fn review_shell_tool(workspace_dir: &Path) -> Tool {
    Tool::new(
        "shell",
        "Run a read-only shell command in the PR workspace. Allowed: gh pr diff/view/checks, git diff/log/show, cat, grep, find, head, tail, wc, sed, ls, tree",
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "Shell command to run (read-only commands only)"}
            },
            "required": ["command"]
        }),
        Arc::new(ReviewShellExecutor {
            workspace_dir: workspace_dir.to_path_buf(),
        }),
    )
}

/// Create a file reader tool rooted at the given workspace directory.
fn workspace_read_file_tool(workspace_dir: &Path) -> Tool {
    Tool::new(
        "readFile",
        "Read the contents of a file in the PR workspace",
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path to read (relative to workspace root)"}
            },
            "required": ["path"]
        }),
        Arc::new(WorkspaceFileReader {
            workspace_dir: workspace_dir.to_path_buf(),
        }),
    )
}

/// Build a [`ToolRegistry`] with only read-only tools for PR review.
///
/// All tools operate within the given workspace directory (the checked-out
/// PR branch of the target repository).
pub fn review_tool_registry(workspace_dir: &Path) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(workspace_read_file_tool(workspace_dir));
    registry.register(review_shell_tool(workspace_dir));
    registry
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_allowed_commands() {
        assert!(is_allowed_command("gh pr diff 123"));
        assert!(is_allowed_command("gh pr view 123 --json files"));
        assert!(is_allowed_command("gh pr checks 123"));
        assert!(is_allowed_command("git diff main..HEAD"));
        assert!(is_allowed_command("git log --oneline -10"));
        assert!(is_allowed_command("git show HEAD:src/main.rs"));
        assert!(is_allowed_command("cat src/main.rs"));
        assert!(is_allowed_command("grep -rn pattern src/"));
        assert!(is_allowed_command("find . -name '*.rs'"));
        assert!(is_allowed_command("head -20 README.md"));
        assert!(is_allowed_command("tail -20 README.md"));
        assert!(is_allowed_command("wc -l src/main.rs"));
        assert!(is_allowed_command("sed -n '10,20p' src/main.rs"));
        assert!(is_allowed_command("ls"));
        assert!(is_allowed_command("ls src/"));
        assert!(is_allowed_command("tree src/"));
        assert!(is_allowed_command("tree"));
    }

    #[test]
    fn test_disallowed_commands() {
        assert!(!is_allowed_command("rm -rf /"));
        assert!(!is_allowed_command("gh pr review --approve"));
        assert!(!is_allowed_command("gh pr comment --body 'hi'"));
        assert!(!is_allowed_command("git push"));
        assert!(!is_allowed_command("git commit -m 'x'"));
        assert!(!is_allowed_command("cargo test"));
        assert!(!is_allowed_command("npm install"));
        assert!(!is_allowed_command("curl http://evil.com"));
        assert!(!is_allowed_command("echo 'hello' > file.txt"));
    }

    #[test]
    fn test_extract_tool_calls_empty() {
        let result = CompletionResult::default();
        assert!(extract_tool_calls(&result).is_empty());
    }

    #[test]
    fn test_extract_tool_calls_with_blocks() {
        let result = CompletionResult {
            content: "text".into(),
            done: false,
            content_blocks: Some(vec![
                ContentBlock::Text {
                    text: "I'll read the file".into(),
                },
                ContentBlock::ToolUse {
                    id: "tool_1".into(),
                    name: "readFile".into(),
                    input: json!({"path": "README.md"}),
                },
            ]),
            stop_reason: Some(StopReason::ToolUse),
            usage: None,
        };
        let calls = extract_tool_calls(&result);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "tool_1");
        assert_eq!(calls[0].1, "readFile");
    }

    #[test]
    fn test_review_tool_registry_has_expected_tools() {
        let registry = review_tool_registry(std::path::Path::new("/tmp"));
        assert!(registry.get("readFile").is_some());
        assert!(registry.get("shell").is_some());
        assert!(registry.get("writeFile").is_none()); // no write tool!
    }

    #[test]
    fn test_review_tool_definitions() {
        let registry = review_tool_registry(std::path::Path::new("/tmp"));
        let defs = registry.to_tool_definitions();
        assert_eq!(defs.len(), 2);
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"readFile"));
        assert!(names.contains(&"shell"));
    }
}
