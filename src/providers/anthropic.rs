use async_trait::async_trait;
use anyhow::Result;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use super::provider::{
    CompleteOptions, CompletionResult, ContentBlock, LLMProvider, Message, MessageRole,
    StopReason, TokenUsage, ToolChoice,
};

/// Anthropic Claude provider using the Messages API with tool-use support.
pub struct AnthropicProvider {
    model: String,
    client: Client,
    api_key: String,
}

impl AnthropicProvider {
    /// Create a new provider instance.  Reads `ANTHROPIC_API_KEY` from the
    /// environment at construction time.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            client: Client::new(),
            api_key: std::env::var("ANTHROPIC_API_KEY").unwrap_or_default(),
        }
    }
}

// ---------------------------------------------------------------------------
// Wire types (Anthropic Messages API)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct AnthropicRequest {
    model: String,
    max_tokens: u32,
    messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<AnthropicTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<AnthropicToolChoice>,
}

#[derive(Serialize)]
struct AnthropicTool {
    name: String,
    description: String,
    input_schema: serde_json::Value,
}

#[derive(Serialize)]
struct AnthropicToolChoice {
    #[serde(rename = "type")]
    choice_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
}

/// An Anthropic message.  `content` can be either a simple string or an
/// array of content blocks (needed for tool-use conversations).
#[derive(Serialize)]
struct AnthropicMessage {
    role: String,
    content: AnthropicContent,
}

/// Serializes as a JSON string for simple text, or as an array of blocks
/// for structured content (tool_use, tool_result).
#[derive(Serialize)]
#[serde(untagged)]
enum AnthropicContent {
    Text(String),
    Blocks(Vec<AnthropicContentBlock>),
}

/// A single content block in a message or response.
#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type")]
enum AnthropicContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        is_error: bool,
    },
}

// -- Response types --

#[derive(Deserialize, Debug)]
struct AnthropicResponse {
    content: Vec<AnthropicContentBlock>,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    usage: Option<AnthropicUsage>,
}

#[derive(Deserialize, Debug)]
struct AnthropicUsage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
}

// ---------------------------------------------------------------------------
// LLMProvider implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl LLMProvider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    fn model(&self) -> &str {
        &self.model
    }

    async fn complete(
        &self,
        messages: &[Message],
        options: Option<&CompleteOptions>,
    ) -> Result<CompletionResult> {
        // Anthropic treats the system prompt as a top-level field, not a message.
        let system = messages
            .iter()
            .find(|m| m.role == MessageRole::System)
            .map(|m| m.content.clone());

        let api_messages: Vec<AnthropicMessage> = messages
            .iter()
            .filter(|m| m.role != MessageRole::System)
            .map(convert_message_to_anthropic)
            .collect();

        // Anthropic API requires max_tokens.  Use a generous default that
        // works with large context models (opus-4-6 supports 128K output,
        // sonnet-4 supports 64K).  If the caller specified a value, use it.
        let default_max_tokens = if self.model.contains("opus-4-6") {
            128_000
        } else {
            64_000
        };
        let max_tokens = options.and_then(|o| o.max_tokens).unwrap_or(default_max_tokens);
        let temperature = options.and_then(|o| o.temperature);

        // Convert tool definitions.
        let tools = options
            .and_then(|o| o.tools.as_ref())
            .map(|defs| {
                defs.iter()
                    .map(|d| AnthropicTool {
                        name: d.name.clone(),
                        description: d.description.clone(),
                        input_schema: d.parameters.clone(),
                    })
                    .collect::<Vec<_>>()
            });

        // Convert tool choice.
        let tool_choice = options
            .and_then(|o| o.tool_choice.as_ref())
            .map(|tc| match tc {
                ToolChoice::Auto => AnthropicToolChoice {
                    choice_type: "auto".into(),
                    name: None,
                },
                ToolChoice::None => AnthropicToolChoice {
                    choice_type: "none".into(),
                    name: None,
                },
                ToolChoice::Required => AnthropicToolChoice {
                    choice_type: "any".into(),
                    name: None,
                },
                ToolChoice::Specific(n) => AnthropicToolChoice {
                    choice_type: "tool".into(),
                    name: Some(n.clone()),
                },
            });

        let request = AnthropicRequest {
            model: self.model.clone(),
            max_tokens,
            messages: api_messages,
            system,
            temperature,
            tools,
            tool_choice,
        };

        let raw_response = self
            .client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&request)
            .send()
            .await?;

        if !raw_response.status().is_success() {
            let status = raw_response.status();
            let body = raw_response.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!(
                "Anthropic API error ({}): {}",
                status,
                body
            ));
        }

        let response = raw_response.json::<AnthropicResponse>().await?;

        // Extract text content.
        let text_content: String = response
            .content
            .iter()
            .filter_map(|block| match block {
                AnthropicContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");

        // Convert response blocks to our ContentBlock type.
        let content_blocks: Vec<ContentBlock> = response
            .content
            .iter()
            .map(|block| match block {
                AnthropicContentBlock::Text { text } => ContentBlock::Text {
                    text: text.clone(),
                },
                AnthropicContentBlock::ToolUse { id, name, input } => ContentBlock::ToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                },
                AnthropicContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => ContentBlock::ToolResult {
                    tool_use_id: tool_use_id.clone(),
                    content: content.clone(),
                    is_error: *is_error,
                },
            })
            .collect();

        // Map stop reason.
        let stop_reason = response.stop_reason.as_deref().map(|sr| match sr {
            "end_turn" => StopReason::EndTurn,
            "tool_use" => StopReason::ToolUse,
            "max_tokens" => StopReason::MaxTokens,
            "stop_sequence" => StopReason::StopSequence,
            _ => StopReason::EndTurn,
        });

        let usage = response.usage.map(|u| TokenUsage {
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
        });

        let has_tool_use = content_blocks
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolUse { .. }));

        Ok(CompletionResult {
            content: text_content,
            done: !has_tool_use,
            content_blocks: Some(content_blocks),
            stop_reason,
            usage,
        })
    }
}

/// Convert a provider `Message` to the Anthropic wire format.
fn convert_message_to_anthropic(msg: &Message) -> AnthropicMessage {
    let role = match msg.role {
        MessageRole::User | MessageRole::Tool => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::System => "user", // shouldn't happen (filtered above)
    };

    // If the message has structured content blocks, use them.
    if let Some(ref blocks) = msg.content_blocks {
        let api_blocks: Vec<AnthropicContentBlock> = blocks
            .iter()
            .map(|b| match b {
                ContentBlock::Text { text } => AnthropicContentBlock::Text {
                    text: text.clone(),
                },
                ContentBlock::ToolUse { id, name, input } => AnthropicContentBlock::ToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    input: input.clone(),
                },
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => AnthropicContentBlock::ToolResult {
                    tool_use_id: tool_use_id.clone(),
                    content: content.clone(),
                    is_error: *is_error,
                },
            })
            .collect();
        AnthropicMessage {
            role: role.to_string(),
            content: AnthropicContent::Blocks(api_blocks),
        }
    } else {
        AnthropicMessage {
            role: role.to_string(),
            content: AnthropicContent::Text(msg.content.clone()),
        }
    }
}
