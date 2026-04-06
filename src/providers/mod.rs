pub mod anthropic;
pub mod ollama;
pub mod openai;
pub mod provider;

pub use anthropic::AnthropicProvider;
pub use ollama::OllamaProvider;
pub use openai::OpenAIProvider;
pub use provider::{CompleteOptions, CompletionResult, LLMProvider, Message, MessageRole};

use anyhow::Result;
use std::sync::Arc;
use crate::types::config::ModelConfig;

/// Resolve a concrete [`LLMProvider`] from a model configuration entry.
///
/// The `OLLAMA_BASE_URL` environment variable overrides the default
/// `http://localhost:11434` when the `ollama` provider is selected.
pub fn resolve_provider(model_config: &ModelConfig) -> Result<Arc<dyn LLMProvider>> {
    match model_config.provider.as_str() {
        "anthropic" => Ok(Arc::new(AnthropicProvider::new(model_config.model.clone()))),
        "openai" => Ok(Arc::new(OpenAIProvider::new(model_config.model.clone()))),
        "ollama" => {
            let base_url = std::env::var("OLLAMA_BASE_URL")
                .unwrap_or_else(|_| "http://localhost:11434".to_string());
            Ok(Arc::new(OllamaProvider::new(model_config.model.clone(), base_url)))
        }
        other => Err(anyhow::anyhow!("Unknown provider: {}", other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::config::ModelConfig;

    fn cfg(provider: &str) -> ModelConfig {
        ModelConfig {
            provider: provider.to_string(),
            model: "test-model".to_string(),
        }
    }

    #[test]
    fn resolve_anthropic() {
        let p = resolve_provider(&cfg("anthropic")).unwrap();
        assert_eq!(p.name(), "anthropic");
        assert_eq!(p.model(), "test-model");
    }

    #[test]
    fn resolve_openai() {
        let p = resolve_provider(&cfg("openai")).unwrap();
        assert_eq!(p.name(), "openai");
    }

    #[test]
    fn resolve_ollama() {
        let p = resolve_provider(&cfg("ollama")).unwrap();
        assert_eq!(p.name(), "ollama");
    }

    #[test]
    fn resolve_unknown_errors() {
        let err = resolve_provider(&cfg("unknown")).err().unwrap();
        assert!(err.to_string().contains("Unknown provider"));
    }
}
