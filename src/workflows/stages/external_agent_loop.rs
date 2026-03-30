use anyhow::Result;
use std::collections::HashMap;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use crate::workflows::stage::StageContext;
use crate::types::technique::{RunResult, StepType, TrajectoryStep};

/// Type alias for the argument builder closure used by [`ExternalAgentAdapter`].
type BuildArgsFn = Box<dyn Fn(&ExternalAgentRunOptions) -> Vec<String> + Send + Sync>;

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
    /// Whether the CLI supports a `--system-prompt` flag.
    pub supports_system_prompt: bool,
    /// Closure that builds the argument list for the CLI invocation.
    pub build_args: BuildArgsFn,
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
        supports_system_prompt: true,
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
        supports_system_prompt: false,
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
        supports_system_prompt: false,
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
/// When `system_prompt` is `Some`, it is injected via the adapter's native
/// system prompt mechanism (e.g. `--system-prompt` for Claude).  For adapters
/// that lack a native flag, the system prompt is prepended to the task text.
///
/// The process is killed if it does not exit within `timeout_secs`.
pub async fn run_external_agent_loop(
    prompt: &str,
    ctx: &StageContext,
    adapter: &ExternalAgentAdapter,
    options: &ExternalAgentRunOptions,
    system_prompt: Option<&str>,
) -> Result<RunResult> {
    let start = std::time::Instant::now();
    let cwd = options
        .cwd
        .as_deref()
        .unwrap_or_else(|| ctx.workspace_dir.to_str().unwrap_or("."));
    let timeout_secs = options.timeout_secs.unwrap_or(600);

    let mut args = (adapter.build_args)(options);

    // Inject the system prompt via the adapter's native mechanism.
    if let Some(sp) = system_prompt {
        if adapter.supports_system_prompt {
            args.push("--system-prompt".to_string());
            args.push(sp.to_string());
        }
    }

    // Build the effective prompt — for adapters without native system prompt
    // support, prepend it to the task text.
    let effective_prompt = match system_prompt {
        Some(sp) if !adapter.supports_system_prompt => {
            format!("<system>\n{}\n</system>\n\n{}", sp, prompt)
        }
        _ => prompt.to_string(),
    };

    // Append the prompt as the last arg for Argument-delivery adapters.
    if adapter.prompt_delivery == PromptDelivery::Argument {
        args.push(effective_prompt.clone());
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
            stdin.write_all(effective_prompt.as_bytes()).await?;
            // `stdin` is dropped here, which closes the pipe.
        }
    }

    // Stream stdout line-by-line so agent output appears in the TUI in
    // real-time instead of being buffered until the process exits.
    let stdout_handle = child.stdout.take();
    let timeout = Duration::from_secs(timeout_secs);

    let mut stdout_lines: Vec<String> = Vec::new();
    let mut trajectory: Vec<TrajectoryStep> = Vec::new();

    let stream_and_wait = async {
        if let Some(stdout) = stdout_handle {
            let mut reader = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                // Parse and log each line as it arrives (real-time TUI updates).
                if adapter.name == "claude" {
                    stream_parse_claude_line(
                        &line,
                        &mut trajectory,
                        ctx,
                    )
                    .await;
                }
                stdout_lines.push(line);
            }
        }
        child.wait().await
    };

    let status = tokio::time::timeout(timeout, stream_and_wait)
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "{} timed out after {}s — killing process",
                adapter.name,
                timeout_secs
            )
        })??;

    let duration_ms = start.elapsed().as_millis() as u64;
    let exit_code = status.code().unwrap_or(1);
    let success = status.success();

    // Read any remaining stderr.
    let stderr_raw = if let Some(mut stderr) = child.stderr.take() {
        let mut buf = String::new();
        let _ = tokio::io::AsyncReadExt::read_to_string(&mut stderr, &mut buf).await;
        buf
    } else {
        String::new()
    };

    let stdout_raw = stdout_lines.join("\n");

    // For Claude stream-json, extract the final answer from the result event.
    let answer = if adapter.name == "claude" {
        extract_claude_final_answer(&stdout_raw)
            .unwrap_or_else(|| stdout_raw.trim().to_string())
    } else {
        stdout_raw.trim().to_string()
    };

    // For non-Claude agents, build a minimal trajectory (Claude's was
    // built incrementally during streaming above).
    if adapter.name != "claude" {
        let now = chrono::Utc::now().to_rfc3339();
        trajectory = vec![
            TrajectoryStep {
                index: 0,
                step_type: StepType::Thought,
                content: format!(
                    "[{} prompt]\n{}",
                    adapter.name,
                    &prompt[..prompt.len().min(500)]
                ),
                timestamp: now.clone(),
                metadata: HashMap::new(),
            },
            TrajectoryStep {
                index: 1,
                step_type: StepType::Observation,
                content: format!(
                    "[{} output]\n{}",
                    adapter.name,
                    &answer[..answer.len().min(2000)]
                ),
                timestamp: now,
                metadata: HashMap::from([(
                    "exit_code".to_string(),
                    serde_json::Value::Number(exit_code.into()),
                )]),
            },
        ];
        // Emit non-Claude trajectory to log (Claude's was emitted in real-time).
        if let Some(ref log) = ctx.workflow_log {
            for step in &trajectory {
                let level = match step.step_type {
                    StepType::Thought => "info",
                    StepType::Action => "info",
                    StepType::Observation => "debug",
                    StepType::Reflection => "info",
                };
                let prefix = match step.step_type {
                    StepType::Thought => "thought",
                    StepType::Action => "tool",
                    StepType::Observation => "result",
                    StepType::Reflection => "reflect",
                };
                let _ = log.log(level, prefix, &step.content).await;
            }
        }
    }

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

