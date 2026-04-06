use async_trait::async_trait;
use anyhow::Result;
use serde::{Deserialize, Serialize};

/// Role of a message in a conversation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    /// Tool-result role (OpenAI format).
    Tool,
}

/// A single message in a conversation with an LLM.
///
/// For backward compatibility, `content` is always a plain `String` —
/// existing callers that don't use tool-use continue to work unchanged.
/// Tool-use callers set `content_blocks` for structured content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    /// Plain text content (always populated for backward compat).
    pub role: MessageRole,
    pub content: String,
    /// Structured content blocks (Anthropic tool-use format).
    /// When present, providers use these instead of `content`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_blocks: Option<Vec<ContentBlock>>,
    /// Tool call ID (for `role: Tool` messages in OpenAI format).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    /// Create a system-role message.
    pub fn system(content: impl Into<String>) -> Self {
        Self { role: MessageRole::System, content: content.into(), content_blocks: None, tool_call_id: None }
    }

    /// Create a user-role message.
    pub fn user(content: impl Into<String>) -> Self {
        Self { role: MessageRole::User, content: content.into(), content_blocks: None, tool_call_id: None }
    }

    /// Create an assistant-role message.
    pub fn assistant(content: impl Into<String>) -> Self {
        Self { role: MessageRole::Assistant, content: content.into(), content_blocks: None, tool_call_id: None }
    }

    /// Create a user-role message containing tool results (Anthropic format).
    pub fn tool_results(results: Vec<ContentBlock>) -> Self {
        Self {
            role: MessageRole::User,
            content: String::new(),
            content_blocks: Some(results),
            tool_call_id: None,
        }
    }

    /// Create a tool-role message (OpenAI format).
    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Tool,
            content: content.into(),
            content_blocks: None,
            tool_call_id: Some(tool_call_id.into()),
        }
    }

    /// Create an assistant message with structured content blocks (tool-use response).
    pub fn assistant_with_blocks(text: &str, blocks: Vec<ContentBlock>) -> Self {
        Self {
            role: MessageRole::Assistant,
            content: text.to_string(),
            content_blocks: Some(blocks),
            tool_call_id: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Content blocks (for tool-use conversations)
// ---------------------------------------------------------------------------

/// A structured content block within a message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    /// Plain text content.
    #[serde(rename = "text")]
    Text { text: String },
    /// The model wants to call a tool (response from LLM).
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// Result of executing a tool (sent back to LLM).
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
}

// ---------------------------------------------------------------------------
// Tool definitions
// ---------------------------------------------------------------------------

/// A tool definition sent to the LLM so it knows what tools are available.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    /// Tool name (e.g. `"readFile"`, `"shell"`).
    pub name: String,
    /// Human-readable description of what the tool does.
    pub description: String,
    /// JSON Schema for the tool's input parameters.
    pub parameters: serde_json::Value,
}

/// How the LLM should choose among available tools.
#[derive(Debug, Clone)]
pub enum ToolChoice {
    /// The model decides whether to use a tool.
    Auto,
    /// The model must not use any tool.
    None,
    /// The model must use at least one tool.
    Required,
    /// The model must use this specific tool.
    Specific(String),
}

// ---------------------------------------------------------------------------
// Stop reason and token usage
// ---------------------------------------------------------------------------

/// Why the model stopped generating.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// The model finished its response normally.
    EndTurn,
    /// The model wants to call one or more tools.
    ToolUse,
    /// The response was cut off by the token limit.
    MaxTokens,
    /// A stop sequence was hit.
    StopSequence,
}

/// Token usage statistics from a completion request.
#[derive(Debug, Clone, Default)]
pub struct TokenUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

// ---------------------------------------------------------------------------
// Completion options and result
// ---------------------------------------------------------------------------

/// Options to pass to a completion request.
#[derive(Debug, Clone, Default)]
pub struct CompleteOptions {
    /// Sampling temperature (0.0 – 1.0).
    pub temperature: Option<f64>,
    /// Maximum number of tokens to generate.
    pub max_tokens: Option<u32>,
    /// Tool definitions available to the model.
    pub tools: Option<Vec<ToolDefinition>>,
    /// How the model should choose among tools.
    pub tool_choice: Option<ToolChoice>,
}

/// The result of a completion request.
#[derive(Debug, Clone)]
pub struct CompletionResult {
    /// The generated text content (plain text, always populated for compat).
    pub content: String,
    /// Whether the generation finished normally.
    pub done: bool,
    /// Structured content blocks from the response (includes tool-use blocks).
    pub content_blocks: Option<Vec<ContentBlock>>,
    /// Why the model stopped generating.
    pub stop_reason: Option<StopReason>,
    /// Token usage statistics.
    pub usage: Option<TokenUsage>,
}

impl Default for CompletionResult {
    fn default() -> Self {
        Self {
            content: String::new(),
            done: true,
            content_blocks: None,
            stop_reason: None,
            usage: None,
        }
    }
}

/// Trait implemented by every LLM backend.
#[async_trait]
pub trait LLMProvider: Send + Sync {
    /// Short identifier for the provider (e.g. `"anthropic"`).
    fn name(&self) -> &str;

    /// Model name used by this provider instance.
    fn model(&self) -> &str;

    /// Send `messages` to the LLM and return the completion.
    async fn complete(
        &self,
        messages: &[Message],
        options: Option<&CompleteOptions>,
    ) -> Result<CompletionResult>;
}

/// A no-op provider that never makes API calls.  Used for lightweight
/// operations (like Info pane refresh) that don't need LLM access.
pub struct NoopProvider;

#[async_trait]
impl LLMProvider for NoopProvider {
    fn name(&self) -> &str { "noop" }
    fn model(&self) -> &str { "noop" }
    async fn complete(
        &self,
        _messages: &[Message],
        _options: Option<&CompleteOptions>,
    ) -> Result<CompletionResult> {
        Err(anyhow::anyhow!("NoopProvider: no LLM available"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_constructors_backward_compat() {
        let m = Message::user("hello");
        assert_eq!(m.role, MessageRole::User);
        assert_eq!(m.content, "hello");
        assert!(m.content_blocks.is_none());
        assert!(m.tool_call_id.is_none());
    }

    #[test]
    fn test_message_tool_results() {
        let m = Message::tool_results(vec![
            ContentBlock::ToolResult {
                tool_use_id: "tool_123".into(),
                content: "file contents".into(),
                is_error: false,
            },
        ]);
        assert_eq!(m.role, MessageRole::User);
        assert!(m.content_blocks.is_some());
        assert_eq!(m.content_blocks.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn test_message_openai_tool_result() {
        let m = Message::tool_result("call_123", "result text");
        assert_eq!(m.role, MessageRole::Tool);
        assert_eq!(m.tool_call_id.as_deref(), Some("call_123"));
        assert_eq!(m.content, "result text");
    }

    #[test]
    fn test_completion_result_default() {
        let r = CompletionResult::default();
        assert_eq!(r.content, "");
        assert!(r.done);
        assert!(r.content_blocks.is_none());
        assert!(r.stop_reason.is_none());
    }

    #[test]
    fn test_content_block_serialization() {
        let block = ContentBlock::ToolUse {
            id: "tool_1".into(),
            name: "readFile".into(),
            input: serde_json::json!({"path": "README.md"}),
        };
        let json = serde_json::to_string(&block).unwrap();
        assert!(json.contains("\"type\":\"tool_use\""));
        assert!(json.contains("\"name\":\"readFile\""));
    }
}
