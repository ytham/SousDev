//! Multi-Agent Debate: N agents debate, a judge synthesises the final answer.
//!
//! # Algorithm
//! 1. Each of `num_agents` agents produces an initial position on the task.
//! 2. For `rounds` rounds:
//!    - Each agent is shown all other agents' latest positions and asked to
//!      revise (or defend) its own.
//! 3. Aggregation:
//!    - **"judge"** (default): a separate LLM call synthesises a final answer
//!      from all positions.
//!    - **"majority"**: pick the most common final answer among agents.

use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use crate::providers::provider::{CompleteOptions, LLMProvider, Message};
use crate::techniques::self_consistency::majority_vote;
use crate::types::technique::{RunResult, StepType, TrajectoryStep};

const DEFAULT_NUM_AGENTS: usize = 3;
const DEFAULT_ROUNDS: usize = 2;

/// Aggregation strategy for the final answer.
#[derive(Debug, Clone, PartialEq)]
pub enum Aggregation {
    Judge,
    Majority,
}

impl Aggregation {
    /// Parse an aggregation strategy from a string.
    pub fn parse(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "majority" => Self::Majority,
            _ => Self::Judge,
        }
    }
}

/// Options for [`run_multi_agent_debate`].
pub struct Options {
    /// The task or question.
    pub task: String,
    /// LLM provider (used for all agents and the judge).
    pub provider: Arc<dyn LLMProvider>,
    /// Number of debating agents.
    pub num_agents: Option<usize>,
    /// Number of debate rounds.
    pub rounds: Option<usize>,
    /// Aggregation strategy: "judge" or "majority".
    pub aggregation: Option<String>,
}

/// Run the Multi-Agent Debate.
pub async fn run_multi_agent_debate(opts: Options) -> Result<RunResult> {
    let start = Instant::now();
    let num_agents = opts.num_agents.unwrap_or(DEFAULT_NUM_AGENTS);
    let rounds = opts.rounds.unwrap_or(DEFAULT_ROUNDS);
    let aggregation = opts
        .aggregation
        .as_deref()
        .map(Aggregation::parse)
        .unwrap_or(Aggregation::Judge);

    let mut trajectory: Vec<TrajectoryStep> = Vec::new();
    let mut llm_calls: usize = 0;
    let mut step_index: usize = 0;

    // ── Initial positions ─────────────────────────────────────────────────────
    let mut positions: Vec<String> = Vec::with_capacity(num_agents);
    for agent_id in 0..num_agents {
        let init_prompt = build_initial_prompt(&opts.task, agent_id, num_agents);
        let msgs = vec![Message::user(init_prompt)];
        let completion = opts
            .provider
            .complete(&msgs, Some(&CompleteOptions { max_tokens: Some(512), ..Default::default() }))
            .await?;
        llm_calls += 1;

        let position = completion.content.trim().to_string();
        positions.push(position.clone());

        trajectory.push(TrajectoryStep {
            index: step_index,
            step_type: StepType::Thought,
            content: format!("Agent {} initial position:\n{}", agent_id + 1, position),
            timestamp: chrono::Utc::now().to_rfc3339(),
            metadata: {
                let mut m = HashMap::new();
                m.insert("agent".to_string(), serde_json::json!(agent_id + 1));
                m.insert("round".to_string(), serde_json::json!(0));
                m
            },
        });
        step_index += 1;
    }

    // ── Debate rounds ─────────────────────────────────────────────────────────
    for round in 0..rounds {
        let mut new_positions: Vec<String> = Vec::with_capacity(num_agents);

        for agent_id in 0..num_agents {
            let others: Vec<String> = positions
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != agent_id)
                .map(|(i, p)| format!("Agent {}: {}", i + 1, p))
                .collect();

            let debate_prompt = build_debate_prompt(
                &opts.task,
                agent_id,
                &positions[agent_id],
                &others,
                round + 1,
            );
            let msgs = vec![Message::user(debate_prompt)];
            let completion = opts
                .provider
                .complete(&msgs, Some(&CompleteOptions { max_tokens: Some(512), ..Default::default() }))
                .await?;
            llm_calls += 1;

            let revised = completion.content.trim().to_string();
            new_positions.push(revised.clone());

            trajectory.push(TrajectoryStep {
                index: step_index,
                step_type: StepType::Thought,
                content: format!(
                    "Agent {} (round {}):\n{}",
                    agent_id + 1,
                    round + 1,
                    revised
                ),
                timestamp: chrono::Utc::now().to_rfc3339(),
                metadata: {
                    let mut m = HashMap::new();
                    m.insert("agent".to_string(), serde_json::json!(agent_id + 1));
                    m.insert("round".to_string(), serde_json::json!(round + 1));
                    m
                },
            });
            step_index += 1;
        }

        positions = new_positions;
    }

    // ── Aggregation ───────────────────────────────────────────────────────────
    let final_answer = match aggregation {
        Aggregation::Judge => {
            let summary: String = positions
                .iter()
                .enumerate()
                .map(|(i, p)| format!("Agent {}: {}", i + 1, p))
                .collect::<Vec<_>>()
                .join("\n\n");

            let judge_prompt = format!(
                "The following agents debated the question: \"{}\"\n\n\
                 Final positions:\n{}\n\n\
                 You are the judge. Synthesise these positions into a single, \
                 well-reasoned final answer. Respond with only the answer.",
                opts.task, summary
            );
            let msgs = vec![Message::user(judge_prompt)];
            let completion = opts
                .provider
                .complete(&msgs, Some(&CompleteOptions { max_tokens: Some(1024), ..Default::default() }))
                .await?;
            llm_calls += 1;

            let answer = completion.content.trim().to_string();

            trajectory.push(TrajectoryStep {
                index: step_index,
                step_type: StepType::Observation,
                content: format!("Judge synthesis:\n{}", answer),
                timestamp: chrono::Utc::now().to_rfc3339(),
                metadata: {
                    let mut m = HashMap::new();
                    m.insert("role".to_string(), serde_json::json!("judge"));
                    m
                },
            });

            answer
        }
        Aggregation::Majority => {
            majority_vote(&positions)
                .unwrap_or_else(|| positions.last().cloned().unwrap_or_default())
        }
    };

    let duration_ms = start.elapsed().as_millis() as u64;
    Ok(RunResult::success(
        "multi-agent-debate",
        final_answer,
        trajectory,
        llm_calls,
        duration_ms,
    ))
}

