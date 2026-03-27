use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::fs;
use tokio::process::Command;
use super::registry::{Tool, ToolExecutor};

// ---------------------------------------------------------------------------
// ReadFile
// ---------------------------------------------------------------------------

struct ReadFileExecutor;

#[async_trait]
impl ToolExecutor for ReadFileExecutor {
    async fn execute(&self, args: &Value) -> Result<String> {
        let path = args
            .get("path")
            .or_else(|| args.get("file_path"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("readFile: missing 'path' argument"))?;
        let content = fs::read_to_string(path).await?;
        Ok(content)
    }
}

/// Return a `Tool` that reads the contents of a file from disk.
pub fn read_file_tool() -> Tool {
    Tool::new(
        "readFile",
        "Read the contents of a file",
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path to read"}
            },
            "required": ["path"]
        }),
        Arc::new(ReadFileExecutor),
    )
}

// ---------------------------------------------------------------------------
// WriteFile
// ---------------------------------------------------------------------------

struct WriteFileExecutor;

#[async_trait]
impl ToolExecutor for WriteFileExecutor {
    async fn execute(&self, args: &Value) -> Result<String> {
        let path = args
            .get("path")
            .or_else(|| args.get("file_path"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("writeFile: missing 'path' argument"))?;
        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("writeFile: missing 'content' argument"))?;
        if let Some(parent) = std::path::Path::new(path).parent() {
            fs::create_dir_all(parent).await?;
        }
        fs::write(path, content).await?;
        Ok(format!("Written {} bytes to {}", content.len(), path))
    }
}

/// Return a `Tool` that writes content to a file, creating parent directories
/// as needed.
pub fn write_file_tool() -> Tool {
    Tool::new(
        "writeFile",
        "Write content to a file",
        json!({
            "type": "object",
            "properties": {
                "path":    {"type": "string", "description": "Destination file path"},
                "content": {"type": "string", "description": "Content to write"}
            },
            "required": ["path", "content"]
        }),
        Arc::new(WriteFileExecutor),
    )
}

// ---------------------------------------------------------------------------
// Shell
// ---------------------------------------------------------------------------

struct ShellExecutor;

#[async_trait]
impl ToolExecutor for ShellExecutor {
    async fn execute(&self, args: &Value) -> Result<String> {
        let command = args
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("shell: missing 'command' argument"))?;
        let output = Command::new("sh")
            .arg("-c")
            .arg(command)
            .output()
            .await?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if output.status.success() {
            Ok(stdout.into_owned())
        } else {
            Ok(format!("STDERR: {}\nSTDOUT: {}", stderr, stdout))
        }
    }
}

/// Return a `Tool` that runs an arbitrary shell command via `sh -c`.
///
/// On success the tool returns stdout.  On failure it returns a combined
/// `STDERR: … STDOUT: …` string so the LLM can diagnose what went wrong
/// without throwing an `Err`.
pub fn shell_tool() -> Tool {
    Tool::new(
        "shell",
        "Run a shell command",
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "Shell command to run"}
            },
            "required": ["command"]
        }),
        Arc::new(ShellExecutor),
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_write_and_read_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.txt");

        let write_tool = write_file_tool();
        let result = write_tool
            .execute(&json!({
                "path": path.to_str().unwrap(),
                "content": "hello world"
            }))
            .await
            .unwrap();
        assert!(result.contains("Written"));

        let read_tool = read_file_tool();
        let content = read_tool
            .execute(&json!({ "path": path.to_str().unwrap() }))
            .await
            .unwrap();
        assert_eq!(content, "hello world");
    }

    #[tokio::test]
    async fn test_read_nonexistent_file_errors() {
        let tool = read_file_tool();
        let result = tool
            .execute(&json!({"path": "/nonexistent/path/file.txt"}))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_shell_tool_success() {
        let tool = shell_tool();
        let result = tool
            .execute(&json!({"command": "echo hello"}))
            .await
            .unwrap();
        assert_eq!(result.trim(), "hello");
    }

    #[tokio::test]
    async fn test_shell_tool_stderr() {
        let tool = shell_tool();
        let result = tool
            .execute(&json!({"command": "echo err >&2 && exit 1"}))
            .await
            .unwrap();
        assert!(result.contains("err"));
    }

    #[tokio::test]
    async fn test_write_missing_path_errors() {
        let tool = write_file_tool();
        let result = tool.execute(&json!({"content": "hello"})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_write_creates_parent_dirs() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nested").join("dir").join("file.txt");

        let tool = write_file_tool();
        let result = tool
            .execute(&json!({
                "path": path.to_str().unwrap(),
                "content": "deep"
            }))
            .await
            .unwrap();
        assert!(result.contains("Written"));

        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(content, "deep");
    }

    #[tokio::test]
    async fn test_read_accepts_file_path_alias() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("alias.txt");
        tokio::fs::write(&path, "alias content").await.unwrap();

        let tool = read_file_tool();
        let content = tool
            .execute(&json!({ "file_path": path.to_str().unwrap() }))
            .await
            .unwrap();
        assert_eq!(content, "alias content");
    }
}