/// Parse a single Claude stream-json line in real-time, append to the
/// trajectory, and emit to the workflow log for TUI display.
async fn stream_parse_claude_line(
    line: &str,
    trajectory: &mut Vec<TrajectoryStep>,
    ctx: &StageContext,
) {
    let line = line.trim();
    if line.is_empty() {
        return;
    }
    let event: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return,
    };

    let event_type = event.get("type").and_then(|t| t.as_str()).unwrap_or("");
    let now = chrono::Utc::now().to_rfc3339();

    match event_type {
        "assistant" => {
            let content_blocks = event
                .pointer("/message/content")
                .and_then(|c| c.as_array());
            if let Some(blocks) = content_blocks {
                for block in blocks {
                    let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    match block_type {
                        "text" => {
                            if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                                let text = text.trim();
                                if !text.is_empty() {
                                    let step = TrajectoryStep {
                                        index: trajectory.len(),
                                        step_type: StepType::Thought,
                                        content: text.to_string(),
                                        timestamp: now.clone(),
                                        metadata: HashMap::new(),
                                    };
                                    if let Some(ref log) = ctx.workflow_log {
                                        let _ = log.log("info", "thought", text).await;
                                    }
                                    trajectory.push(step);
                                }
                            }
                        }
                        "tool_use" => {
                            let tool_name = block
                                .get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or("unknown");
                            let input_display = block
                                .get("input")
                                .map(|i| format_tool_input(tool_name, i))
                                .unwrap_or_default();
                            let step = TrajectoryStep {
                                index: trajectory.len(),
                                step_type: StepType::Action,
                                content: input_display.clone(),
                                timestamp: now.clone(),
                                metadata: HashMap::from([(
                                    "tool".to_string(),
                                    serde_json::Value::String(tool_name.to_string()),
                                )]),
                            };
                            if let Some(ref log) = ctx.workflow_log {
                                let _ = log.log("info", "tool", &input_display).await;
                            }
                            trajectory.push(step);
                        }
                        _ => {}
                    }
                }
            }
        }
        "tool" => {
            let content_blocks = event.get("content").and_then(|c| c.as_array());
            if let Some(blocks) = content_blocks {
                for block in blocks {
                    let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    if block_type == "tool_result" {
                        let output = block
                            .get("output")
                            .and_then(|o| o.as_str())
                            .unwrap_or("");
                        let output_display = if output.len() > 500 {
                            format!("{}…", &output[..500])
                        } else {
                            output.to_string()
                        };
                        let step = TrajectoryStep {
                            index: trajectory.len(),
                            step_type: StepType::Observation,
                            content: output_display.clone(),
                            timestamp: now.clone(),
                            metadata: HashMap::new(),
                        };
                        if let Some(ref log) = ctx.workflow_log {
                            let _ = log.log("debug", "result", &output_display).await;
                        }
                        trajectory.push(step);
                    }
                }
            }
        }
        // Skip "result" events — the final answer is extracted separately.
        _ => {}
    }
}

