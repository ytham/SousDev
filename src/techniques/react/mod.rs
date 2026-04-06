//! ReAct: Think → Act → Observe loop.
//!
//! The agent is prompted to reason through a task step by step.  When it
//! emits a JSON tool-call block (`{"tool":"name","args":{…}}`) the tool is
//! executed and the observation is fed back.  The loop terminates when:
//! - the LLM responds with plain text and no tool call (final answer), or
//! - `max_iterations` is reached.

use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use crate::providers::provider::{CompleteOptions, LLMProvider, Message};
use crate::tools::registry::ToolRegistry;
use crate::types::technique::{RunResult, StepType, TrajectoryStep};
use crate::utils::prompt_loader::PromptLoader;

/// Default maximum number of Think-Act-Observe iterations.
const DEFAULT_MAX_ITERATIONS: usize = 10;

/// Options for [`run_react`].
pub struct Options {
    /// The task or question the agent should work through.
    pub task: String,
    /// LLM provider used for all completions.
    pub provider: Arc<dyn LLMProvider>,
    /// Tool registry.  If `None`, tool calls in the agent's output are treated
    /// as unknown and an error observation is returned.
    pub registry: Option<Arc<ToolRegistry>>,
    /// Override the system prompt.  If `None`, a built-in default is used.
    pub system_prompt: Option<String>,
    /// Maximum Think-Act-Observe iterations before stopping.
    pub max_iterations: Option<usize>,
    /// Absolute path of the harness root, used to load the default system
    /// prompt from `prompts/react-system.md` when `system_prompt` is `None`.
    pub harness_root: Option<std::path::PathBuf>,
}

/// Run the ReAct (Reasoning + Acting) loop.
pub async fn run_react(opts: Options) -> Result<RunResult> {
    let start = Instant::now();
    let max_iterations = opts.max_iterations.unwrap_or(DEFAULT_MAX_ITERATIONS);

    // Load system prompt.
    let system_prompt = match opts.system_prompt {
        Some(s) => s,
        None => {
            let default_system = DEFAULT_REACT_SYSTEM_PROMPT.to_string();
            if let Some(ref root) = opts.harness_root {
                let loader = PromptLoader::new(root);
                let path = format!("{}/prompts/react-system.md", root.to_string_lossy());
                loader.load(&path, &HashMap::new()).await.unwrap_or(default_system)
            } else {
                default_system
            }
        }
    };

    let mut messages: Vec<Message> = vec![
        Message::system(&system_prompt),
        Message::user(&opts.task),
    ];

    let mut trajectory: Vec<TrajectoryStep> = Vec::new();
    let mut llm_calls: usize = 0;
    let mut step_index: usize = 0;

    for iteration in 0..max_iterations {
        let completion = opts
            .provider
            .complete(&messages, Some(&CompleteOptions { max_tokens: Some(2048), ..Default::default() }))
            .await?;
        llm_calls += 1;

        let response_text = completion.content.trim().to_string();

        // Record the thought step.
        trajectory.push(TrajectoryStep {
            index: step_index,
            step_type: StepType::Thought,
            content: response_text.clone(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            metadata: HashMap::new(),
        });
        step_index += 1;

        // Try to parse a tool call from the response.
        match parse_tool_call(&response_text) {
            Some((tool_name, tool_args)) => {
                // Record the action.
                trajectory.push(TrajectoryStep {
                    index: step_index,
                    step_type: StepType::Action,
                    content: format!("{}({})", tool_name, serde_json::to_string(&tool_args).unwrap_or_default()),
                    timestamp: chrono::Utc::now().to_rfc3339(),
                    metadata: {
                        let mut m = HashMap::new();
                        m.insert("tool".to_string(), serde_json::Value::String(tool_name.clone()));
                        m
                    },
                });
                step_index += 1;

                // Execute the tool.
                let observation = match &opts.registry {
                    Some(reg) => reg.execute(&tool_name, &tool_args).await
                        .unwrap_or_else(|e| format!("Error: {}", e)),
                    None => format!("Error: No tool registry available — cannot execute '{}'", tool_name),
                };

                // Record the observation.
                trajectory.push(TrajectoryStep {
                    index: step_index,
                    step_type: StepType::Observation,
                    content: observation.clone(),
                    timestamp: chrono::Utc::now().to_rfc3339(),
                    metadata: HashMap::new(),
                });
                step_index += 1;

                // Add assistant response + observation to the message history.
                messages.push(Message::assistant(&response_text));
                messages.push(Message::user(format!("Observation: {}", observation)));
            }
            None => {
                // No tool call — treat the response as the final answer.
                let duration_ms = start.elapsed().as_millis() as u64;
                return Ok(RunResult::success(
                    "react",
                    response_text,
                    trajectory,
                    llm_calls,
                    duration_ms,
                ));
            }
        }

        // After the last iteration, use the last response as the answer.
        if iteration == max_iterations - 1 {
            let answer = response_text;
            let duration_ms = start.elapsed().as_millis() as u64;
            return Ok(RunResult::success(
                "react",
                answer,
                trajectory,
                llm_calls,
                duration_ms,
            ));
        }
    }

    let duration_ms = start.elapsed().as_millis() as u64;
    Ok(RunResult::failure(
        "react",
        format!("Max iterations ({}) reached without a final answer.", max_iterations),
        trajectory,
        duration_ms,
    ))
}

// ---------------------------------------------------------------------------
// Tool call parsing
// ---------------------------------------------------------------------------

/// Public re-export for sibling technique modules (e.g. Reflexion) that
/// reuse the same tool-call parsing logic.
pub fn parse_tool_call_pub(
    text: &str,
) -> Option<(String, serde_json::Value)> {
    parse_tool_call(text)
}

/// Try to extract a tool call from `text`.
///
/// Accepts two formats:
/// 1. Fenced JSON block:
///    ```json
///    {"tool":"name","args":{…}}
///    ```
/// 2. `<tool>name</tool>` … `<args>{…}</args>` XML-style tags.
///
/// Returns `(tool_name, args_value)` or `None` if no tool call is found.
fn parse_tool_call(text: &str) -> Option<(String, serde_json::Value)> {
    // Strategy 1: look for a raw JSON object containing "tool" and "args".
    // Search within fenced code blocks or bare JSON.
    let candidates = extract_json_candidates(text);
    for candidate in candidates {
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&candidate) {
            if let (Some(tool), Some(args)) = (
                val.get("tool").and_then(|v| v.as_str()),
                val.get("args"),
            ) {
                return Some((tool.to_string(), args.clone()));
            }
        }
    }

    // Strategy 2: XML-style <tool>…</tool> and <args>…</args> tags.
    let tool_re = regex::Regex::new(r"(?s)<tool>\s*([^<]+?)\s*</tool>").ok()?;
    let args_re = regex::Regex::new(r"(?s)<args>\s*(\{.*?\})\s*</args>").ok()?;
    let tool_name = tool_re
        .captures(text)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().trim().to_string())?;
    let args = args_re
        .captures(text)
        .and_then(|c| c.get(1))
        .and_then(|m| serde_json::from_str::<serde_json::Value>(m.as_str()).ok())
        .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));

    Some((tool_name, args))
}

