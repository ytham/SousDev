use anyhow::Result;
use std::collections::HashMap;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use crate::pipelines::stage::StageContext;
use crate::types::technique::{RunResult, StepType, TrajectoryStep};

// ---------------------------------------------------------------------------
// Prompt delivery modes
// ---------------------------------------------------------------------------

/// Controls how the prompt string is delivered to the external agent process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptDelivery {
    /// Write the prompt to the process's stdin.
    Stdin,
    /// Append the prompt as the final command-line argument.
    Argument,
}

// ---------------------------------------------------------------------------
// External agent adapter
// ---------------------------------------------------------------------------

/// Describes how to invoke a specific external agent CLI.
pub struct ExternalAgentAdapter {
    /// Short identifier (e.g. `"claude"`).
    pub name: String,
    /// Binary name on `PATH` (e.g. `"claude"`).
    pub binary: String,
    /// How the prompt is delivered to the process.
    pub prompt_delivery: PromptDelivery,
    /// Closure that builds the argument list for the CLI invocation.
    pub build_args: Box<dyn Fn(&ExternalAgentRunOptions) -> Vec<String> + Send + Sync>,
}

// ---------------------------------------------------------------------------
// Run options
// ---------------------------------------------------------------------------

/// Per-invocation options that may override adapter defaults.
#[derive(Debug, Clone, Default)]
pub struct ExternalAgentRunOptions {
    /// Working directory for the spawned process.
    pub cwd: Option<String>,
    /// Kill the process after this many seconds (default 600).
    pub timeout_secs: Option<u64>,
    /// Model override passed to the CLI.
    pub model: Option<String>,
    /// Additional CLI flags appended verbatim.
    pub extra_flags: Option<Vec<String>>,
}

// ---------------------------------------------------------------------------
// Built-in adapters
// ---------------------------------------------------------------------------

/// Build an adapter for the Anthropic `claude` CLI.
///
/// Uses `--output-format=stream-json` so the final answer can be extracted
/// from a `{"type":"result",...}` event.
pub fn claude_adapter(defaults: ExternalAgentRunOptions) -> ExternalAgentAdapter {
    ExternalAgentAdapter {
        name: "claude".to_string(),
        binary: "claude".to_string(),
        prompt_delivery: PromptDelivery::Stdin,
        build_args: Box::new(move |options| {
            let model = options
                .model
                .clone()
                .or_else(|| defaults.model.clone())
                .or_else(|| std::env::var("ANTHROPIC_MODEL").ok());
            let mut args = vec![
                "--print".to_string(),
                "--dangerously-skip-permissions".to_string(),
                "--output-format".to_string(),
                "stream-json".to_string(),
                "--verbose".to_string(),
            ];
            if let Some(m) = model {
                args.push("--model".to_string());
                args.push(m);
            }
            if let Some(flags) = &options.extra_flags {
                args.extend(flags.clone());
            }
            if let Some(flags) = &defaults.extra_flags {
                args.extend(flags.clone());
            }
            // Sentinel `"-"` tells claude to read the prompt from stdin.
            args.push("-".to_string());
            args
        }),
    }
}

/// Build an adapter for the OpenAI `codex` CLI.
pub fn codex_adapter(defaults: ExternalAgentRunOptions) -> ExternalAgentAdapter {
    ExternalAgentAdapter {
        name: "codex".to_string(),
        binary: "codex".to_string(),
        prompt_delivery: PromptDelivery::Argument,
        build_args: Box::new(move |options| {
            let model = options
                .model
                .clone()
                .or_else(|| defaults.model.clone())
                .or_else(|| std::env::var("OPENAI_MODEL").ok());
            let mut args = vec!["--quiet".to_string()];
            if let Some(m) = model {
                args.push("--model".to_string());
                args.push(m);
            }
            if let Some(flags) = &options.extra_flags {
                args.extend(flags.clone());
            }
            if let Some(flags) = &defaults.extra_flags {
                args.extend(flags.clone());
            }
            args
        }),
    }
}

