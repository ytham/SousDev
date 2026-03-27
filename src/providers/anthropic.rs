use async_trait::async_trait;
use anyhow::Result;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use super::provider::{CompleteOptions, CompletionResult, LLMProvider, Message, MessageRole};

/// Anthropic Claude provider using the Messages API.
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
// Wire types
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
}

#[derive(Serialize, Deserialize)]
struct AnthropicMessage {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct AnthropicResponse {
    content: Vec<AnthropicContent>,
}

#[derive(Deserialize)]
struct AnthropicContent {
    text: Option<String>,
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
            .map(|m| AnthropicMessage {
                role: match m.role {
                    MessageRole::User => "user".to_string(),
                    MessageRole::Assistant => "assistant".to_string(),
                    // Should not reach here after the filter above, but be safe.
                    MessageRole::System => "user".to_string(),
                },
                content: m.content.clone(),
            })
            .collect();

        let max_tokens = options.and_then(|o| o.max_tokens).unwrap_or(8192);
        let temperature = options.and_then(|o| o.temperature);

        let request = AnthropicRequest {
            model: self.model.clone(),
            max_tokens,
            messages: api_messages,
            system,
            temperature,
        };

        let response = self
            .client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&request)
            .send()
            .await?
            .error_for_status()?
            .json::<AnthropicResponse>()
            .await?;

        let content = response
            .content
            .into_iter()
            .filter_map(|c| c.text)
            .collect::<Vec<_>>()
            .join("");

        Ok(CompletionResult { content, done: true })
    }
}