// ---------------------------------------------------------------------------
// Prompt builders
// ---------------------------------------------------------------------------

fn build_initial_prompt(task: &str, agent_id: usize, total_agents: usize) -> String {
    format!(
        "You are Agent {} of {} in a multi-agent debate.\n\
         Task: {}\n\n\
         Provide your initial position and reasoning. \
         Be concise (2–4 sentences).",
        agent_id + 1,
        total_agents,
        task
    )
}

fn build_debate_prompt(
    task: &str,
    agent_id: usize,
    my_position: &str,
    others: &[String],
    round: usize,
) -> String {
    let others_text = if others.is_empty() {
        "(no other agents)".to_string()
    } else {
        others.join("\n")
    };

    format!(
        "You are Agent {} in a multi-agent debate (round {}).\n\
         Task: {}\n\n\
         Your current position:\n{}\n\n\
         Other agents' positions:\n{}\n\n\
         Consider their arguments. Update or defend your position. \
         Be concise (2–4 sentences).",
        agent_id + 1,
        round,
        task,
        my_position,
        others_text
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_aggregation_from_str() {
        assert_eq!(Aggregation::parse("judge"), Aggregation::Judge);
        assert_eq!(Aggregation::parse("majority"), Aggregation::Majority);
        assert_eq!(Aggregation::parse("MAJORITY"), Aggregation::Majority);
        assert_eq!(Aggregation::parse("unknown"), Aggregation::Judge);
    }

    #[test]
    fn test_build_initial_prompt_contains_fields() {
        let p = build_initial_prompt("What is 2+2?", 0, 3);
        assert!(p.contains("Agent 1 of 3"));
        assert!(p.contains("2+2"));
    }

    #[test]
    fn test_build_debate_prompt_contains_others() {
        let others = vec![
            "Agent 2: The answer is 4.".to_string(),
            "Agent 3: Agree, it's 4.".to_string(),
        ];
        let p = build_debate_prompt("2+2?", 0, "I say 4.", &others, 1);
        assert!(p.contains("round 1"));
        assert!(p.contains("Agent 2"));
        assert!(p.contains("Agent 3"));
        assert!(p.contains("I say 4."));
    }

    #[test]
    fn test_build_debate_prompt_no_others() {
        let p = build_debate_prompt("2+2?", 0, "4", &[], 1);
        assert!(p.contains("(no other agents)"));
    }

    #[tokio::test]
    async fn test_run_multi_agent_debate_judge() {
        use crate::providers::provider::{CompletionResult, CompleteOptions};
        use async_trait::async_trait;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountingProvider {
            call: Arc<AtomicUsize>,
        }

        #[async_trait]
        impl LLMProvider for CountingProvider {
            fn name(&self) -> &str { "mock" }
            fn model(&self) -> &str { "mock" }
            async fn complete(&self, msgs: &[Message], _: Option<&CompleteOptions>) -> Result<CompletionResult> {
                let n = self.call.fetch_add(1, Ordering::SeqCst);
                let last = msgs.last().map(|m| m.content.as_str()).unwrap_or("");
                let content = if last.contains("judge") || last.contains("Synthesise") {
                    "The consensus answer is 4.".to_string()
                } else {
                    format!("Position from call {}: The answer is 4.", n)
                };
                Ok(CompletionResult { content, done: true, content_blocks: None, stop_reason: None, usage: None })
            }
        }

        let result = run_multi_agent_debate(Options {
            task: "What is 2+2?".into(),
            provider: Arc::new(CountingProvider { call: Arc::new(AtomicUsize::new(0)) }),
            num_agents: Some(2),
            rounds: Some(1),
            aggregation: Some("judge".into()),
        })
        .await
        .unwrap();

        assert!(result.success);
        assert!(!result.answer.is_empty());
        assert_eq!(result.technique, "multi-agent-debate");
        // 2 initial + 2 round-1 + 1 judge = 5 calls
        assert_eq!(result.llm_calls, 5);
    }

    #[tokio::test]
    async fn test_run_multi_agent_debate_majority() {
        use crate::providers::provider::{CompletionResult, CompleteOptions};
        use async_trait::async_trait;

        struct FixedProvider;
        #[async_trait]
        impl LLMProvider for FixedProvider {
            fn name(&self) -> &str { "mock" }
            fn model(&self) -> &str { "mock" }
            async fn complete(&self, _msgs: &[Message], _: Option<&CompleteOptions>) -> Result<CompletionResult> {
                Ok(CompletionResult { content: "42".into(), done: true, content_blocks: None, stop_reason: None, usage: None })
            }
        }

        let result = run_multi_agent_debate(Options {
            task: "What is the answer?".into(),
            provider: Arc::new(FixedProvider),
            num_agents: Some(3),
            rounds: Some(1),
            aggregation: Some("majority".into()),
        })
        .await
        .unwrap();

        assert!(result.success);
        // All agents say "42" so majority vote wins.
        assert_eq!(result.answer, "42");
    }
}
