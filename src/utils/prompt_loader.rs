use anyhow::Result;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::fs;

/// Loads prompt templates from `.md` files or inline strings and performs
/// `{{variable}}` placeholder substitution.
pub struct PromptLoader {
    /// Root directory used to resolve relative template file paths.
    pub harness_root: PathBuf,
}

impl PromptLoader {
    /// Create a new [`PromptLoader`] anchored at `harness_root`.
    pub fn new(harness_root: impl Into<PathBuf>) -> Self {
        Self {
            harness_root: harness_root.into(),
        }
    }

    /// Resolve and render a prompt template.
    ///
    /// If `template` looks like a file path (single line ending in `.md`,
    /// `.txt`, or `.prompt`) the file is read from disk — absolute paths are
    /// used as-is, relative paths are resolved against `harness_root`.
    /// Otherwise `template` is treated as the literal template string.
    ///
    /// Every `{{key}}` occurrence in the resolved content is replaced with the
    /// corresponding value from `vars`.  Unknown placeholders are left as-is.
    pub async fn load(&self, template: &str, vars: &HashMap<String, String>) -> Result<String> {
        let content = if Self::is_file_path(template) {
            let path = if Path::new(template).is_absolute() {
                PathBuf::from(template)
            } else {
                self.harness_root.join(template)
            };
            fs::read_to_string(&path).await?
        } else {
            template.to_string()
        };

        Ok(Self::substitute(&content, vars))
    }

    /// Returns `true` when `s` is a single-line string ending in `.md`,
    /// `.txt`, or `.prompt` — heuristic used to distinguish file paths from
    /// inline template strings.
    fn is_file_path(s: &str) -> bool {
        !s.contains('\n')
            && (s.ends_with(".md") || s.ends_with(".txt") || s.ends_with(".prompt"))
    }

    /// Replace all `{{key}}` occurrences in `template` with values from `vars`.
    fn substitute(template: &str, vars: &HashMap<String, String>) -> String {
        let mut result = template.to_string();
        for (key, value) in vars {
            result = result.replace(&format!("{{{{{}}}}}", key), value);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tempfile::TempDir;

    // --- unit tests for pure helpers ---

    #[test]
    fn test_substitute_simple() {
        let vars: HashMap<String, String> =
            [("name".to_string(), "world".to_string())].into_iter().collect();
        let result = PromptLoader::substitute("Hello {{name}}!", &vars);
        assert_eq!(result, "Hello world!");
    }

    #[test]
    fn test_substitute_multiple() {
        let vars: HashMap<String, String> = [
            ("task".to_string(), "fix bug".to_string()),
            ("repo".to_string(), "owner/repo".to_string()),
        ]
        .into_iter()
        .collect();
        let result = PromptLoader::substitute("Task: {{task}} in {{repo}}", &vars);
        assert_eq!(result, "Task: fix bug in owner/repo");
    }

    #[test]
    fn test_substitute_missing_var_unchanged() {
        let vars: HashMap<String, String> = HashMap::new();
        let result = PromptLoader::substitute("Hello {{name}}!", &vars);
        assert_eq!(result, "Hello {{name}}!");
    }

    #[test]
    fn test_substitute_repeated_placeholder() {
        let vars: HashMap<String, String> =
            [("x".to_string(), "Y".to_string())].into_iter().collect();
        let result = PromptLoader::substitute("{{x}} and {{x}}", &vars);
        assert_eq!(result, "Y and Y");
    }

    #[test]
    fn test_is_file_path_md() {
        assert!(PromptLoader::is_file_path("prompts/code-review.md"));
        assert!(PromptLoader::is_file_path("some/path.txt"));
        assert!(PromptLoader::is_file_path("my.prompt"));
        assert!(!PromptLoader::is_file_path("Hello\nworld"));
        assert!(!PromptLoader::is_file_path("not a file"));
        assert!(!PromptLoader::is_file_path("inline template string"));
    }

    // --- async integration tests ---

    #[tokio::test]
    async fn test_load_inline_template() {
        let loader = PromptLoader::new("/tmp");
        let vars: HashMap<String, String> =
            [("lang".to_string(), "Rust".to_string())].into_iter().collect();
        let result = loader
            .load("Write code in {{lang}}.", &vars)
            .await
            .unwrap();
        assert_eq!(result, "Write code in Rust.");
    }

    #[tokio::test]
    async fn test_load_from_file_absolute() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("prompt.md");
        tokio::fs::write(&file_path, "Hello {{name}}!")
            .await
            .unwrap();

        let loader = PromptLoader::new(dir.path());
        let vars: HashMap<String, String> =
            [("name".to_string(), "agent".to_string())].into_iter().collect();
        let result = loader
            .load(file_path.to_str().unwrap(), &vars)
            .await
            .unwrap();
        assert_eq!(result, "Hello agent!");
    }

    #[tokio::test]
    async fn test_load_from_file_relative() {
        let dir = TempDir::new().unwrap();
        tokio::fs::write(dir.path().join("review.md"), "Review: {{pr}}")
            .await
            .unwrap();

        let loader = PromptLoader::new(dir.path());
        let vars: HashMap<String, String> =
            [("pr".to_string(), "#42".to_string())].into_iter().collect();
        let result = loader.load("review.md", &vars).await.unwrap();
        assert_eq!(result, "Review: #42");
    }

    #[tokio::test]
    async fn test_load_file_not_found_returns_error() {
        let loader = PromptLoader::new("/nonexistent/dir");
        let vars = HashMap::new();
        let result = loader.load("missing.md", &vars).await;
        assert!(result.is_err());
    }

    // ── Additional tests ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_load_inline_string_with_substitution() {
        let loader = PromptLoader::new("/tmp");
        let vars: HashMap<String, String> =
            [("name".to_string(), "Alice".to_string())].into_iter().collect();
        let result = loader.load("Hello {{name}}", &vars).await.unwrap();
        assert_eq!(result, "Hello Alice");
    }