/// Parse Claude's `stream-json` output into a rich trajectory of steps.
///
/// Used by tests to verify parsing logic.  Production code uses
/// [`stream_parse_claude_line`] for real-time line-by-line parsing.
///
/// The stream contains JSON lines with these event types:
#[cfg(test)]
/// - `{"type":"assistant","message":{"content":[...]}}` — assistant messages
///   containing `"type":"text"` (thoughts) and `"type":"tool_use"` (tool calls)
/// - `{"type":"tool","content":[{"type":"tool_result",...}]}` — tool results
/// - `{"type":"result",...}` — final result (skipped, captured elsewhere)
fn parse_claude_stream_trajectory(stdout: &str, agent_name: &str) -> Vec<TrajectoryStep> {
    let mut steps = Vec::new();
    let now = chrono::Utc::now().to_rfc3339();

    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let event: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let event_type = event.get("type").and_then(|t| t.as_str()).unwrap_or("");

        match event_type {
            "assistant" => {
                let content_blocks = event
                    .pointer("/message/content")
                    .and_then(|c| c.as_array());
                if let Some(blocks) = content_blocks {
                    for block in blocks {
                        let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                        match block_type {
                            "text" => {
                                if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                                    let text = text.trim();
                                    if !text.is_empty() {
                                        steps.push(TrajectoryStep {
                                            index: steps.len(),
                                            step_type: StepType::Thought,
                                            content: text.to_string(),
                                            timestamp: now.clone(),
                                            metadata: HashMap::new(),
                                        });
                                    }
                                }
                            }
                            "tool_use" => {
                                let tool_name = block
                                    .get("name")
                                    .and_then(|n| n.as_str())
                                    .unwrap_or("unknown");
                                let input_display = block
                                    .get("input")
                                    .map(|i| format_tool_input(tool_name, i))
                                    .unwrap_or_default();
                                steps.push(TrajectoryStep {
                                    index: steps.len(),
                                    step_type: StepType::Action,
                                    content: input_display,
                                    timestamp: now.clone(),
                                    metadata: HashMap::from([(
                                        "tool".to_string(),
                                        serde_json::Value::String(tool_name.to_string()),
                                    )]),
                                });
                            }
                            _ => {}
                        }
                    }
                }
            }
            "tool" => {
                let content_blocks = event.get("content").and_then(|c| c.as_array());
                if let Some(blocks) = content_blocks {
                    for block in blocks {
                        let block_type = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                        if block_type == "tool_result" {
                            let output = block
                                .get("output")
                                .and_then(|o| o.as_str())
                                .unwrap_or("");
                            let output_display = if output.len() > 500 {
                                format!("{}…", &output[..500])
                            } else {
                                output.to_string()
                            };
                            steps.push(TrajectoryStep {
                                index: steps.len(),
                                step_type: StepType::Observation,
                                content: output_display,
                                timestamp: now.clone(),
                                metadata: HashMap::new(),
                            });
                        }
                    }
                }
            }
            // Skip "result" events — the final answer is extracted separately.
            _ => {}
        }
    }

    // If parsing yielded nothing (non-stream output), fall back to a minimal summary.
    if steps.is_empty() {
        let answer = extract_claude_final_answer(stdout)
            .unwrap_or_else(|| stdout.chars().take(2000).collect());
        steps.push(TrajectoryStep {
            index: 0,
            step_type: StepType::Thought,
            content: format!("[{} output]\n{}", agent_name, answer),
            timestamp: now,
            metadata: HashMap::new(),
        });
    }

    steps
}