/// Build an adapter for the Google `gemini` CLI.
pub fn gemini_adapter(defaults: ExternalAgentRunOptions) -> ExternalAgentAdapter {
    ExternalAgentAdapter {
        name: "gemini".to_string(),
        binary: "gemini".to_string(),
        prompt_delivery: PromptDelivery::Argument,
        build_args: Box::new(move |options| {
            let model = options
                .model
                .clone()
                .or_else(|| defaults.model.clone())
                .or_else(|| std::env::var("GEMINI_MODEL").ok());
            let mut args = vec!["--yolo".to_string()];
            if let Some(m) = model {
                args.push("--model".to_string());
                args.push(m);
            }
            if let Some(flags) = &options.extra_flags {
                args.extend(flags.clone());
            }
            if let Some(flags) = &defaults.extra_flags {
                args.extend(flags.clone());
            }
            args
        }),
    }
}

// ---------------------------------------------------------------------------
// Core runner
// ---------------------------------------------------------------------------

/// Spawn an external agent CLI, deliver the prompt, wait for it to exit, and
/// return a [`RunResult`] describing the outcome.
///
/// The process is killed if it does not exit within `timeout_secs`.
pub async fn run_external_agent_loop(
    prompt: &str,
    ctx: &StageContext,
    adapter: &ExternalAgentAdapter,
    options: &ExternalAgentRunOptions,
) -> Result<RunResult> {
    let start = std::time::Instant::now();
    let cwd = options
        .cwd
        .as_deref()
        .unwrap_or_else(|| ctx.workspace_dir.to_str().unwrap_or("."));
    let timeout_secs = options.timeout_secs.unwrap_or(600);

    let mut args = (adapter.build_args)(options);

    // Append the prompt as the last arg for Argument-delivery adapters.
    if adapter.prompt_delivery == PromptDelivery::Argument {
        args.push(prompt.to_string());
    }

    ctx.logger.info(&format!(
        "ExternalAgentLoop [{}]: running in {}",
        adapter.name, cwd
    ));

    let mut child = Command::new(&adapter.binary)
        .args(&args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    // Write the prompt to stdin, then close stdin.
    if adapter.prompt_delivery == PromptDelivery::Stdin {
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(prompt.as_bytes()).await?;
            // `stdin` is dropped here, which closes the pipe.
        }
    }

    let timeout = Duration::from_secs(timeout_secs);
    let output = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "{} timed out after {}s — killing process",
                adapter.name,
                timeout_secs
            )
        })??;

    let duration_ms = start.elapsed().as_millis() as u64;
    let stdout_raw = String::from_utf8_lossy(&output.stdout);
    let stderr_raw = String::from_utf8_lossy(&output.stderr);
    let exit_code = output.status.code().unwrap_or(1);
    let success = output.status.success();

    // For Claude stream-json, extract the final answer from the result event.
    let answer = if adapter.name == "claude" {
        extract_claude_final_answer(&stdout_raw)
            .unwrap_or_else(|| stdout_raw.trim().to_string())
    } else {
        stdout_raw.trim().to_string()
    };

    let now = chrono::Utc::now().to_rfc3339();
    let trajectory = vec![
        TrajectoryStep {
            index: 0,
            step_type: StepType::Thought,
            content: format!(
                "[{} prompt]\n{}",
                adapter.name,
                &prompt[..prompt.len().min(500)]
            ),
            timestamp: now.clone(),
            metadata: HashMap::from([
                (
                    "stage".to_string(),
                    serde_json::Value::String("prompt".to_string()),
                ),
                (
                    "agent".to_string(),
                    serde_json::Value::String(adapter.name.clone()),
                ),
            ]),
        },
        TrajectoryStep {
            index: 1,
            step_type: StepType::Thought,
            content: format!(
                "[{} output]\n{}",
                adapter.name,
                &answer[..answer.len().min(2000)]
            ),
            timestamp: now,
            metadata: HashMap::from([
                (
                    "exit_code".to_string(),
                    serde_json::Value::Number(exit_code.into()),
                ),
                (
                    "agent".to_string(),
                    serde_json::Value::String(adapter.name.clone()),
                ),
            ]),
        },
    ];

    if !success {
        ctx.logger.error(&format!(
            "{} exited with code {}",
            adapter.name, exit_code
        ));
        Ok(RunResult::failure(
            format!("external-agent:{}", adapter.name),
            format!(
                "{} exited with code {}:\n{}",
                adapter.name,
                exit_code,
                &stderr_raw[..stderr_raw.len().min(500)]
            ),
            trajectory,
            duration_ms,
        ))
    } else {
        ctx.logger.info(&format!(
            "{} completed in {:.1}s",
            adapter.name,
            duration_ms as f64 / 1000.0
        ));
        Ok(RunResult::success(
            format!("external-agent:{}", adapter.name),
            answer,
            trajectory,
            1,
            duration_ms,
        ))
    }
}

