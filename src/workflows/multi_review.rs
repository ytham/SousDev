//! Multi-model PR review support.
//!
//! When multiple AI model CLIs are available (auto-detected), the PR
//! reviewer runs them in parallel, each independently reviewing the
//! same PR.  A consolidation step merges all reviews into a single
//! timeline comment.

use std::path::Path;

use tokio::process::Command;

// ---------------------------------------------------------------------------
// Reviewer model detection
// ---------------------------------------------------------------------------

/// Supported external reviewer model CLIs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReviewerModel {
    /// Anthropic Claude CLI (`claude`).
    Claude,
    /// OpenAI Codex CLI (`codex`).
    Codex,
    /// Google Gemini CLI (`gemini`).
    Gemini,
}

impl ReviewerModel {
    /// Human-readable name for display and logging.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
        }
    }

    /// The technique string used by `ExternalAgentAdapter`.
    pub fn technique(&self) -> &'static str {
        match self {
            Self::Claude => "claude-loop",
            Self::Codex => "codex-loop",
            Self::Gemini => "gemini-loop",
        }
    }
}

impl std::fmt::Display for ReviewerModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name())
    }
}

/// Detect which reviewer model CLIs are available on the system.
///
/// A model is available when its CLI binary is on `$PATH` and, for models
/// that require it, the corresponding API key env var is set.
///
/// Claude CLI uses OAuth (no API key env var needed).
/// Codex CLI requires `OPENAI_API_KEY`.
/// Gemini CLI requires `GEMINI_API_KEY`.
pub async fn detect_available_reviewers_all() -> Vec<ReviewerModel> {
    let mut models = Vec::new();

    if cli_available("claude").await {
        models.push(ReviewerModel::Claude);
    }
    if cli_available("codex").await && env_set("OPENAI_API_KEY") {
        models.push(ReviewerModel::Codex);
    }
    if cli_available("gemini").await && env_set("GEMINI_API_KEY") {
        models.push(ReviewerModel::Gemini);
    }

    models
}

/// Detect which reviewer models are available, filtered to only include
/// models that are configured in the `[[models]]` array.
pub async fn detect_available_reviewers(
    models: &[crate::types::config::ModelConfig],
) -> Vec<ReviewerModel> {
    let mut available = Vec::new();
    for mc in models {
        let reviewer = match mc.provider.as_str() {
            "anthropic" => {
                if env_set("ANTHROPIC_API_KEY") || cli_available("claude").await {
                    Some(ReviewerModel::Claude)
                } else {
                    None
                }
            }
            "openai" => {
                if env_set("OPENAI_API_KEY") || cli_available("codex").await {
                    Some(ReviewerModel::Codex)
                } else {
                    None
                }
            }
            "ollama" => None, // No review support for Ollama
            _ => None,
        };
        if let Some(r) = reviewer {
            if !available.contains(&r) {
                available.push(r);
            }
        }
    }
    available
}

/// Check whether multi-model review should be used.
///
/// Returns the list of models to use, or `None` if single-model
/// review should be used instead.
///
/// Multi-model requires Claude (for consolidation) plus at least one
/// other model.
pub async fn resolve_multi_model(
    config_override: Option<bool>,
    models: &[crate::types::config::ModelConfig],
) -> Option<Vec<ReviewerModel>> {
    let enabled = config_override.unwrap_or(true);
    if !enabled || models.len() < 2 {
        return None;
    }

    let available = detect_available_reviewers(models).await;
    // Multi-model requires 2+ available models, and Claude must be one of them
    // (for consolidation).
    if available.len() >= 2 && available.contains(&ReviewerModel::Claude) {
        Some(available)
    } else {
        None
    }
}

