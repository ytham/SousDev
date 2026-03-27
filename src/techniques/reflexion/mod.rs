//! Reflexion: ReAct + written self-reflection between trials.
//!
//! After each failed trial the agent produces a written reflection on what
//! went wrong and how to improve.  This reflection is stored in a memory
//! window and prepended to the next trial's context.
//!
//! Algorithm (per trial):
//!   1. Build the task context (original task + prior reflections).
//!   2. Run a ReAct-style attempt using the LLM and tools.
//!   3. Evaluate success heuristic (non-empty answer = success for now).
//!   4. If failed, ask the LLM to reflect; store the reflection.
//!   5. Repeat up to `max_trials`.

use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use crate::providers::provider::{CompleteOptions, LLMProvider, Message};
use crate::tools::registry::ToolRegistry;
use crate::types::technique::{RunResult, StepType, TrajectoryStep};
use crate::utils::prompt_loader::PromptLoader;

const DEFAULT_MAX_TRIALS: usize = 3;
const DEFAULT_MEMORY_WINDOW: usize = 5;
const DEFAULT_MAX_INNER_ITERATIONS: usize = 8;

/// Options for [`run_reflexion`].
pub struct Options {
    /// The task to attempt.
    pub task: String,
    /// LLM provider.
    pub provider: Arc<dyn LLMProvider>,
    /// Optional tool registry.
    pub registry: Option<Arc<ToolRegistry>>,
    /// Optional system prompt override.
    pub system_prompt: Option<String>,
    /// Optional reflection prompt override.
    pub reflect_prompt: Option<String>,
    /// Maximum number of trial attempts.
    pub max_trials: Option<usize>,
    /// Maximum number of prior reflections to keep in context.
    pub memory_window: Option<usize>,
    /// Maximum inner loop iterations per trial.
    pub max_inner_iterations: Option<usize>,
    /// Harness root for loading prompt files.
    pub harness_root: Option<std::path::PathBuf>,
}

/// Run the Reflexion technique.
pub async fn run_reflexion(opts: Options) -> Result<RunResult> {
    let start = Instant::now();
    let max_trials = opts.max_trials.unwrap_or(DEFAULT_MAX_TRIALS);
    let memory_window = opts.memory_window.unwrap_or(DEFAULT_MEMORY_WINDOW);
    let max_inner = opts.max_inner_iterations.unwrap_or(DEFAULT_MAX_INNER_ITERATIONS);

    let system_prompt = opts
        .system_prompt
        .unwrap_or_else(|| DEFAULT_REFLEXION_SYSTEM.to_string());

    let reflect_template = opts
        .reflect_prompt
        .unwrap_or_else(|| DEFAULT_REFLECT_PROMPT.to_string());

    let mut reflections: Vec<String> = Vec::new();
    let mut trajectory: Vec<TrajectoryStep> = Vec::new();
    let mut total_llm_calls: usize = 0;
    let mut step_index: usize = 0;
    let mut best_answer = String::new();

    for trial in 0..max_trials {
        // Build context: task + recent reflections.
        let context = build_trial_context(&opts.task, &reflections, memory_window);

        // Inner ReAct loop for this trial.
        let (answer, inner_steps, llm_calls, success) = run_inner_loop(
            &context,
            &system_prompt,
            &opts.provider,
            opts.registry.as_ref(),
            max_inner,
        )
        .await?;

        total_llm_calls += llm_calls;

        // Re-index and append inner trajectory steps.
        for mut s in inner_steps {
            s.index = step_index;
            s.metadata.insert(
                "trial".to_string(),
                serde_json::Value::Number(trial.into()),
            );
            trajectory.push(s);
            step_index += 1;
        }

        if !answer.is_empty() {
            best_answer = answer.clone();
        }

        if success {
            let duration_ms = start.elapsed().as_millis() as u64;
            return Ok(RunResult::success(
                "reflexion",
                best_answer,
                trajectory,
                total_llm_calls,
                duration_ms,
            ));
        }

        // Produce a reflection for the next trial (unless this is the last one).
        if trial < max_trials - 1 {
            let reflection = produce_reflection(
                &opts.task,
                &answer,
                &reflect_template,
                &opts.provider,
                opts.harness_root.as_ref(),
            )
            .await?;
            total_llm_calls += 1;

            trajectory.push(TrajectoryStep {
                index: step_index,
                step_type: StepType::Reflection,
                content: reflection.clone(),
                timestamp: chrono::Utc::now().to_rfc3339(),
                metadata: {
                    let mut m = HashMap::new();
                    m.insert("trial".to_string(), serde_json::Value::Number(trial.into()));
                    m
                },
            });
            step_index += 1;

            reflections.push(reflection);
        }
    }

    let duration_ms = start.elapsed().as_millis() as u64;

    if best_answer.is_empty() {
        Ok(RunResult::failure(
            "reflexion",
            format!("All {} trials failed to produce an answer.", max_trials),
            trajectory,
            duration_ms,
        ))
    } else {
        Ok(RunResult::success(
            "reflexion",
            best_answer,
            trajectory,
            total_llm_calls,
            duration_ms,
        ))
    }
}

