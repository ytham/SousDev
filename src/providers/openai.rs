use async_trait::async_trait;
use anyhow::Result;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use super::provider::{CompleteOptions, CompletionResult, LLMProvider, Message, MessageRole};

/// OpenAI Chat Completions provider.
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
// Wire types
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct OpenAIRequest {
    model: String,
    messages: Vec<OpenAIMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
}

#[derive(Serialize, Deserialize)]
struct OpenAIMessage {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct OpenAIResponse {
    choices: Vec<OpenAIChoice>,
}

#[derive(Deserialize)]
struct OpenAIChoice {
    message: OpenAIMessage,
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
            .map(|m| OpenAIMessage {
                role: match m.role {
                    MessageRole::System => "system".to_string(),
                    MessageRole::User => "user".to_string(),
                    MessageRole::Assistant => "assistant".to_string(),
                },
                content: m.content.clone(),
            })
            .collect();

        let request = OpenAIRequest {
            model: self.model.clone(),
            messages: api_messages,
            max_tokens: options.and_then(|o| o.max_tokens),
            temperature: options.and_then(|o| o.temperature),
        };

        let response = self
            .client
            .post("https://api.openai.com/v1/chat/completions")
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&request)
            .send()
            .await?
            .error_for_status()?
            .json::<OpenAIResponse>()
            .await?;

        let content = response
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .unwrap_or_default();

        Ok(CompletionResult { content, done: true })
    }
}