/// Format a tool invocation input as a concise function-call-style string.
///
/// Produces output like:
/// - `Read("src/main.rs", limit=60, offset=130)`
/// - `Bash("cargo test", timeout=120000)`
/// - `Grep("pattern", path="src/", include="*.rs")`
/// - `Write("path/to/file.rs", content=<342 chars>)`
fn format_tool_input(tool_name: &str, input: &serde_json::Value) -> String {
    let obj = match input.as_object() {
        Some(o) => o,
        None => return format!("{}()", tool_name),
    };

    if obj.is_empty() {
        return format!("{}()", tool_name);
    }

    // Determine the "primary" argument to show first (unkeyed).
    // Common patterns: file_path, path, command, pattern, query, prompt, content.
    let primary_keys = [
        "file_path", "filePath", "command", "pattern", "query", "prompt", "url", "path",
    ];

    let mut parts: Vec<String> = Vec::new();
    let mut used_primary = false;

    // Show the primary argument first as a bare quoted string.
    for key in &primary_keys {
        if let Some(val) = obj.get(*key) {
            if let Some(s) = val.as_str() {
                let display = truncate_str(s, 80);
                parts.push(format!("\"{}\"", display));
                used_primary = true;
                break;
            }
        }
    }

    // Show remaining arguments as key=value pairs.
    for (key, val) in obj {
        // Skip the primary key we already showed.
        if used_primary && primary_keys.contains(&key.as_str()) {
            if let Some(s) = val.as_str() {
                // Only skip if this is the one we displayed.
                let display = truncate_str(s, 80);
                if parts.first().map(|p| p.as_str()) == Some(&format!("\"{}\"", display)) {
                    continue;
                }
            }
        }

        let formatted = format_value(key, val);
        parts.push(formatted);
    }

    format!("{}({})", tool_name, parts.join(", "))
}

/// Format a single key=value pair for tool input display.
fn format_value(key: &str, val: &serde_json::Value) -> String {
    match val {
        serde_json::Value::String(s) => {
            // Large string values (file contents, etc.) show length instead.
            if s.len() > 100 {
                format!("{}=<{} chars>", key, s.len())
            } else {
                format!("{}=\"{}\"", key, s)
            }
        }
        serde_json::Value::Number(n) => format!("{}={}", key, n),
        serde_json::Value::Bool(b) => format!("{}={}", key, b),
        serde_json::Value::Array(arr) => format!("{}=[{} items]", key, arr.len()),
        serde_json::Value::Null => format!("{}=null", key),
        serde_json::Value::Object(_) => format!("{}={{...}}", key),
    }
}

