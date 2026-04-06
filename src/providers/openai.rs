use async_trait::async_trait;
use anyhow::Result;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use super::provider::{
    CompleteOptions, CompletionResult, ContentBlock, LLMProvider, Message, MessageRole,
    StopReason, TokenUsage, ToolChoice,
};

/// OpenAI Chat Completions provider with tool-use (function calling) support.
pub struct OpenAIProvider {
    model: String,
    client: Client,
    api_key: String,
}

impl OpenAIProvider {
    /// Create a new provider instance.  Reads `OPENAI_API_KEY` from the
    /// environment at construction time.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            client: Client::new(),
            api_key: std::env::var("OPENAI_API_KEY").unwrap_or_default(),
        }
    }
}

// ---------------------------------------------------------------------------
// Wire types (OpenAI Chat Completions API)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct OpenAIRequest {
    model: String,
    messages: Vec<OpenAIMessage>,
    /// Newer OpenAI models (GPT-5.4+) require `max_completion_tokens`
    /// instead of `max_tokens`.
    #[serde(skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<OpenAITool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<serde_json::Value>,
}

#[derive(Serialize)]
struct OpenAITool {
    #[serde(rename = "type")]
    tool_type: String,
    function: OpenAIFunction,
}

#[derive(Serialize)]
struct OpenAIFunction {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

/// An OpenAI message.  Supports plain text, assistant tool_calls, and tool results.
#[derive(Serialize, Deserialize, Debug)]
struct OpenAIMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OpenAIToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct OpenAIToolCall {
    id: String,
    #[serde(rename = "type")]
    call_type: String,
    function: OpenAIFunctionCall,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct OpenAIFunctionCall {
    name: String,
    arguments: String, // JSON-encoded string
}

// -- Response types --

#[derive(Deserialize, Debug)]
struct OpenAIResponse {
    choices: Vec<OpenAIChoice>,
    #[serde(default)]
    usage: Option<OpenAIUsage>,
}

#[derive(Deserialize, Debug)]
struct OpenAIChoice {
    message: OpenAIMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Debug)]
struct OpenAIUsage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
}

// ---------------------------------------------------------------------------
// LLMProvider implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl LLMProvider for OpenAIProvider {
    fn name(&self) -> &str {
        "openai"
    }

    fn model(&self) -> &str {
        &self.model
    }

    async fn complete(
        &self,
        messages: &[Message],
        options: Option<&CompleteOptions>,
    ) -> Result<CompletionResult> {
        let api_messages: Vec<OpenAIMessage> = messages
            .iter()
            .map(convert_message_to_openai)
            .collect();

        // Convert tool definitions.
        let tools = options
            .and_then(|o| o.tools.as_ref())
            .map(|defs| {
                defs.iter()
                    .map(|d| OpenAITool {
                        tool_type: "function".into(),
                        function: OpenAIFunction {
                            name: d.name.clone(),
                            description: d.description.clone(),
                            parameters: d.parameters.clone(),
                        },
                    })
                    .collect::<Vec<_>>()
            });

        // Convert tool choice.
        let tool_choice = options
            .and_then(|o| o.tool_choice.as_ref())
            .map(|tc| match tc {
                ToolChoice::Auto => serde_json::json!("auto"),
                ToolChoice::None => serde_json::json!("none"),
                ToolChoice::Required => serde_json::json!("required"),
                ToolChoice::Specific(n) => serde_json::json!({
                    "type": "function",
                    "function": {"name": n}
                }),
            });

        // GPT-5.4+ uses max_completion_tokens instead of max_tokens.
        // Default to 16384 when not specified.
        let max_completion_tokens = options
            .and_then(|o| o.max_tokens)
            .or(Some(16384));

        let request = OpenAIRequest {
            model: self.model.clone(),
            messages: api_messages,
            max_completion_tokens,
            temperature: options.and_then(|o| o.temperature),
            tools,
            tool_choice,
        };

        let raw_response = self
            .client
            .post("https://api.openai.com/v1/chat/completions")
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&request)
            .send()
            .await?;

        if !raw_response.status().is_success() {
            let status = raw_response.status();
            let body = raw_response.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!(
                "OpenAI API error ({}): {}",
                status,
                body
            ));
        }

        let response = raw_response.json::<OpenAIResponse>().await?;

        let choice = response.choices.into_iter().next().ok_or_else(|| {
            anyhow::anyhow!("OpenAI response contained no choices")
        })?;

        let text_content = choice.message.content.clone().unwrap_or_default();

        // Convert tool calls to ContentBlocks.
        let content_blocks: Option<Vec<ContentBlock>> = choice
            .message
            .tool_calls
            .as_ref()
            .map(|calls| {
                let mut blocks: Vec<ContentBlock> = Vec::new();
                if !text_content.is_empty() {
                    blocks.push(ContentBlock::Text {
                        text: text_content.clone(),
                    });
                }
                for call in calls {
                    let input: serde_json::Value =
                        serde_json::from_str(&call.function.arguments)
                            .unwrap_or(serde_json::Value::Object(Default::default()));
                    blocks.push(ContentBlock::ToolUse {
                        id: call.id.clone(),
                        name: call.function.name.clone(),
                        input,
                    });
                }
                blocks
            });

        // Map finish reason to StopReason.
        let stop_reason = choice.finish_reason.as_deref().map(|fr| match fr {
            "stop" => StopReason::EndTurn,
            "tool_calls" => StopReason::ToolUse,
            "length" => StopReason::MaxTokens,
            _ => StopReason::EndTurn,
        });

        let has_tool_calls = choice.message.tool_calls.is_some();

        let usage = response.usage.map(|u| TokenUsage {
            input_tokens: u.prompt_tokens,
            output_tokens: u.completion_tokens,
        });

        Ok(CompletionResult {
            content: text_content,
            done: !has_tool_calls,
            content_blocks,
            stop_reason,
            usage,
        })
    }
}

/// Convert a provider `Message` to OpenAI wire format.
fn convert_message_to_openai(msg: &Message) -> OpenAIMessage {
    let role = match msg.role {
        MessageRole::System => "system",
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::Tool => "tool",
    };

    // Assistant messages with tool-use content blocks need tool_calls.
    if msg.role == MessageRole::Assistant {
        if let Some(ref blocks) = msg.content_blocks {
            let tool_calls: Vec<OpenAIToolCall> = blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolUse { id, name, input } => Some(OpenAIToolCall {
                        id: id.clone(),
                        call_type: "function".into(),
                        function: OpenAIFunctionCall {
                            name: name.clone(),
                            arguments: serde_json::to_string(input).unwrap_or_default(),
                        },
                    }),
                    _ => None,
                })
                .collect();

            if !tool_calls.is_empty() {
                return OpenAIMessage {
                    role: role.to_string(),
                    content: if msg.content.is_empty() {
                        None
                    } else {
                        Some(msg.content.clone())
                    },
                    tool_calls: Some(tool_calls),
                    tool_call_id: None,
                };
            }
        }
    }

    // Tool-result messages need tool_call_id.
    if msg.role == MessageRole::Tool {
        return OpenAIMessage {
            role: role.to_string(),
            content: Some(msg.content.clone()),
            tool_calls: None,
            tool_call_id: msg.tool_call_id.clone(),
        };
    }

    // Plain text message.
    OpenAIMessage {
        role: role.to_string(),
        content: Some(msg.content.clone()),
        tool_calls: None,
        tool_call_id: None,
    }
}
