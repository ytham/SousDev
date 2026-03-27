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
}

/// A single message in a conversation with an LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: MessageRole,
    pub content: String,
}

impl Message {
    /// Create a system-role message.
    pub fn system(content: impl Into<String>) -> Self {
        Self { role: MessageRole::System, content: content.into() }
    }

    /// Create a user-role message.
    pub fn user(content: impl Into<String>) -> Self {
        Self { role: MessageRole::User, content: content.into() }
    }

    /// Create an assistant-role message.
    pub fn assistant(content: impl Into<String>) -> Self {
        Self { role: MessageRole::Assistant, content: content.into() }
    }
}

/// Options to pass to a completion request.
#[derive(Debug, Clone, Default)]
pub struct CompleteOptions {
    /// Sampling temperature (0.0 – 1.0).
    pub temperature: Option<f64>,
    /// Maximum number of tokens to generate.
    pub max_tokens: Option<u32>,
}

/// The result of a completion request.
#[derive(Debug, Clone)]
pub struct CompletionResult {
    /// The generated text content.
    pub content: String,
    /// Whether the generation finished normally.
    pub done: bool,
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