// ---------------------------------------------------------------------------
// Inner loop (simplified ReAct without recursion)
// ---------------------------------------------------------------------------

async fn run_inner_loop(
    task: &str,
    system_prompt: &str,
    provider: &Arc<dyn LLMProvider>,
    registry: Option<&Arc<ToolRegistry>>,
    max_iterations: usize,
) -> Result<(String, Vec<TrajectoryStep>, usize, bool)> {
    let mut messages = vec![
        Message::system(system_prompt),
        Message::user(task),
    ];
    let mut steps: Vec<TrajectoryStep> = Vec::new();
    let mut llm_calls = 0usize;
    let mut last_answer = String::new();

    for _iter in 0..max_iterations {
        let completion = provider
            .complete(
                &messages,
                Some(&CompleteOptions { max_tokens: Some(2048), ..Default::default() }),
            )
            .await?;
        llm_calls += 1;
        let response = completion.content.trim().to_string();
        last_answer = response.clone();

        steps.push(TrajectoryStep {
            index: 0, // will be re-indexed by caller
            step_type: StepType::Thought,
            content: response.clone(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            metadata: HashMap::new(),
        });

        // Try to extract a tool call.
        if let Some((tool_name, tool_args)) = crate::techniques::react::parse_tool_call_pub(&response) {
            steps.push(TrajectoryStep {
                index: 0,
                step_type: StepType::Action,
                content: format!("{}({})", tool_name, serde_json::to_string(&tool_args).unwrap_or_default()),
                timestamp: chrono::Utc::now().to_rfc3339(),
                metadata: {
                    let mut m = HashMap::new();
                    m.insert("tool".to_string(), serde_json::Value::String(tool_name.clone()));
                    m
                },
            });

            let observation = match registry {
                Some(reg) => reg.execute(&tool_name, &tool_args).await
                    .unwrap_or_else(|e| format!("Error: {}", e)),
                None => format!("Error: No registry — cannot execute '{}'", tool_name),
            };

            steps.push(TrajectoryStep {
                index: 0,
                step_type: StepType::Observation,
                content: observation.clone(),
                timestamp: chrono::Utc::now().to_rfc3339(),
                metadata: HashMap::new(),
            });

            messages.push(Message::assistant(&response));
            messages.push(Message::user(format!("Observation: {}", observation)));
        } else {
            // No tool call — final answer.
            let success = !response.is_empty();
            return Ok((response, steps, llm_calls, success));
        }
    }

    Ok((last_answer, steps, llm_calls, false))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn build_trial_context(task: &str, reflections: &[String], window: usize) -> String {
    if reflections.is_empty() {
        return task.to_string();
    }
    let recent: Vec<&String> = reflections.iter().rev().take(window).rev().collect();
    let reflection_block = recent
        .iter()
        .enumerate()
        .map(|(i, r)| format!("Reflection {}: {}", i + 1, r))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "{}\n\n---\nPrior reflections (use these to improve your approach):\n{}",
        task, reflection_block
    )
}

async fn produce_reflection(
    task: &str,
    last_answer: &str,
    template: &str,
    provider: &Arc<dyn LLMProvider>,
    harness_root: Option<&std::path::PathBuf>,
) -> Result<String> {
    let loader = harness_root
        .map(|r| PromptLoader::new(r))
        .unwrap_or_else(|| PromptLoader::new("."));
    let mut vars = HashMap::new();
    vars.insert("task".to_string(), task.to_string());
    vars.insert(
        "last_attempt".to_string(),
        if last_answer.is_empty() {
            "(no answer produced)".to_string()
        } else {
            last_answer.to_string()
        },
    );
    let prompt = loader.load(template, &vars).await.unwrap_or_else(|_| {
        format!(
            "Task: {}\nLast attempt: {}\n\nReflect on what went wrong and how to improve.",
            task, last_answer
        )
    });
    let messages = vec![Message::user(prompt)];
    let result = provider
        .complete(&messages, Some(&CompleteOptions { max_tokens: Some(512), ..Default::default() }))
        .await?;
    Ok(result.content.trim().to_string())
}

const DEFAULT_REFLEXION_SYSTEM: &str = r#"You are a reflective AI assistant. You reason step by step, using tools when needed.
After failed attempts you carefully reflect on your mistakes and adjust your strategy.

When you need to use a tool, respond with a JSON object in a fenced code block:
```json
{"tool": "tool_name", "args": {"arg1": "value1"}}
```

When you have the final answer, respond with plain text — no JSON tool block.
"#;

const DEFAULT_REFLECT_PROMPT: &str = r#"Task: {{task}}

Last attempt result: {{last_attempt}}

The attempt did not succeed. Please write a brief reflection:
1. What went wrong?
2. What should be done differently next time?
Keep it concise (2–4 sentences)."#;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_trial_context_no_reflections() {
        let ctx = build_trial_context("Fix the bug", &[], 5);
        assert_eq!(ctx, "Fix the bug");
    }

    #[test]
    fn test_build_trial_context_with_reflections() {
        let reflections = vec!["I forgot to import the module.".to_string()];
        let ctx = build_trial_context("Fix the bug", &reflections, 5);
        assert!(ctx.contains("Fix the bug"));
        assert!(ctx.contains("Prior reflections"));
        assert!(ctx.contains("forgot to import"));
    }

    #[test]
    fn test_build_trial_context_respects_window() {
        let reflections: Vec<String> = (0..10).map(|i| format!("reflection {}", i)).collect();
        let ctx = build_trial_context("task", &reflections, 3);
        // Only last 3 reflections should appear.
        assert!(ctx.contains("reflection 9"));
        assert!(ctx.contains("reflection 8"));
        assert!(ctx.contains("reflection 7"));
        assert!(!ctx.contains("reflection 6"));
    }

    #[tokio::test]
    async fn test_run_reflexion_succeeds_first_trial() {
        use crate::providers::provider::{CompletionResult, CompleteOptions};
        use async_trait::async_trait;

        struct MockProvider;
        #[async_trait]
        impl LLMProvider for MockProvider {
            fn name(&self) -> &str { "mock" }
            fn model(&self) -> &str { "mock" }
            async fn complete(&self, _msgs: &[Message], _opts: Option<&CompleteOptions>) -> Result<CompletionResult> {
                Ok(CompletionResult { content: "The answer is 42.".into(), done: true })
            }
        }

        let result = run_reflexion(Options {
            task: "What is 6*7?".into(),
            provider: Arc::new(MockProvider),
            registry: None,
            system_prompt: None,
            reflect_prompt: None,
            max_trials: Some(3),
            memory_window: Some(5),
            max_inner_iterations: Some(5),
            harness_root: None,
        })
        .await
        .unwrap();

        assert!(result.success);
        assert!(result.answer.contains("42"));
        assert_eq!(result.technique, "reflexion");
    }
}