// ---------------------------------------------------------------------------
// Claude stream-json parser
// ---------------------------------------------------------------------------

/// Extract the final answer from `claude --output-format=stream-json` stdout.
///
/// Scans lines for a `{"type":"result","result":"..."}` JSON event.
/// Returns `None` when no such event is found.
pub fn extract_claude_final_answer(stdout: &str) -> Option<String> {
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(event) = serde_json::from_str::<serde_json::Value>(line) {
            if event.get("type").and_then(|t| t.as_str()) == Some("result") {
                if let Some(result) = event.get("result").and_then(|r| r.as_str()) {
                    return Some(result.to_string());
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_claude_final_answer_found() {
        let stdout = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"thinking..."}]}}
{"type":"result","result":"The answer is 42.","num_turns":3,"total_cost_usd":0.005}"#;
        assert_eq!(
            extract_claude_final_answer(stdout),
            Some("The answer is 42.".to_string())
        );
    }

    #[test]
    fn test_extract_claude_final_answer_not_found() {
        let stdout = "just plain text output\nno JSON here";
        assert_eq!(extract_claude_final_answer(stdout), None);
    }

    #[test]
    fn test_extract_claude_skips_non_result_events() {
        let stdout = r#"{"type":"assistant","message":{}}
{"type":"user","message":{}}
{"type":"result","result":"final answer"}"#;
        assert_eq!(
            extract_claude_final_answer(stdout),
            Some("final answer".to_string())
        );
    }

    #[test]
    fn test_extract_claude_empty_input() {
        assert_eq!(extract_claude_final_answer(""), None);
    }

    #[test]
    fn test_extract_claude_malformed_json_skipped() {
        let stdout = "not json\n{\"type\":\"result\",\"result\":\"ok\"}";
        assert_eq!(
            extract_claude_final_answer(stdout),
            Some("ok".to_string())
        );
    }

    #[test]
    fn test_claude_adapter_required_flags() {
        let adapter = claude_adapter(ExternalAgentRunOptions::default());
        let args = (adapter.build_args)(&ExternalAgentRunOptions::default());
        assert!(args.contains(&"--print".to_string()));
        assert!(args.contains(&"--dangerously-skip-permissions".to_string()));
        assert!(args.contains(&"stream-json".to_string()));
        assert_eq!(args.last(), Some(&"-".to_string()));
        assert_eq!(adapter.prompt_delivery, PromptDelivery::Stdin);
    }

    #[test]
    fn test_claude_adapter_model_override() {
        let adapter = claude_adapter(ExternalAgentRunOptions::default());
        let args = (adapter.build_args)(&ExternalAgentRunOptions {
            model: Some("claude-3-5-sonnet".to_string()),
            ..Default::default()
        });
        let model_pos = args.iter().position(|a| a == "--model").unwrap();
        assert_eq!(args[model_pos + 1], "claude-3-5-sonnet");
    }

    #[test]
    fn test_codex_adapter_args() {
        let adapter = codex_adapter(ExternalAgentRunOptions::default());
        let args = (adapter.build_args)(&ExternalAgentRunOptions::default());
        assert!(args.contains(&"--quiet".to_string()));
        assert_eq!(adapter.prompt_delivery, PromptDelivery::Argument);
    }

    #[test]
    fn test_gemini_adapter_args() {
        let adapter = gemini_adapter(ExternalAgentRunOptions::default());
        let args = (adapter.build_args)(&ExternalAgentRunOptions::default());
        assert!(args.contains(&"--yolo".to_string()));
        assert_eq!(adapter.prompt_delivery, PromptDelivery::Argument);
    }

    #[test]
    fn test_extra_flags_merged() {
        let defaults = ExternalAgentRunOptions {
            extra_flags: Some(vec!["--default-flag".to_string()]),
            ..Default::default()
        };
        let adapter = claude_adapter(defaults);
        let opts = ExternalAgentRunOptions {
            extra_flags: Some(vec!["--override-flag".to_string()]),
            ..Default::default()
        };
        let args = (adapter.build_args)(&opts);
        assert!(args.contains(&"--override-flag".to_string()));
        assert!(args.contains(&"--default-flag".to_string()));
    }
}