/// Check if a CLI binary is available on `$PATH`.
async fn cli_available(binary: &str) -> bool {
    Command::new("which")
        .arg(binary)
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Check if an environment variable is set (and non-empty).
fn env_set(var: &str) -> bool {
    std::env::var(var)
        .map(|v| !v.is_empty())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Workspace convention loading
// ---------------------------------------------------------------------------

/// Load codebase conventions from the target repository workspace.
///
/// Reads `CLAUDE.md` and `.claude/skills/pr-review/SKILL.md` (if present)
/// from the workspace directory.  These are injected into the system prompt
/// for non-Claude models so they have the same codebase context that Claude
/// gets natively.
///
/// Returns the formatted convention text, or an empty string if no files
/// were found.
pub async fn load_workspace_conventions(workspace_dir: &Path) -> String {
    let mut conventions = String::new();

    // Primary conventions file.
    let claude_md = workspace_dir.join("CLAUDE.md");
    if let Ok(content) = tokio::fs::read_to_string(&claude_md).await {
        conventions.push_str("<codebase-conventions>\n");
        conventions.push_str(&content);
        conventions.push_str("\n</codebase-conventions>\n\n");
    }

    // Also try AGENTS.md as a fallback convention source.
    if conventions.is_empty() {
        let agents_md = workspace_dir.join("AGENTS.md");
        if let Ok(content) = tokio::fs::read_to_string(&agents_md).await {
            conventions.push_str("<codebase-conventions>\n");
            conventions.push_str(&content);
            conventions.push_str("\n</codebase-conventions>\n\n");
        }
    }

    // PR-review skill (provides structured review methodology).
    let skill_path = workspace_dir.join(".claude/skills/pr-review/SKILL.md");
    if let Ok(content) = tokio::fs::read_to_string(&skill_path).await {
        conventions.push_str("<review-skill>\n");
        conventions.push_str(&content);
        conventions.push_str("\n</review-skill>\n\n");
    }

    conventions
}

/// Build `--disallowedTools` flags appropriate for a given model.
///
/// Claude supports `--disallowedTools` natively.  Codex and Gemini do not,
/// so we return an empty vec for them (they rely on prompt-level constraints
/// in `pr-review.md` instead).
/// Build extra CLI flags appropriate for a given reviewer model.
///
/// For Claude: uses `--permission-mode auto` (read-only review doesn't need
/// `--dangerously-skip-permissions`) and `--disallowedTools` to block write
/// commands.  For Codex/Gemini: returns empty (they rely on prompt-level
/// constraints in `pr-review.md`).
pub fn disallowed_tools_for(model: ReviewerModel) -> Vec<String> {
    match model {
        ReviewerModel::Claude => vec![
            "--permission-mode".to_string(),
            "auto".to_string(),
            "--disallowedTools".to_string(),
            "Bash(gh pr review*)".to_string(),
            "Bash(gh pr comment*)".to_string(),
            "Bash(gh pr approve*)".to_string(),
            "Bash(gh pr merge*)".to_string(),
            "Bash(gh api --method POST*)".to_string(),
            "Bash(gh api --method PUT*)".to_string(),
            "Bash(gh api -X POST*)".to_string(),
            "Bash(gh api -X PUT*)".to_string(),
        ],
        ReviewerModel::Codex | ReviewerModel::Gemini => vec![],
    }
}

/// Create an LLM provider from a model config entry.
///
/// Returns `None` if the required API key is not set (CLI fallback should be used).
pub fn provider_for_model_config(
    config: &crate::types::config::ModelConfig,
) -> Option<std::sync::Arc<dyn crate::providers::provider::LLMProvider>> {
    match config.provider.as_str() {
        "anthropic" => {
            if env_set("ANTHROPIC_API_KEY") {
                Some(std::sync::Arc::new(
                    crate::providers::anthropic::AnthropicProvider::new(&config.model),
                ))
            } else {
                None
            }
        }
        "openai" => {
            if env_set("OPENAI_API_KEY") {
                Some(std::sync::Arc::new(
                    crate::providers::openai::OpenAIProvider::new(&config.model),
                ))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Create an LLM provider for a specific reviewer model, if an API key is available.
///
/// Returns `None` if the required API key is not set (CLI fallback should be used).
pub fn provider_for_review_model(
    model: ReviewerModel,
) -> Option<std::sync::Arc<dyn crate::providers::provider::LLMProvider>> {
    match model {
        ReviewerModel::Claude => {
            if env_set("ANTHROPIC_API_KEY") {
                let model_name = std::env::var("ANTHROPIC_MODEL")
                    .unwrap_or_else(|_| "claude-sonnet-4-20250514".to_string());
                Some(std::sync::Arc::new(
                    crate::providers::anthropic::AnthropicProvider::new(model_name),
                ))
            } else {
                None
            }
        }
        ReviewerModel::Codex => {
            if env_set("OPENAI_API_KEY") {
                let model_name = std::env::var("OPENAI_MODEL")
                    .unwrap_or_else(|_| "gpt-4o".to_string());
                Some(std::sync::Arc::new(
                    crate::providers::openai::OpenAIProvider::new(model_name),
                ))
            } else {
                None
            }
        }
        ReviewerModel::Gemini => {
            // Gemini doesn't have a harness-native provider yet — CLI only.
            None
        }
    }
}

/// Format multiple model review outputs for the consolidation prompt.
pub fn format_reviews_for_consolidation(reviews: &[(ReviewerModel, String)]) -> String {
    let mut output = String::new();
    for (model, review_text) in reviews {
        output.push_str(&format!(
            "### Review from {}\n\n{}\n\n---\n\n",
            model.name(),
            review_text
        ));
    }
    output
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_reviewer_model_names() {
        assert_eq!(ReviewerModel::Claude.name(), "claude");
        assert_eq!(ReviewerModel::Codex.name(), "codex");
        assert_eq!(ReviewerModel::Gemini.name(), "gemini");
    }

    #[test]
    fn test_reviewer_model_techniques() {
        assert_eq!(ReviewerModel::Claude.technique(), "claude-loop");
        assert_eq!(ReviewerModel::Codex.technique(), "codex-loop");
        assert_eq!(ReviewerModel::Gemini.technique(), "gemini-loop");
    }

    #[test]
    fn test_disallowed_tools_claude() {
        let flags = disallowed_tools_for(ReviewerModel::Claude);
        assert!(flags.contains(&"--disallowedTools".to_string()));
        assert!(flags.len() > 1);
    }

    #[test]
    fn test_disallowed_tools_codex_empty() {
        let flags = disallowed_tools_for(ReviewerModel::Codex);
        assert!(flags.is_empty());
    }

    #[test]
    fn test_disallowed_tools_gemini_empty() {
        let flags = disallowed_tools_for(ReviewerModel::Gemini);
        assert!(flags.is_empty());
    }

    #[tokio::test]
    async fn test_load_workspace_conventions_empty_dir() {
        let dir = TempDir::new().unwrap();
        let result = load_workspace_conventions(dir.path()).await;
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn test_load_workspace_conventions_with_claude_md() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "# My Project\nConventions here.")
            .unwrap();
        let result = load_workspace_conventions(dir.path()).await;
        assert!(result.contains("<codebase-conventions>"));
        assert!(result.contains("Conventions here."));
        assert!(result.contains("</codebase-conventions>"));
    }

    #[tokio::test]
    async fn test_load_workspace_conventions_with_skill() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "conventions").unwrap();
        let skill_dir = dir.path().join(".claude/skills/pr-review");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), "review skill content").unwrap();
        let result = load_workspace_conventions(dir.path()).await;
        assert!(result.contains("<review-skill>"));
        assert!(result.contains("review skill content"));
    }

    #[tokio::test]
    async fn test_load_workspace_conventions_agents_md_fallback() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "# Agents conventions").unwrap();
        let result = load_workspace_conventions(dir.path()).await;
        assert!(result.contains("Agents conventions"));
    }

    #[test]
    fn test_format_reviews_for_consolidation() {
        let reviews = vec![
            (ReviewerModel::Claude, "Claude found bug X.".to_string()),
            (ReviewerModel::Codex, "Codex found bug Y.".to_string()),
        ];
        let formatted = format_reviews_for_consolidation(&reviews);
        assert!(formatted.contains("### Review from claude"));
        assert!(formatted.contains("Claude found bug X."));
        assert!(formatted.contains("### Review from codex"));
        assert!(formatted.contains("Codex found bug Y."));
    }

    #[test]
    fn test_env_set_missing() {
        // Use a unique env var name that won't exist.
        assert!(!env_set("SOUSDEV_TEST_NONEXISTENT_VAR_XYZ123"));
    }
}