/// Truncate a string for display, appending "…" if needed.
fn truncate_str(s: &str, max: usize) -> String {
    if s.len() > max {
        format!("{}…", &s[..max])
    } else {
        s.to_string()
    }
}

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

    // ── Claude stream-json trajectory parsing ──────────────────────────────

    #[test]
    fn test_parse_claude_stream_text_and_tool_use() {
        let stdout = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Let me read the file first."},{"type":"tool_use","name":"Read","input":{"path":"src/main.rs"}}]}}
{"type":"tool","content":[{"type":"tool_result","output":"fn main() { }"}]}
{"type":"assistant","message":{"content":[{"type":"text","text":"I see the issue. Let me fix it."}]}}
{"type":"result","result":"Fixed the bug."}"#;

        let steps = parse_claude_stream_trajectory(stdout, "claude");
        assert_eq!(steps.len(), 4);
        assert_eq!(steps[0].step_type, StepType::Thought);
        assert!(steps[0].content.contains("read the file"));
        assert_eq!(steps[1].step_type, StepType::Action);
        assert!(steps[1].content.contains("Read"));
        assert!(steps[1].content.contains("src/main.rs"));
        assert_eq!(steps[2].step_type, StepType::Observation);
        assert!(steps[2].content.contains("fn main()"));
        assert_eq!(steps[3].step_type, StepType::Thought);
        assert!(steps[3].content.contains("fix it"));
    }

    #[test]
    fn test_parse_claude_stream_empty_output() {
        let steps = parse_claude_stream_trajectory("", "claude");
        // Fallback: one thought step with empty output
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].step_type, StepType::Thought);
    }

    #[test]
    fn test_parse_claude_stream_result_only() {
        let stdout = r#"{"type":"result","result":"done"}"#;
        let steps = parse_claude_stream_trajectory(stdout, "claude");
        // Result events are skipped; fallback kicks in
        assert_eq!(steps.len(), 1);
        assert!(steps[0].content.contains("done"));
    }

    #[test]
    fn test_parse_claude_stream_malformed_lines_skipped() {
        let stdout = "not json\n{invalid\n{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"ok\"}]}}";
        let steps = parse_claude_stream_trajectory(stdout, "claude");
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].step_type, StepType::Thought);
        assert_eq!(steps[0].content, "ok");
    }

    #[test]
    fn test_parse_claude_stream_tool_use_metadata() {
        let stdout = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Write","input":{"path":"foo.rs","content":"hello"}}]}}"#;
        let steps = parse_claude_stream_trajectory(stdout, "claude");
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].step_type, StepType::Action);
        assert_eq!(
            steps[0].metadata.get("tool").and_then(|v| v.as_str()),
            Some("Write")
        );
    }

    #[test]
    fn test_parse_claude_stream_large_input_summarized() {
        let big_input = "x".repeat(1000);
        let stdout = format!(
            r#"{{"type":"assistant","message":{{"content":[{{"type":"tool_use","name":"Write","input":{{"content":"{}"}}}}]}}}}"#,
            big_input
        );
        let steps = parse_claude_stream_trajectory(&stdout, "claude");
        assert_eq!(steps.len(), 1);
        // Large content values show char count instead of the full string.
        assert!(steps[0].content.contains("Write("));
        assert!(steps[0].content.contains("<1000 chars>"));
    }

    #[test]
    fn test_parse_claude_stream_steps_indexed() {
        let stdout = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"a"}]}}
{"type":"assistant","message":{"content":[{"type":"text","text":"b"}]}}
{"type":"assistant","message":{"content":[{"type":"text","text":"c"}]}}"#;
        let steps = parse_claude_stream_trajectory(stdout, "claude");
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[0].index, 0);
        assert_eq!(steps[1].index, 1);
        assert_eq!(steps[2].index, 2);
    }

    // ── format_tool_input ──────────────────────────────────────────────────

    #[test]
    fn test_format_tool_input_read() {
        let input = serde_json::json!({
            "file_path": "src/main.rs",
            "limit": 60,
            "offset": 130
        });
        let result = format_tool_input("Read", &input);
        assert_eq!(result, r#"Read("src/main.rs", limit=60, offset=130)"#);
    }

    #[test]
    fn test_format_tool_input_bash() {
        let input = serde_json::json!({
            "command": "cargo test",
            "timeout": 120000
        });
        let result = format_tool_input("Bash", &input);
        assert_eq!(result, r#"Bash("cargo test", timeout=120000)"#);
    }

    #[test]
    fn test_format_tool_input_grep() {
        let input = serde_json::json!({
            "pattern": "fn main",
            "path": "src/"
        });
        let result = format_tool_input("Grep", &input);
        // "pattern" is the primary key
        assert!(result.starts_with("Grep(\"fn main\""));
        assert!(result.contains("path="));
    }

    #[test]
    fn test_format_tool_input_write_large_content() {
        let big_content = "x".repeat(500);
        let input = serde_json::json!({
            "file_path": "foo.rs",
            "content": big_content
        });
        let result = format_tool_input("Write", &input);
        assert!(result.contains("Write(\"foo.rs\""));
        assert!(result.contains("<500 chars>"));
        assert!(!result.contains("xxxxx"));
    }

    #[test]
    fn test_format_tool_input_empty() {
        let input = serde_json::json!({});
        assert_eq!(format_tool_input("Noop", &input), "Noop()");
    }

    #[test]
    fn test_format_tool_input_non_object() {
        let input = serde_json::json!("just a string");
        assert_eq!(format_tool_input("Noop", &input), "Noop()");
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
