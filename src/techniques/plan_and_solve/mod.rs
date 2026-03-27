//! Plan-and-Solve (PS+): generate an explicit step-by-step plan, then execute.
//!
//! Unlike ReAct (which interleaves reasoning and acting), Plan-and-Solve first
//! asks the LLM to produce a comprehensive plan, then executes each step in
//! sequence, gathering observations along the way.
//!
//! The `detailed_plan` flag enables PS+ mode: the plan prompt asks for not
//! just the steps, but also sub-steps and caveats.  This tends to reduce
//! calculation errors and missed steps.

use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use crate::providers::provider::{CompleteOptions, LLMProvider, Message};
use crate::tools::registry::ToolRegistry;
use crate::types::technique::{RunResult, StepType, TrajectoryStep};

/// Options for [`run_plan_and_solve`].
pub struct Options {
    /// The task or problem.
    pub task: String,
    /// LLM provider.
    pub provider: Arc<dyn LLMProvider>,
    /// Optional tool registry for executing actions.
    pub registry: Option<Arc<ToolRegistry>>,
    /// Use detailed PS+ planning (sub-steps and caveats).
    pub detailed_plan: Option<bool>,
    /// Maximum steps to execute from the plan.
    pub max_steps: Option<usize>,
}

/// Run the Plan-and-Solve algorithm.
pub async fn run_plan_and_solve(opts: Options) -> Result<RunResult> {
    let start = Instant::now();
    let detailed = opts.detailed_plan.unwrap_or(false);
    let max_steps = opts.max_steps.unwrap_or(20);

    let mut trajectory: Vec<TrajectoryStep> = Vec::new();
    let mut llm_calls: usize = 0;
    let mut step_index: usize = 0;

    // ── Phase 1: Generate the plan ────────────────────────────────────────────
    let plan_prompt = build_plan_prompt(&opts.task, detailed);
    let msgs = vec![Message::user(&plan_prompt)];
    let plan_completion = opts
        .provider
        .complete(&msgs, Some(&CompleteOptions { max_tokens: Some(2048), ..Default::default() }))
        .await?;
    llm_calls += 1;

    let plan_text = plan_completion.content.trim().to_string();

    trajectory.push(TrajectoryStep {
        index: step_index,
        step_type: StepType::Thought,
        content: format!("Plan:\n{}", plan_text),
        timestamp: chrono::Utc::now().to_rfc3339(),
        metadata: {
            let mut m = HashMap::new();
            m.insert("phase".to_string(), serde_json::json!("planning"));
            m
        },
    });
    step_index += 1;

    // Extract individual steps from the plan.
    let steps = extract_plan_steps(&plan_text);

    // ── Phase 2: Execute each plan step ───────────────────────────────────────
    let mut messages: Vec<Message> = vec![
        Message::user(opts.task.clone()),
        Message::assistant(format!("My plan:\n{}", plan_text)),
    ];

    for (i, step_text) in steps.iter().enumerate().take(max_steps) {
        let execute_prompt = format!(
            "Now execute step {}: {}",
            i + 1,
            step_text
        );
        messages.push(Message::user(execute_prompt.clone()));

        let completion = opts
            .provider
            .complete(&messages, Some(&CompleteOptions { max_tokens: Some(1024), ..Default::default() }))
            .await?;
        llm_calls += 1;

        let response = completion.content.trim().to_string();

        // Record the action.
        trajectory.push(TrajectoryStep {
            index: step_index,
            step_type: StepType::Action,
            content: format!("Step {}: {}", i + 1, step_text),
            timestamp: chrono::Utc::now().to_rfc3339(),
            metadata: {
                let mut m = HashMap::new();
                m.insert("step_number".to_string(), serde_json::json!(i + 1));
                m
            },
        });
        step_index += 1;

        // Check for a tool call in the response.
        let observation = if let Some((tool_name, tool_args)) =
            crate::techniques::react::parse_tool_call_pub(&response)
        {
            let obs = match &opts.registry {
                Some(reg) => reg.execute(&tool_name, &tool_args).await
                    .unwrap_or_else(|e| format!("Error: {}", e)),
                None => format!("Error: No registry to execute '{}'", tool_name),
            };
            messages.push(Message::assistant(&response));
            messages.push(Message::user(format!("Observation: {}", obs)));
            Some(obs)
        } else {
            messages.push(Message::assistant(&response));
            None
        };

        // Record thought.
        trajectory.push(TrajectoryStep {
            index: step_index,
            step_type: StepType::Thought,
            content: response.clone(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            metadata: {
                let mut m = HashMap::new();
                m.insert("step_number".to_string(), serde_json::json!(i + 1));
                m
            },
        });
        step_index += 1;

        // Record observation if there was a tool call.
        if let Some(obs) = observation {
            trajectory.push(TrajectoryStep {
                index: step_index,
                step_type: StepType::Observation,
                content: obs,
                timestamp: chrono::Utc::now().to_rfc3339(),
                metadata: HashMap::new(),
            });
            step_index += 1;
        }
    }

    // ── Phase 3: Synthesise final answer ──────────────────────────────────────
    let final_prompt = "Based on all the steps above, provide a clear, concise final answer.".to_string();
    messages.push(Message::user(final_prompt));

    let final_completion = opts
        .provider
        .complete(&messages, Some(&CompleteOptions { max_tokens: Some(1024), ..Default::default() }))
        .await?;
    llm_calls += 1;

    let answer = final_completion.content.trim().to_string();

    trajectory.push(TrajectoryStep {
        index: step_index,
        step_type: StepType::Thought,
        content: format!("Final answer: {}", answer),
        timestamp: chrono::Utc::now().to_rfc3339(),
        metadata: {
            let mut m = HashMap::new();
            m.insert("phase".to_string(), serde_json::json!("synthesis"));
            m
        },
    });

    let duration_ms = start.elapsed().as_millis() as u64;
    Ok(RunResult::success(
        "plan-and-solve",
        answer,
        trajectory,
        llm_calls,
        duration_ms,
    ))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn build_plan_prompt(task: &str, detailed: bool) -> String {
    if detailed {
        format!(
            "Task: {}\n\n\
             Before solving, devise a detailed step-by-step plan. \
             For each step include:\n\
             1. What to do\n\
             2. Why it is necessary\n\
             3. Any pitfalls to watch for\n\n\
             Number each step. Write the plan only — do not solve it yet.",
            task
        )
    } else {
        format!(
            "Task: {}\n\n\
             Devise a clear step-by-step plan to solve this task. \
             Number each step. Write the plan only — do not execute it yet.",
            task
        )
    }
}

/// Extract numbered steps from a plan string.
/// Handles "1. ...", "1) ...", "Step 1: ..." formats.
pub fn extract_plan_steps(plan: &str) -> Vec<String> {
    let mut steps: Vec<String> = Vec::new();
    let mut current: Option<String> = None;

    for line in plan.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Detect a new step heading.
        if is_step_heading(trimmed) {
            if let Some(prev) = current.take() {
                steps.push(prev.trim().to_string());
            }
            current = Some(trimmed.to_string());
        } else if let Some(ref mut cur) = current {
            cur.push('\n');
            cur.push_str(trimmed);
        }
    }
    if let Some(last) = current {
        steps.push(last.trim().to_string());
    }

    // If no numbered steps were found, fall back to treating each non-empty
    // line as a step.
    if steps.is_empty() {
        plan.lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect()
    } else {
        steps
    }
}

fn is_step_heading(line: &str) -> bool {
    // "1." / "2." / "1)" / "Step 1:" / "Step 1."
    let numbered_re = regex::Regex::new(r"^\d+[.)]\s+").unwrap();
    let step_re = regex::Regex::new(r"^(?i)step\s+\d+[.):]\s*").unwrap();
    numbered_re.is_match(line) || step_re.is_match(line)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_plan_steps_numbered_dot() {
        let plan = "1. Identify the issue.\n2. Write a test.\n3. Fix the code.";
        let steps = extract_plan_steps(plan);
        assert_eq!(steps.len(), 3);
        assert!(steps[0].contains("Identify"));
        assert!(steps[2].contains("Fix"));
    }

    #[test]
    fn test_extract_plan_steps_step_prefix() {
        let plan = "Step 1: Read the file.\nStep 2: Parse JSON.\nStep 3: Output result.";
        let steps = extract_plan_steps(plan);
        assert_eq!(steps.len(), 3);
        assert!(steps[1].contains("Parse"));
    }

    #[test]
    fn test_extract_plan_steps_fallback_lines() {
        let plan = "Read the file.\nParse the data.\nPrint output.";
        let steps = extract_plan_steps(plan);
        assert_eq!(steps.len(), 3);
    }

    #[test]
    fn test_extract_plan_steps_empty() {
        let steps = extract_plan_steps("");
        assert!(steps.is_empty());
    }

    #[test]
    fn test_build_plan_prompt_standard() {
        let prompt = build_plan_prompt("Fix the bug", false);
        assert!(prompt.contains("Fix the bug"));
        assert!(prompt.contains("step-by-step plan"));
        assert!(!prompt.contains("pitfalls"));
    }

    #[test]
    fn test_build_plan_prompt_detailed() {
        let prompt = build_plan_prompt("Fix the bug", true);
        assert!(prompt.contains("Fix the bug"));
        assert!(prompt.contains("pitfalls"));
    }

    #[test]
    fn test_is_step_heading_numbered() {
        assert!(is_step_heading("1. Do something"));
        assert!(is_step_heading("2) Another step"));
        assert!(is_step_heading("Step 3: Execute"));
        assert!(!is_step_heading("Just a sentence."));
    }

    #[tokio::test]
    async fn test_run_plan_and_solve_basic() {
        use crate::providers::provider::{CompletionResult, CompleteOptions};
        use async_trait::async_trait;

        struct MockProvider;
        #[async_trait]
        impl LLMProvider for MockProvider {
            fn name(&self) -> &str { "mock" }
            fn model(&self) -> &str { "mock" }
            async fn complete(&self, msgs: &[Message], _: Option<&CompleteOptions>) -> Result<CompletionResult> {
                let last = msgs.last().map(|m| m.content.as_str()).unwrap_or("");
                let content = if last.contains("plan") || last.contains("Plan") {
                    "1. Compute 6*7.\n2. Report the result.".to_string()
                } else if last.contains("concise final answer") {
                    "The answer is 42.".to_string()
                } else {
                    "Executing step.".to_string()
                };
                Ok(CompletionResult { content, done: true })
            }
        }

        let result = run_plan_and_solve(Options {
            task: "What is 6*7?".into(),
            provider: Arc::new(MockProvider),
            registry: None,
            detailed_plan: Some(false),
            max_steps: Some(5),
        })
        .await
        .unwrap();

        assert!(result.success);
        assert!(result.answer.contains("42"));
        assert_eq!(result.technique, "plan-and-solve");
    }
}