/// Extract all JSON-ish substrings from `text` that look like `{…}`.
fn extract_json_candidates(text: &str) -> Vec<String> {
    let mut candidates = Vec::new();

    // Fenced blocks: ```json\n…\n```
    let fence_re =
        regex::Regex::new(r"(?s)```(?:json)?\s*(\{.*?\})\s*```").unwrap();
    for cap in fence_re.captures_iter(text) {
        if let Some(m) = cap.get(1) {
            candidates.push(m.as_str().to_string());
        }
    }

    // Bare braces: the first top-level `{…}` block in the text.
    let mut depth: i32 = 0;
    let mut start: Option<usize> = None;
    for (i, ch) in text.char_indices() {
        match ch {
            '{' => {
                if depth == 0 {
                    start = Some(i);
                }
                depth += 1;
            }
            '}' => {
                depth -= 1;
                if depth == 0 {
                    if let Some(s) = start {
                        candidates.push(text[s..=i].to_string());
                        start = None;
                    }
                }
            }
            _ => {}
        }
    }

    candidates
}

// ---------------------------------------------------------------------------
// Default system prompt
// ---------------------------------------------------------------------------

const DEFAULT_REACT_SYSTEM_PROMPT: &str = r#"You are a helpful AI assistant that reasons step by step.

When you need to use a tool, respond with a JSON object in a fenced code block:
```json
{"tool": "tool_name", "args": {"arg1": "value1"}}
```

When you have the final answer, respond with plain text — no JSON tool block.

