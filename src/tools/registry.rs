use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

/// Executes a named tool given a JSON argument object.
#[async_trait]
pub trait ToolExecutor: Send + Sync {
    async fn execute(&self, args: &Value) -> Result<String>;
}

/// A registered tool with its schema and executor.
pub struct Tool {
    /// Tool name used to invoke it (e.g. `"readFile"`).
    pub name: String,
    /// Human-readable description exposed to the LLM.
    pub description: String,
    /// JSON Schema describing the accepted arguments.
    pub parameters: Value,
    executor: Arc<dyn ToolExecutor>,
}

impl Tool {
    /// Create a new tool.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
        executor: Arc<dyn ToolExecutor>,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
            executor,
        }
    }

    /// Execute this tool with the given JSON arguments.
    pub async fn execute(&self, args: &Value) -> Result<String> {
        self.executor.execute(args).await
    }
}

/// Registry mapping tool names to their implementations.
pub struct ToolRegistry {
    tools: HashMap<String, Tool>,
}

impl ToolRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self { tools: HashMap::new() }
    }

    /// Register a tool.  Any previous registration with the same name is replaced.
    pub fn register(&mut self, tool: Tool) {
        self.tools.insert(tool.name.clone(), tool);
    }

    /// Look up a tool by name.
    pub fn get(&self, name: &str) -> Option<&Tool> {
        self.tools.get(name)
    }

    /// Return all registered tools in arbitrary order.
    pub fn all(&self) -> Vec<&Tool> {
        self.tools.values().collect()
    }

    /// Execute a named tool, returning an error if it is not registered.
    pub async fn execute(&self, name: &str, args: &Value) -> Result<String> {
        match self.tools.get(name) {
            Some(tool) => tool.execute(args).await,
            None => Err(anyhow::anyhow!("Unknown tool: {}", name)),
        }
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Convenience constructor: build a [`ToolRegistry`] from a `Vec<Tool>`.
pub fn create_registry(tools: Vec<Tool>) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    for tool in tools {
        registry.register(tool);
    }
    registry
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct EchoTool;

    #[async_trait]
    impl ToolExecutor for EchoTool {
        async fn execute(&self, args: &Value) -> Result<String> {
            Ok(args
                .get("input")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string())
        }
    }

    #[tokio::test]
    async fn test_registry_execute_known_tool() {
        let mut registry = ToolRegistry::new();
        registry.register(Tool::new(
            "echo",
            "Echo input",
            json!({"type": "object", "properties": {"input": {"type": "string"}}}),
            Arc::new(EchoTool),
        ));
        let result = registry
            .execute("echo", &json!({"input": "hello"}))
            .await
            .unwrap();
        assert_eq!(result, "hello");
    }

    #[tokio::test]
    async fn test_registry_execute_unknown_tool() {
        let registry = ToolRegistry::new();
        let result = registry.execute("nonexistent", &json!({})).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Unknown tool"));
    }

    #[test]
    fn test_registry_get() {
        let mut registry = ToolRegistry::new();
        registry.register(Tool::new(
            "echo",
            "Echo",
            json!({}),
            Arc::new(EchoTool),
        ));
        assert!(registry.get("echo").is_some());
        assert!(registry.get("unknown").is_none());
    }

    #[test]
    fn test_create_registry() {
        let tools = vec![
            Tool::new("echo", "Echo", json!({}), Arc::new(EchoTool)),
        ];
        let registry = create_registry(tools);
        assert!(registry.get("echo").is_some());
    }

    #[test]
    fn test_all_returns_all_tools() {
        let mut registry = ToolRegistry::new();
        registry.register(Tool::new("a", "A", json!({}), Arc::new(EchoTool)));
        registry.register(Tool::new("b", "B", json!({}), Arc::new(EchoTool)));
        assert_eq!(registry.all().len(), 2);
    }
}