    #[tokio::test]
    async fn test_load_inline_string_no_substitution() {
        let loader = PromptLoader::new("/tmp");
        let vars = HashMap::new();
        let result = loader.load("Hello world", &vars).await.unwrap();
        assert_eq!(result, "Hello world");
    }

    #[test]
    fn test_is_file_path_txt() {
        assert!(PromptLoader::is_file_path("templates/system.txt"));
    }

    #[test]
    fn test_is_file_path_prompt() {
        assert!(PromptLoader::is_file_path("review.prompt"));
    }

    #[test]
    fn test_is_file_path_with_newlines_is_not_file() {
        assert!(!PromptLoader::is_file_path("line1\nline2.md"));
    }

    #[test]
    fn test_is_file_path_no_extension() {
        assert!(!PromptLoader::is_file_path("some_string"));
    }

    #[test]
    fn test_substitute_empty_template() {
        let vars = HashMap::new();
        let result = PromptLoader::substitute("", &vars);
        assert_eq!(result, "");
    }

    #[test]
    fn test_substitute_no_placeholders() {
        let vars: HashMap<String, String> =
            [("key".to_string(), "val".to_string())].into_iter().collect();
        let result = PromptLoader::substitute("No placeholders here.", &vars);
        assert_eq!(result, "No placeholders here.");
    }

    #[tokio::test]
    async fn test_load_file_then_substitute() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("template.md");
        tokio::fs::write(&file_path, "Dear {{recipient}}, welcome to {{project}}!")
            .await
            .unwrap();

        let loader = PromptLoader::new(dir.path());
        let vars: HashMap<String, String> = [
            ("recipient".to_string(), "Bob".to_string()),
            ("project".to_string(), "Harness".to_string()),
        ]
        .into_iter()
        .collect();
        let result = loader.load("template.md", &vars).await.unwrap();
        assert_eq!(result, "Dear Bob, welcome to Harness!");
    }
}