Think carefully before acting. After each tool observation, reflect on what you learned and decide the next step.
"#;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_tool_call_json_fenced() {
        let text = "I need to read a file.\n```json\n{\"tool\":\"readFile\",\"args\":{\"path\":\"src/main.rs\"}}\n```";
        let result = parse_tool_call(text);
        assert!(result.is_some());
        let (tool, args) = result.unwrap();
        assert_eq!(tool, "readFile");
        assert_eq!(args["path"], "src/main.rs");
    }

    #[test]
    fn test_parse_tool_call_bare_json() {
        let text = r#"Let me run this: {"tool":"shell","args":{"command":"ls -la"}}"#;
        let result = parse_tool_call(text);
        assert!(result.is_some());
        let (tool, args) = result.unwrap();
        assert_eq!(tool, "shell");
        assert_eq!(args["command"], "ls -la");
    }

    #[test]
    fn test_parse_tool_call_xml_style() {
        let text = "Using tool: <tool>readFile</tool>\n<args>{\"path\":\"README.md\"}</args>";
        let result = parse_tool_call(text);
        assert!(result.is_some());
        let (tool, _args) = result.unwrap();
        assert_eq!(tool, "readFile");
    }

    #[test]
    fn test_parse_tool_call_plain_text_none() {
        let text = "The answer is 42. No tools needed.";
        let result = parse_tool_call(text);
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_tool_call_json_missing_tool_key() {
        let text = r#"{"action":"read","path":"file.txt"}"#;
        let result = parse_tool_call(text);
        assert!(result.is_none());
    }

    #[test]
    fn test_extract_json_candidates_fenced() {
        let text = "Before\n```json\n{\"a\":1}\n```\nAfter";
        let candidates = extract_json_candidates(text);
        assert!(candidates.iter().any(|c| c.contains("\"a\"")));
    }

    #[test]
    fn test_extract_json_candidates_bare() {
        let text = r#"Some text {"tool":"x","args":{}} more text"#;
        let candidates = extract_json_candidates(text);
        assert!(!candidates.is_empty());
    }

    #[tokio::test]
    async fn test_run_react_returns_final_answer_immediately() {
        use crate::providers::provider::{CompletionResult, CompleteOptions};
        use async_trait::async_trait;

        struct MockProvider;

        #[async_trait]
        impl LLMProvider for MockProvider {
            fn name(&self) -> &str { "mock" }
            fn model(&self) -> &str { "mock" }
            async fn complete(
                &self,
                _messages: &[Message],
                _opts: Option<&CompleteOptions>,
            ) -> Result<CompletionResult> {
                Ok(CompletionResult {
                    content: "The final answer is 42.".to_string(),
                    done: true,
                    content_blocks: None,
                    stop_reason: None,
                    usage: None,
                })
            }
        }

        let result = run_react(Options {
            task: "What is 6 × 7?".into(),
            provider: Arc::new(MockProvider),
            registry: None,
            system_prompt: None,
            max_iterations: Some(5),
            harness_root: None,
        })
        .await
        .unwrap();

        assert!(result.success);
        assert_eq!(result.technique, "react");
        assert!(result.answer.contains("42"));
        assert_eq!(result.llm_calls, 1);
        // One thought step, no action/observation.
        assert_eq!(result.trajectory.len(), 1);
    }

    #[tokio::test]
    async fn test_run_react_tool_call_then_answer() {
        use crate::providers::provider::{CompletionResult, CompleteOptions};
        use crate::tools::registry::{Tool, ToolExecutor};
        use async_trait::async_trait;
        use serde_json::Value;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct TwoShotProvider {
            call: Arc<AtomicUsize>,
        }

        #[async_trait]
        impl LLMProvider for TwoShotProvider {
            fn name(&self) -> &str { "mock" }
            fn model(&self) -> &str { "mock" }
            async fn complete(
                &self,
                _messages: &[Message],
                _opts: Option<&CompleteOptions>,
            ) -> Result<CompletionResult> {
                let n = self.call.fetch_add(1, Ordering::SeqCst);
                let content = if n == 0 {
                    // First call: emit a tool call.
                    r#"```json
{"tool":"echo","args":{"input":"hello"}}
```"#.to_string()
                } else {
                    // Second call: plain answer.
                    "The echo said: hello".to_string()
                };
                Ok(CompletionResult { content, done: true, content_blocks: None, stop_reason: None, usage: None })
            }
        }

        struct EchoExecutor;

        #[async_trait]
        impl ToolExecutor for EchoExecutor {
            async fn execute(&self, args: &Value) -> Result<String> {
                Ok(args["input"].as_str().unwrap_or("").to_string())
            }
        }

        let mut reg = ToolRegistry::new();
        reg.register(Tool::new(
            "echo",
            "Echo input",
            serde_json::json!({}),
            Arc::new(EchoExecutor),
        ));

        let result = run_react(Options {
            task: "Echo the word hello".into(),
            provider: Arc::new(TwoShotProvider { call: Arc::new(AtomicUsize::new(0)) }),
            registry: Some(Arc::new(reg)),
            system_prompt: Some("You are helpful.".into()),
            max_iterations: Some(5),
            harness_root: None,
        })
        .await
        .unwrap();

        assert!(result.success);
        assert_eq!(result.llm_calls, 2);
        // thought + action + observation + thought(final)
        assert_eq!(result.trajectory.len(), 4);
    }
}
