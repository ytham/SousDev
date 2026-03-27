---
name: add-technique
description: Add a new standalone agentic reasoning technique to SousDev following the established module pattern with Options struct, run function, and tests.
---

## When to use

Use this when implementing a new agentic reasoning algorithm (e.g. a new prompting strategy,
a new search/sampling method, a new multi-agent pattern).

## Steps

1. **Create the module** at `src/techniques/<name>/mod.rs`:

```rust
use anyhow::Result;
use std::sync::Arc;
use crate::providers::provider::{LLMProvider, Message, CompleteOptions, CompletionResult};
use crate::types::technique::{RunResult, TrajectoryStep, StepType};

const DEFAULT_<PARAM>: usize = <default>;

pub struct Options {
    pub task: String,
    pub provider: Arc<dyn LLMProvider>,
    // technique-specific options — all optional with defaults
    pub <param>: Option<usize>,
}

pub async fn run_<name>(opts: Options) -> Result<RunResult> {
    let start = std::time::Instant::now();
    let <param> = opts.<param>.unwrap_or(DEFAULT_<PARAM>);
    let mut trajectory = Vec::new();
    let mut llm_calls = 0usize;

    // Implement the algorithm:
    // 1. Each LLM call: provider.complete(&messages, Some(&options)).await?
    // 2. Increment llm_calls after each call
    // 3. Record TrajectoryStep for each meaningful step
    // 4. Return RunResult::success or RunResult::failure

    let duration_ms = start.elapsed().as_millis() as u64;
    Ok(RunResult::success("<name>", answer, trajectory, llm_calls, duration_ms))
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    struct MockProvider { responses: Vec<String> }

    #[async_trait]
    impl LLMProvider for MockProvider {
        fn name(&self) -> &str { "mock" }
        fn model(&self) -> &str { "mock" }
        async fn complete(&self, _messages: &[Message], _options: Option<&CompleteOptions>) -> Result<CompletionResult> {
            // Return responses in sequence; cycle if exhausted
            Ok(CompletionResult { content: self.responses[0].clone(), done: true })
        }
    }

    #[tokio::test]
    async fn test_<name>_basic() {
        // Test with mock provider
    }

    #[tokio::test]
    async fn test_<name>_returns_trajectory() {
        // Verify trajectory is populated
    }
}
```

2. **Register in `src/techniques/mod.rs`** — add `pub mod <name>;`

3. **Add CLI support in `src/main.rs`**:
   - Add entry to the `TECHNIQUES` constant (name, description, paper citation)
   - Add a match arm in the `Commands::Run` handler
   - Map CLI flags to the `Options` struct
   - Add technique-specific CLI args if needed (e.g. `--<param>`)

4. **Add config defaults in `src/types/config.rs`**:
   - Add `<Name>Config` struct with optional fields
   - Add `pub <name>: Option<<Name>Config>` to `TechniquesConfig`
   - Wire defaults from config into the CLI match arm in main.rs

5. **Write at least 3 tests**:
   - Basic success path with mock provider
   - Trajectory is populated with correct step types
   - Edge case (e.g. empty response, max iterations hit)

6. **Run `cargo test` and `cargo clippy`**.

## Key conventions

- Technique name in `RunResult.technique` must match the module name
- All LLM calls use `provider.complete()` — never call APIs directly
- `TrajectoryStep.timestamp` should use `chrono::Utc::now().to_rfc3339()`
- Return `RunResult::failure` on error — do not panic or propagate `Err` for
  expected failures (e.g. LLM returned empty)
