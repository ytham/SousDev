use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use crate::types::config::HarnessConfig;

/// Config file names searched for (in priority order).
const CONFIG_FILENAMES: &[&str] = &["config.toml"];

/// Walk up the directory tree from `start_dir` (defaults to the current working
/// directory) looking for `config.toml`.
///
/// Returns the deserialised [`HarnessConfig`] and the directory that contained
/// the config file (the project root).
///
/// # Errors
///
/// Returns an error if no config file is found in the entire ancestor chain,
/// or if the file cannot be read or parsed.
pub async fn load_config(start_dir: Option<&Path>) -> Result<(HarnessConfig, PathBuf)> {
    let start = start_dir
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

    let mut current: &Path = &start;
    loop {
        for filename in CONFIG_FILENAMES {
            let candidate = current.join(filename);
            if candidate.exists() {
                let content = tokio::fs::read_to_string(&candidate)
                    .await
                    .with_context(|| format!("Reading {}", candidate.display()))?;
                let config: HarnessConfig = toml::from_str(&content)
                    .with_context(|| format!("Parsing {}", candidate.display()))?;
                return Ok((config, current.to_path_buf()));
            }
        }
        match current.parent() {
            Some(parent) => current = parent,
            None => {
                return Err(anyhow::anyhow!(
                    "config.toml not found (searched from {})",
                    start.display()
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tokio::fs;

    #[tokio::test]
    async fn test_load_config_found() {
        let dir = TempDir::new().unwrap();
        let config_content = r#"
target_repo = "owner/repo"

[[models]]
provider = "anthropic"
model = "claude-opus-4-6"
"#;
        fs::write(dir.path().join("config.toml"), config_content)
            .await
            .unwrap();

        let (config, root) = load_config(Some(dir.path())).await.unwrap();
        assert_eq!(config.models[0].provider, "anthropic");
        assert_eq!(config.models[0].model, "claude-opus-4-6");
        assert_eq!(config.target_repo.as_deref(), Some("owner/repo"));
        assert_eq!(root, dir.path());
    }

    #[tokio::test]
    async fn test_load_config_not_found() {
        let dir = TempDir::new().unwrap();
        let result = load_config(Some(dir.path())).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("config.toml not found"));
    }

    #[tokio::test]
    async fn test_load_config_walks_up() {
        let dir = TempDir::new().unwrap();
        let subdir = dir.path().join("a").join("b").join("c");
        fs::create_dir_all(&subdir).await.unwrap();
        let config_content = r#"
[[models]]
provider = "openai"
model = "gpt-4o"
"#;
        fs::write(dir.path().join("config.toml"), config_content)
            .await
            .unwrap();

        let (config, root) = load_config(Some(&subdir)).await.unwrap();
        assert_eq!(config.models[0].provider, "openai");
        assert_eq!(config.models[0].model, "gpt-4o");
        assert_eq!(root, dir.path());
    }

    #[tokio::test]
    async fn test_load_config_uses_cwd_when_no_start_given() {
        // This test verifies the function doesn't panic when called with None.
        // It will either find a config somewhere in the real filesystem ancestry
        // or return an error — both outcomes are acceptable here.
        let result = load_config(None).await;
        // We just confirm it doesn't panic; we don't assert on success/failure
        // because the real CWD may or may not contain a config file.
        let _ = result;
    }

    #[tokio::test]
    async fn test_load_config_parse_error() {
        let dir = TempDir::new().unwrap();
        // Write deliberately invalid TOML
        fs::write(
            dir.path().join("config.toml"),
            "this is [not valid toml !!!",
        )
        .await
        .unwrap();

        let result = load_config(Some(dir.path())).await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("Parsing") || msg.contains("config.toml"));
    }

    #[tokio::test]
    async fn test_load_config_full_fields() {
        let dir = TempDir::new().unwrap();
        let config_content = r#"
git_method = "ssh"

[logging]
level = "warn"
pretty = false

[techniques.react]
max_iterations = 5

[techniques.tree_of_thoughts]
branching = 3
strategy = "bfs"
max_depth = 4
score_threshold = 0.7

[[models]]
provider = "ollama"
model = "llama3"
"#;
        fs::write(dir.path().join("config.toml"), config_content)
            .await
            .unwrap();

        let (config, _root) = load_config(Some(dir.path())).await.unwrap();
        assert_eq!(config.models[0].provider, "ollama");
        assert_eq!(config.git_method.as_deref(), Some("ssh"));

        let log = config.logging.as_ref().unwrap();
        assert_eq!(log.level.as_deref(), Some("warn"));
        assert_eq!(log.pretty, Some(false));

        let tech = config.techniques.as_ref().unwrap();
        let tot = tech.tree_of_thoughts.as_ref().unwrap();
        assert_eq!(tot.branching, Some(3));
        assert_eq!(tot.strategy.as_deref(), Some("bfs"));
        assert_eq!(tot.score_threshold, Some(0.7));
    }
}
